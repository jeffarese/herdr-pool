//! `test`: one test suite split across machines. Every machine gets the same
//! snapshot; each lane (a tree on a machine) runs shards from a shared queue,
//! so a fast machine takes more of them. What comes back is a line per shard
//! and the end of the output of the ones that failed.

use crate::args;
use crate::config;
use crate::error::{Error, Result};
use crate::git::Repo;
use crate::hc::{self, Env, Hc};
use crate::out;
use crate::pool::{self, Kind, World, BASE, JOB, KIND, RUN, TREE};
use crate::signal;
use crate::state::now_ms;
use crate::sync::{self, Lease};
use crate::ui;
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const HELP: &str = "herdr-pool test [options] [--] [command]

Runs the command on a snapshot of this repository (uncommitted changes
included), split into shards over the machines with room. vitest, jest,
playwright and cargo nextest are split for you; otherwise put {shard} and
{total} (1-based) in the command. Without a command: `test` from the config
or .herdr-pool.toml. Prints a line per shard and the end of each failure;
exits 1 if any failed.

  --shards N       how many shards (default: one per machine with room)
  --each           the whole command once on every machine (an OS matrix)
  --on M,N         only these machines
  --needs T        only machines tagged T
  --timeout S      end a shard after S seconds
  --tail N         lines shown from a failed shard (default 30)
  --clean          snapshot HEAD only
  --fresh          reused trees also lose ignored files (deps, caches)
  --no-setup       skip installing dependencies
  --json           JSON output";

pub fn test(argv: &[String]) -> Result<i32> {
    let flags = ["each", "clean", "fresh", "no-setup", "json"];
    let a = args::parse(argv, &flags, &["shards", "on", "needs", "timeout", "tail"], true)?;
    if a.flag("help") {
        println!("{HELP}");
        return Ok(0);
    }
    let repo = Repo::find(&std::env::current_dir()?).ok_or("run test inside a repository: shards run on a snapshot of it")?;
    signal::install(); // from here, Ctrl-C ends what this run started instead of orphaning it
    let cfg = config::load()?;
    let conf = cfg.repo(&repo.names, &repo.root);
    let command = match a.rest.len() {
        0 => conf.test.clone().ok_or("no command: herdr-pool test -- <command>, or set `test` in .herdr-pool.toml")?,
        1 => a.rest[0].clone(),
        _ => out::shell_join(&a.rest),
    };
    let mut hc = Hc::connect()?;
    let mut world = World::discover(&mut hc, &cfg)?;
    let on = a.list("on");
    for name in &on {
        world.find(name)?;
    }
    if !on.is_empty() {
        let me = world.me.name.clone();
        for m in &mut world.machines {
            m.conf.disabled = !(on.contains(&m.name) || (m.on.is_none() && on.contains(&me)));
        }
    }
    let needs = a.list("needs");
    world.probe(|m| m.eligible(&needs));
    let each = a.flag("each");
    let lanes: Vec<usize> = match each {
        true => (0..world.machines.len())
            .filter(|i| {
                let m = &world.machines[*i];
                m.eligible(&needs) && m.used(Kind::Job) < m.cap(Kind::Job, &cfg.defaults)
            })
            .collect(),
        false => {
            let machines = world.spread(Kind::Job, &needs, world.machines.len(), &cfg.defaults);
            let distinct = machines.iter().collect::<std::collections::HashSet<_>>().len();
            let total = a.num::<usize>("shards")?.unwrap_or(distinct).max(1);
            world.spread(Kind::Job, &needs, total, &cfg.defaults)
        }
    };
    if lanes.is_empty() {
        return Err(Error::new(world.pick(Kind::Job, &needs, &cfg.defaults).err().map(|e| e.msg).unwrap_or_default()));
    }
    let total = match each {
        true => lanes.len(),
        false => a.num::<usize>("shards")?.unwrap_or(lanes.len()).max(1),
    };
    let command = match each {
        true => command,
        false => split(&command, total)?,
    };

    let snap = repo.snapshot(a.flag("clean"))?;
    let run = format!("test-{}", base36(now_ms() % 60_466_176));
    repo.pin(&run, &snap.sha)?;
    let mut machines: Vec<usize> = lanes.clone();
    machines.sort_unstable();
    machines.dedup();
    let names: Vec<&str> = machines.iter().map(|i| world.machines[*i].name.as_str()).collect();
    let pretty = ui::pretty() && !a.flag("json");
    if pretty {
        let g = ui::g();
        let dirty = if snap.dirty { " with uncommitted changes" } else { "" };
        let on = match names.as_slice() {
            [one] => one.to_string(),
            [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
            [] => String::new(),
        };
        let what = if each { format!("on each of {on}") } else { format!("{total} shard{} on {on}", if total == 1 { "" } else { "s" }) };
        println!("{} {}  {} {}", ui::accent(g.dot), ui::bold(&run), what, ui::dim(format!("{} {}{dirty}", g.sep, out::short(&snap.sha))));
        println!("  {}", ui::dim(&command));
    } else if !a.flag("json") {
        let dirty = if snap.dirty { "+uncommitted" } else { "" };
        println!("test {run}: {total} shard(s) on {} · {}{dirty} · {command}", names.join(", "), out::short(&snap.sha));
    }
    let sending = ui::spin(format!("Sending the snapshot to {}", names.join(", ")));

    // Every machine gets the snapshot once, all at the same time. Nothing has
    // started yet, so Ctrl-C meanwhile just leaves.
    let shipped: Vec<(usize, Result<Env>)> = std::thread::scope(|s| {
        let handles: Vec<_> = machines
            .iter()
            .map(|&i| {
                let (m, repo, run, sha) = (&world.machines[i], &repo, &run, &snap.sha);
                (i, s.spawn(move || -> Result<Env> {
                    let mut hc = Hc::connect()?;
                    let env = pool::env_for(&mut hc, m);
                    sync::ship(&mut hc, m, repo, run, sha, &env)?;
                    Ok(env)
                }))
            })
            .collect();
        while !handles.iter().all(|(_, h)| h.is_finished()) {
            if signal::stopped() {
                repo.unpin(&run);
                stopped(&run);
                std::process::exit(130);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        handles.into_iter().map(|(i, h)| (i, h.join().unwrap_or_else(|_| Err(Error::new("crashed"))))).collect()
    });
    sending.clear();
    let mut envs: BTreeMap<usize, Env> = BTreeMap::new();
    let mut problems: Vec<String> = Vec::new();
    for (i, r) in shipped {
        match r {
            Ok(env) => {
                envs.insert(i, env);
            }
            Err(e) => problems.push(format!("{}: {}", world.machines[i].name, e.msg)),
        }
    }

    let queue: Mutex<VecDeque<usize>> = Mutex::new((1..=total).collect());
    let results: Mutex<Vec<Shard>> = Mutex::new(Vec::new());
    let started: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let held: Mutex<Vec<(usize, u32, String)>> = Mutex::new(Vec::new());
    let lane_errors: Mutex<Vec<String>> = Mutex::new(problems);
    let running = AtomicUsize::new(0);
    let shard_timeout = a.num::<f64>("timeout")?;
    let tail = a.num::<usize>("tail")?.unwrap_or(cfg.defaults.tail);
    let view = Arc::new(Mutex::new(View { shards: vec![Line::Waiting; total], setups: Vec::new() }));
    let lane = Lane {
        world: &world,
        repo: &repo,
        sha: &snap.sha,
        run: &run,
        command: &command,
        total,
        each,
        fresh: a.flag("fresh"),
        setup: if a.flag("no-setup") { None } else { setup_for(&cfg, &repo, &snap.sha) },
        timeout: shard_timeout,
        tail,
        queue: &queue,
        results: &results,
        started: &started,
        held: &held,
        view: &view,
    };
    let began = Instant::now();
    let mut board = {
        let view = view.clone();
        ui::board(move |frame| view.lock().map(|v| v.render(frame, began)).unwrap_or_default())
    };
    std::thread::scope(|s| {
        for (n, &i) in lanes.iter().enumerate() {
            let Some(env) = envs.get(&i).cloned() else { continue };
            running.fetch_add(1, Ordering::SeqCst);
            let (lane, running, lane_errors) = (&lane, &running, &lane_errors);
            s.spawn(move || {
                if let Err(e) = lane.work(i, n + 1, &env) {
                    lane_errors.lock().unwrap().push(format!("{}: {}", lane.world.machines[i].name, e.msg));
                }
                running.fetch_sub(1, Ordering::SeqCst);
            });
        }
        // Ctrl-C: end every shard that runs or starts, and let the lanes give
        // their trees back on their way out. A lane can only notice between
        // shards, so a second Ctrl-C, or ten seconds, means leave now.
        let (mut ended, mut stopping) = (0, None::<Instant>);
        while running.load(Ordering::SeqCst) > 0 {
            if signal::stopped() {
                let since = *stopping.get_or_insert_with(|| {
                    board.close();
                    match ui::live() {
                        true => ui::step(ui::Tone::Warn, &format!("Stopping {run}  {}", ui::dim("Ctrl-C again to leave at once")), &[]),
                        false => eprintln!("herdr-pool: stopping {run} (Ctrl-C again to leave at once)"),
                    }
                    Instant::now()
                });
                let now_started = started.lock().unwrap().clone();
                if let Ok(mut hc) = Hc::connect() {
                    for address in &now_started[ended..] {
                        let _ = hc.call("terminal.kill", json!({"terminal": address}));
                    }
                }
                ended = now_started.len();
                if signal::times() > 1 || since.elapsed() > Duration::from_secs(10) {
                    leave(&world, &started, &held, &repo, &run);
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    board.close();
    repo.unpin(&run);
    for &i in &machines {
        if let Some(env) = envs.get(&i) {
            // The run's ref in that mirror (its trees went back already).
            let _ = sync::drop_tree(&mut hc, &world.machines[i], &repo.key, 0, &run, false, env);
        }
    }
    if signal::stopped() {
        for address in started.lock().unwrap().iter() {
            let _ = hc.call("terminal.rm", json!({"terminal": address}));
        }
        stopped(&run);
        return Ok(130);
    }

    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|r| r.shard);
    let not_run: Vec<usize> = queue.into_inner().unwrap().into_iter().collect();
    let errors = lane_errors.into_inner().unwrap();
    let failed = results.iter().filter(|r| !r.passed).count() + not_run.len();
    if a.flag("json") {
        let shards: Vec<Value> = results.iter().map(Shard::json).collect();
        println!(
            "{}",
            json!({"run": run, "passed": failed == 0, "total": total, "shards": shards, "not_run": not_run, "errors": errors, "base": snap.sha})
        );
        return Ok(if failed == 0 { 0 } else { 1 });
    }
    let took = out::span(began.elapsed().as_millis() as u64);
    if pretty {
        report(&results, &errors, &not_run, total, tail, &took, &run);
        return Ok(if failed == 0 { 0 } else { 1 });
    }
    let rows: Vec<Vec<String>> = results.iter().map(|r| r.row(total)).collect();
    print!("{}", out::table(&rows));
    for e in &errors {
        println!("ERROR {e}");
    }
    for r in results.iter().filter(|r| !r.passed && !r.tail.is_empty()) {
        println!("--- {}/{total} on {} (last {tail} lines)\n{}", r.shard, r.machine, r.tail);
    }
    match failed {
        0 => println!("passed: {total} of {total} shard(s) in {took}"),
        _ if !not_run.is_empty() => {
            let list: Vec<String> = not_run.iter().map(|s| s.to_string()).collect();
            println!("FAILED: {} of {total} shard(s) in {took}; never ran: {}", failed, list.join(", "));
        }
        _ => println!("FAILED: {failed} of {total} shard(s) in {took}; failed shards stay for hc read until herdr-pool gc"),
    }
    Ok(if failed == 0 { 0 } else { 1 })
}

struct Lane<'a> {
    world: &'a World,
    repo: &'a Repo,
    sha: &'a str,
    run: &'a str,
    command: &'a str,
    total: usize,
    /// Lane n runs shard n and nothing else: the whole suite once per machine.
    each: bool,
    fresh: bool,
    setup: Option<(String, Vec<String>)>,
    timeout: Option<f64>,
    tail: usize,
    queue: &'a Mutex<VecDeque<usize>>,
    results: &'a Mutex<Vec<Shard>>,
    started: &'a Mutex<Vec<String>>,
    held: &'a Mutex<Vec<(usize, u32, String)>>,
    view: &'a Arc<Mutex<View>>,
}

/// What the live board shows: each shard, and lanes still installing.
struct View {
    shards: Vec<Line>,
    setups: Vec<(String, String)>,
}

#[derive(Clone)]
enum Line {
    Waiting,
    Running(String, Instant),
    Done { passed: bool, machine: String, ms: u64, what: String },
}

impl View {
    fn render(&self, frame: &str, began: Instant) -> Vec<String> {
        let g = ui::g();
        let total = self.shards.len();
        let w = total.to_string().len();
        let name_w = self
            .shards
            .iter()
            .map(|l| match l {
                Line::Running(m, _) | Line::Done { machine: m, .. } => ui::width(m),
                Line::Waiting => 0,
            })
            .max()
            .unwrap_or(0);
        let mut lines: Vec<String> = self
            .setups
            .iter()
            .map(|(m, cmd)| format!("  {} installing dependencies on {m}  {}", ui::accent(frame), ui::dim(cmd)))
            .collect();
        for (i, l) in self.shards.iter().enumerate() {
            let n = format!("{:>w$}/{total}", i + 1);
            lines.push(match l {
                Line::Waiting => ui::dim(format!("  {} {n}  {}  waiting", g.sep, " ".repeat(name_w))),
                Line::Running(m, since) => {
                    format!("  {} {n}  {}  {}", ui::accent(frame), ui::pad(m, name_w), ui::dim(ui::secs(since.elapsed().as_millis() as u64)))
                }
                Line::Done { passed: true, machine, ms, .. } => {
                    format!("  {} {n}  {}  {}", ui::good(g.ok), ui::pad(machine, name_w), ui::dim(ui::secs(*ms)))
                }
                Line::Done { passed: false, machine, ms, what } => {
                    format!("  {} {n}  {}  {}  {}", ui::bad(g.bad), ui::pad(machine, name_w), ui::dim(ui::secs(*ms)), ui::bad(what))
                }
            });
        }
        let done = self.shards.iter().filter(|l| matches!(l, Line::Done { .. })).count();
        let failed = self.shards.iter().filter(|l| matches!(l, Line::Done { passed: false, .. })).count();
        let width = ui::columns().saturating_sub(30).clamp(10, 40);
        let tone = if failed > 0 { ui::Tone::Bad } else { ui::Tone::Good };
        let mut foot = format!("{done} of {total} done");
        if failed > 0 {
            foot += &format!(", {failed} failed");
        }
        lines.push(format!(
            "  {}  {}",
            ui::meter(done as f64 / total.max(1) as f64, width, tone),
            ui::dim(format!("{foot} {} {}", g.sep, ui::secs(began.elapsed().as_millis() as u64)))
        ));
        lines
    }
}

/// The end of a run, for a person: a line per shard, the failures' last
/// lines, and the verdict.
fn report(results: &[Shard], errors: &[String], not_run: &[usize], total: usize, tail: usize, took: &str, run: &str) {
    let g = ui::g();
    let w = total.to_string().len();
    let name_w = results.iter().map(|r| ui::width(&r.machine)).max().unwrap_or(0);
    println!();
    for r in results {
        let n = format!("{:>w$}/{total}", r.shard);
        match r.passed {
            true => println!("  {} {n}  {}  {}", ui::good(g.ok), ui::pad(&r.machine, name_w), ui::dim(ui::secs(r.ms))),
            false => println!(
                "  {} {n}  {}  {}  {}  {}",
                ui::bad(g.bad),
                ui::pad(&r.machine, name_w),
                ui::dim(ui::secs(r.ms)),
                ui::bad(&r.what),
                ui::dim(format!("hc read {}", r.address))
            ),
        }
    }
    for s in not_run {
        println!("  {} {:>w$}/{total}  {}", ui::warn(g.warn), s, ui::warn("never ran"));
    }
    for e in errors {
        println!("  {} {}", ui::bad(g.bad), ui::bad(e));
    }
    for r in results.iter().filter(|r| !r.passed && !r.tail.is_empty()) {
        println!();
        let shown = match r.tail.lines().count() < tail {
            true => "its output".to_string(),
            false => format!("its last {tail} lines"),
        };
        println!("  {} {}/{total} on {}  {}", ui::bad(g.bad), r.shard, ui::bold(&r.machine), ui::dim(shown));
        for l in r.tail.lines() {
            println!("  {} {}", ui::dim(g.pipe), ui::fit(&l.replace('\t', "  "), ui::columns().saturating_sub(5)));
        }
    }
    println!();
    let failed = results.iter().filter(|r| !r.passed).count() + not_run.len();
    match failed {
        0 => println!("{} {}  {}", ui::good(g.ok), ui::good(format!("{total} of {total} passed")), ui::dim(format!("in {took}"))),
        n => {
            println!("{} {}  {}", ui::bad(g.bad), ui::bad(format!("{n} of {total} failed")), ui::dim(format!("in {took}")));
            println!("  {} {}", ui::accent(g.arrow), ui::dim(format!("failed shards stay for hc read until herdr-pool gc (or rm {run})")));
        }
    }
}

struct Shard {
    shard: usize,
    machine: String,
    address: String,
    passed: bool,
    what: String,
    ms: u64,
    tail: String,
}

impl Lane<'_> {
    /// One tree on machine `i`: prepared once, then shards until none are left.
    fn work(&self, i: usize, n: usize, env: &Env) -> Result<()> {
        let m = &self.world.machines[i];
        let mut hc = Hc::connect()?;
        let job = format!("{}-lane{n}", self.run);
        let lease = Lease {
            job: job.clone(),
            kind: "shard".into(),
            terminal: None,
            base: self.sha.into(),
            since: now_ms(),
            by: self.world.me.id.clone(),
            pulled: None,
        };
        let key = &self.repo.key;
        if signal::stopped() {
            return Ok(());
        }
        let slot = sync::take(&mut hc, m, key, &lease)?;
        self.held.lock().unwrap().push((i, slot, job.clone()));
        let worked = self.shards(&mut hc, i, n, slot, &job, env);
        let _ = sync::drop_tree(&mut hc, m, key, slot, &job, false, env);
        let _ = sync::release(&mut hc, m, key, slot, &job);
        worked
    }

    fn shards(&self, hc: &mut Hc, i: usize, n: usize, slot: u32, job: &str, env: &Env) -> Result<()> {
        let m = &self.world.machines[i];
        let key = &self.repo.key;
        let dir = sync::checkout(hc, m, key, slot, self.sha, None, self.fresh, job, env)?;
        if let Some((cmd, files)) = &self.setup {
            if let Ok(mut v) = self.view.lock() {
                v.setups.push((m.name.clone(), cmd.clone()));
            }
            let set = sync::setup(hc, m, key, slot, cmd, files, env);
            if let Ok(mut v) = self.view.lock() {
                if let Some(at) = v.setups.iter().position(|(name, _)| *name == m.name) {
                    v.setups.remove(at);
                }
            }
            set?;
        }
        loop {
            if signal::stopped() {
                return Ok(());
            }
            let next = {
                let mut queue = self.queue.lock().unwrap();
                match self.each {
                    true => queue.iter().position(|s| *s == n).and_then(|at| queue.remove(at)),
                    false => queue.pop_front(),
                }
            };
            let Some(shard) = next else { return Ok(()) };
            let line = self
                .command
                .replace("{shard}", &shard.to_string())
                .replace("{total}", &self.total.to_string())
                .replace("{index}", &(shard - 1).to_string());
            let mut p = json!({
                "line": line, "cwd": dir, "name": format!("{}-{shard}", self.run), "env": env, "no_notify": true,
                "labels": {JOB: self.run, KIND: "shard", RUN: self.run, TREE: format!("{key}/{slot}"), BASE: self.sha},
            });
            if let Some(on) = &m.on {
                p["on"] = on.as_str().into();
            }
            if let Some(caller) = hc::caller() {
                p["caller"] = caller.into();
            }
            let r = match hc.call("terminal.run", p) {
                Ok(r) => r,
                Err(e) => {
                    self.queue.lock().unwrap().push_back(shard); // another lane may manage
                    return Err(e.into());
                }
            };
            // hc answers with our name for it (`studio/t7`); the lease keeps
            // that machine's own (`t7`), like a job's.
            let address = r["address"].as_str().unwrap_or_default().to_string();
            let (_, there) = pool::split_address(&address);
            let address = pool::address_on(m, there);
            self.started.lock().unwrap().push(address.clone());
            if let Ok(mut v) = self.view.lock() {
                v.shards[shard - 1] = Line::Running(m.name.clone(), Instant::now());
            }
            let _ = sync::update(hc, m, key, slot, job, |l| l.terminal = Some(there.to_string()));
            let shard = self.watch(hc, shard, &m.name, &address)?;
            if let Ok(mut v) = self.view.lock() {
                let line = Line::Done { passed: shard.passed, machine: shard.machine.clone(), ms: shard.ms, what: shard.what.clone() };
                v.shards[shard.shard - 1] = line;
            }
            // Until the next shard starts the lane is between shards, not gone.
            let _ = sync::update(hc, m, key, slot, job, |l| {
                l.terminal = None;
                l.since = now_ms();
            });
            if shard.passed {
                let _ = hc.call("terminal.rm", json!({"terminal": address}));
            }
            self.results.lock().unwrap().push(shard);
        }
    }

    /// Waits for the shard to exit (ending it at its timeout) and reads how it went.
    fn watch(&self, hc: &mut Hc, shard: usize, machine: &str, address: &str) -> Result<Shard> {
        let deadline = self.timeout.map(|s| Instant::now() + Duration::from_secs_f64(s));
        let mut timed_out = false;
        loop {
            let left = deadline.map(|d| d.saturating_duration_since(Instant::now()).as_secs_f64());
            let r = hc.call("terminal.wait", json!({"terminal": address, "timeout": left.unwrap_or(600.0).clamp(1.0, 600.0)}))?;
            if r["reason"] == "exit" {
                break;
            }
            if r["reason"] != "timeout" {
                // It said it is idle or done (OSC 7501) but may run on: only
                // its exit ends a shard, and hc's wait no longer waits for it.
                if !pool::is_live(&hc.call("terminal.get", json!({"terminal": address}))?) {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                let _ = hc.call("terminal.kill", json!({"terminal": address}));
                timed_out = true;
                break;
            }
        }
        let t = hc.call("terminal.get", json!({"terminal": address}))?;
        let code = t["exit_code"].as_i64();
        let passed = code == Some(0) && !timed_out;
        let ms = t["finished_ms"].as_u64().unwrap_or_else(now_ms).saturating_sub(t["created_ms"].as_u64().unwrap_or(0));
        let what = match (timed_out, code, t["signal"].as_i64()) {
            (true, _, _) => "timed out".to_string(),
            (_, Some(c), _) => format!("exit {c}"),
            (_, None, Some(s)) => format!("signal {s}"),
            _ => t["reason"].as_str().unwrap_or("ended").to_string(),
        };
        let tail = match passed {
            true => String::new(),
            false => hc
                .call("terminal.read", json!({"terminal": address, "lines": self.tail + 20}))
                .map(|r| out::tail(r["text"].as_str().unwrap_or_default(), self.tail))
                .unwrap_or_default(),
        };
        Ok(Shard { shard, machine: machine.into(), address: address.into(), passed, what, ms, tail })
    }
}

impl Shard {
    fn row(&self, total: usize) -> Vec<String> {
        let mut row = vec![
            if self.passed { "ok" } else { "FAIL" }.to_string(),
            format!("{}/{total}", self.shard),
            self.machine.clone(),
            out::span(self.ms),
        ];
        if !self.passed {
            row.push(self.what.clone());
            row.push(format!("hc read {}", self.address));
        }
        row
    }

    fn json(&self) -> Value {
        json!({
            "shard": self.shard, "machine": self.machine, "address": self.address, "passed": self.passed,
            "result": self.what, "ms": self.ms, "tail": self.tail,
        })
    }
}

/// Leaving at once (a second Ctrl-C, or lanes stuck in setup): end every
/// shard, give the trees back, and go.
fn leave(world: &World, started: &Mutex<Vec<String>>, held: &Mutex<Vec<(usize, u32, String)>>, repo: &Repo, run: &str) -> ! {
    if let Ok(mut hc) = Hc::connect() {
        for address in started.lock().unwrap().iter() {
            let _ = hc.call("terminal.kill", json!({"terminal": address}));
            let _ = hc.call("terminal.rm", json!({"terminal": address}));
        }
        for (i, slot, job) in held.lock().unwrap().iter() {
            let _ = sync::release(&mut hc, &world.machines[*i], &repo.key, *slot, job);
        }
    }
    repo.unpin(run);
    stopped(run);
    std::process::exit(130);
}

/// The last word after a Ctrl-C. Spinners and boards stop first, and the
/// line a second Ctrl-C echoed (`^C`) is cleared.
fn stopped(run: &str) {
    ui::restore();
    match ui::live() {
        true => ui::step(ui::Tone::Warn, &format!("Stopped {run}"), &[]),
        false => eprintln!("herdr-pool: stopped {run}"),
    }
}

/// Adds the runner's own shard flag when the command lacks `{shard}`.
pub fn split(command: &str, total: usize) -> Result<String> {
    if total == 1 || command.contains("{shard}") {
        return Ok(command.into());
    }
    let words: Vec<&str> = command.split_whitespace().collect();
    let has = |w: &str| words.iter().any(|x| *x == w || x.ends_with(&format!("/{w}")));
    if has("vitest") || has("jest") || has("playwright") {
        return Ok(format!("{command} --shard={{shard}}/{{total}}"));
    }
    if has("nextest") {
        return Ok(format!("{command} --partition count:{{shard}}/{{total}}"));
    }
    Err(Error::usage(format!(
        "can't tell how to split `{command}`: put {{shard}} and {{total}} in it (e.g. `npm test -- --shard={{shard}}/{{total}}`), or pass --shards 1"
    )))
}

fn setup_for(cfg: &config::Config, repo: &Repo, sha: &str) -> Option<(String, Vec<String>)> {
    let conf = cfg.repo(&repo.names, &repo.root);
    let files: Vec<String> = crate::git::git(&repo.root, &["ls-tree", "--name-only", sha])
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect();
    match conf.setup {
        Some(cmd) if cmd.trim().is_empty() => None,
        Some(cmd) => Some((cmd, conf.setup_when.unwrap_or_else(|| sync::default_setup(&files).map(|d| d.1).unwrap_or_default()))),
        None => sync::default_setup(&files),
    }
}

fn base36(mut n: u64) -> String {
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut s = Vec::new();
    loop {
        s.push(digits[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    s.reverse();
    String::from_utf8(s).expect("ascii")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_runners_split_themselves() {
        assert_eq!(split("npx vitest run", 3).unwrap(), "npx vitest run --shard={shard}/{total}");
        assert_eq!(split("./node_modules/.bin/jest --ci", 2).unwrap(), "./node_modules/.bin/jest --ci --shard={shard}/{total}");
        assert_eq!(split("cargo nextest run", 2).unwrap(), "cargo nextest run --partition count:{shard}/{total}");
        assert_eq!(split("make test", 1).unwrap(), "make test");
        assert_eq!(split("pytest --splits {total} --group {shard}", 4).unwrap(), "pytest --splits {total} --group {shard}");
        assert!(split("npm test", 2).is_err());
    }

    #[test]
    fn run_ids_are_short() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert!(base36(60_466_175).len() <= 5);
    }
}
