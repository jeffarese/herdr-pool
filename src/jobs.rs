//! Jobs: an agent or a command placed on a machine, usually in a work tree
//! holding a snapshot of your repository. `run` starts them; `ls`, `wait`,
//! `diff`, `cat` look at them; `pull` brings their work back; `rm` and `gc`
//! clean up.

use crate::args;
use crate::config::{self, Config};
use crate::error::{Error, Result};
use crate::git::{Repo, Snap};
use crate::hc::{self, Hc};
use crate::out;
use crate::pool::{self, Kind, World, BASE, JOB, KIND, PROBE_LABEL, TREE};
use crate::state::now_ms;
use crate::sync::{self, Lease, Outcome, Setup, Then};
use crate::ui::{self, Tone};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

pub const RUN_HELP: &str = "herdr-pool run [options] <program|template> [task...]

Starts an agent or command on the machine with the most room. Inside a git
repository it runs in a work tree there holding a snapshot of yours
(uncommitted and untracked files included); `pull` brings the work back.
Agents get a footer on their task saying where they are and how to report.
Claude Code is told to trust the repository's pool trees on that machine
first, so it doesn't stop to ask (trust = false in the config: it asks).

  --name N         job name (default: from the task)
  --on M           this machine (`local` is this one)
  --needs T        only machines tagged T (repeatable)
  --count N        N copies (name-1..N), each placed on its own
  --here           no work tree: run in this folder
  --cwd DIR        no work tree: run in DIR on that machine
  --clean          snapshot HEAD only, not uncommitted changes
  --fresh          a reused tree also loses ignored files (deps, caches)
  --no-setup       skip installing dependencies in the tree
  --raw            don't add the footer to the task
  --keep           restart a lost program as a shell (hc --keep)
  --outlive        keep running when the terminal that started it ends
  --wait-slot[=S]  wait up to S seconds (default 600) for room
  --json           JSON output";

pub fn run(argv: &[String]) -> Result<i32> {
    let flags = ["here", "clean", "fresh", "no-setup", "raw", "keep", "outlive", "wait-slot", "json"];
    let a = args::parse(argv, &flags, &["name", "on", "needs", "count", "cwd"], true)?;
    if a.flag("help") || a.rest.is_empty() {
        println!("{RUN_HELP}");
        return Ok(if a.flag("help") { 0 } else { 2 });
    }
    let cfg = config::load()?;
    let mut hc = Hc::connect()?;
    let mut world = World::discover(&mut hc, &cfg)?;
    // Each template's name and the command it runs.
    let templates: Vec<(String, String)> = hc.call("templates.list", json!({}))?["templates"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| Some((t["name"].as_str()?.to_string(), t["command"].as_str().unwrap_or_default().to_string())))
        .collect();
    let mut program = a.rest.clone();
    let base = basename(&program[0]);
    let runs = templates.iter().find(|(name, _)| *name == program[0]).map(|(_, command)| command.as_str());
    let template = runs.is_some();
    // Claude Code, which asks before working in a folder it hasn't seen.
    let claude = basename(runs.and_then(program_of).unwrap_or(&program[0])) == "claude";
    let kind = match template || cfg.defaults.agents.iter().any(|g| *g == base) {
        true => Kind::Agent,
        false => Kind::Job,
    };
    // Codex's commands otherwise run in its daemon, outside the terminal hc
    // watches. (A template's words are its task, not codex's flags.)
    if base == "codex" && !template && !program.iter().any(|w| w == "--no-daemon") {
        program.insert(1, "--no-daemon".into());
    }
    let on = match (a.flag("here"), a.value("on")) {
        (true, Some(m)) if world.find(m)? != 0 => {
            return Err(Error::usage("--here runs in this folder, on this machine; elsewhere use --cwd DIR with --on"))
        }
        (true, _) => Some(pool::LOCAL),
        (false, on) => on,
    };
    // A machine that isn't there fails before any work is done.
    if let Some(on) = on {
        world.find(on)?;
    }
    let here = std::env::current_dir()?;
    let repo = match a.flag("here") || a.value("cwd").is_some() {
        true => None,
        false => Repo::find(&here),
    };
    let snap = match &repo {
        Some(r) => {
            let s = ui::spin("Taking a snapshot of your work tree");
            let snap = r.snapshot(a.flag("clean"))?;
            let with = if snap.dirty { ui::dim(" with your uncommitted changes") } else { String::new() };
            s.ok(format!("Snapshot {}{with}", out::short(&snap.sha)));
            Some(snap)
        }
        None => None,
    };
    let count = a.num::<u32>("count")?.unwrap_or(1).max(1);
    let needs = a.list("needs");
    let mut taken = taken_names(&mut hc, &world);
    let stem = git_safe(a.value("name").map(String::from).unwrap_or_else(|| job_name(&program)));
    let wait_slot = match a.flag("wait-slot") {
        true => Some(a.num::<f64>("wait-slot")?.unwrap_or(600.0)),
        false => None,
    };
    if on.is_none() {
        let s = ui::spin("Looking for room");
        world.probe(|m| m.eligible(&needs));
        s.clear();
    }
    let mut started = Vec::new();
    for n in 1..=count {
        let wanted = if count == 1 { stem.clone() } else { format!("{stem}-{n}") };
        let job = unique(&wanted, &taken);
        taken.insert(job.clone());
        let i = place(&mut hc, &mut world, &cfg, kind, &needs, on, wait_slot)?;
        let launch = Launch {
            job: &job,
            kind,
            claude,
            program: &program,
            repo: repo.as_ref(),
            snap: snap.as_ref(),
            cwd: a.value("cwd").map(String::from).or_else(|| a.flag("here").then(|| here.to_string_lossy().into_owned())),
            fresh: a.flag("fresh"),
            setup: !a.flag("no-setup"),
            footer: !a.flag("raw"),
            keep: a.flag("keep"),
            outlive: a.flag("outlive"),
        };
        let done = launch.start(&mut hc, &world, i, &cfg)?;
        match kind {
            Kind::Agent => world.machines[i].agents += 1,
            Kind::Job => world.machines[i].jobs += 1,
        }
        match (a.flag("json"), ui::pretty()) {
            (true, _) => {}
            (false, true) => done.card(kind),
            (false, false) => println!("{}", done.line()),
        }
        started.push(done);
    }
    if a.flag("json") {
        println!("{}", Value::Array(started.iter().map(Started::json).collect()));
    }
    Ok(0)
}

/// Where the next one goes: `--on`, else the best machine with room, waiting
/// for room if asked to.
fn place(
    hc: &mut Hc,
    world: &mut World,
    cfg: &Config,
    kind: Kind,
    needs: &[String],
    on: Option<&str>,
    wait_slot: Option<f64>,
) -> Result<usize> {
    if let Some(on) = on {
        let i = world.find(on)?;
        let m = &world.machines[i];
        if !m.reachable() {
            fail!("{} is {}{}", m.name, m.state, m.why.as_ref().map(|w| format!(": {w}")).unwrap_or_default());
        }
        return Ok(i);
    }
    let deadline = wait_slot.map(|s| Instant::now() + Duration::from_secs_f64(s));
    let mut waiting: Option<ui::Spin> = None;
    loop {
        match world.pick(kind, needs, &cfg.defaults) {
            Ok(i) => {
                if let Some(s) = waiting.take() {
                    s.ok(format!("Room on {}", world.machines[i].name));
                }
                return Ok(i);
            }
            Err(e) if deadline.is_none_or(|d| Instant::now() >= d) => return Err(e),
            Err(e) => {
                if waiting.is_none() {
                    waiting = Some(ui::spin(format!("Waiting for room: {e}")));
                }
                std::thread::sleep(Duration::from_secs(10));
                *world = World::discover(hc, cfg)?;
                world.probe(|m| m.eligible(needs));
            }
        }
    }
}

struct Launch<'a> {
    job: &'a str,
    kind: Kind,
    /// It runs Claude Code: its tree gets trusted first.
    claude: bool,
    program: &'a [String],
    repo: Option<&'a Repo>,
    snap: Option<&'a Snap>,
    cwd: Option<String>,
    fresh: bool,
    setup: bool,
    footer: bool,
    keep: bool,
    outlive: bool,
}

struct Started {
    job: String,
    address: String,
    machine: String,
    dir: Option<String>,
    base: Option<String>,
    dirty: bool,
    reply: Option<String>,
    first_message: Value,
}

impl Launch<'_> {
    fn start(&self, hc: &mut Hc, world: &World, i: usize, cfg: &Config) -> Result<Started> {
        let m = &world.machines[i];
        let env = pool::env_for(hc, m);
        let reply = hc::caller().and_then(|c| pool::reply_address(hc, world, m, &c));
        let mut labels = json!({JOB: self.job, KIND: self.kind.label()});
        let mut tree = None;
        let mut cwd = self.cwd.clone();
        if let (Some(repo), Some(snap)) = (self.repo, self.snap) {
            repo.pin(self.job, &snap.sha)?;
            let lease = Lease {
                job: self.job.into(),
                kind: self.kind.label().into(),
                terminal: None,
                base: snap.sha.clone(),
                since: now_ms(),
                by: world.me.id.clone(),
                pulled: None,
            };
            let s = ui::spin(format!("Sending the snapshot to {}", m.name));
            let prepared = sync::ship(hc, m, repo, self.job, &snap.sha, &env).and_then(|sent| {
                match (m.on.is_some(), sent) {
                    (false, _) => s.clear(),
                    (true, 0) => s.ok(format!("{} has the snapshot already", m.name)),
                    (true, n) => s.ok(format!("Sent the snapshot to {}  {}", m.name, ui::dim(bytes(n)))),
                }
                let s = ui::spin(format!("Getting a work tree on {}", m.name));
                let slot = sync::take(hc, m, &repo.key, &lease)?;
                match self.prepare(hc, m, repo, snap, slot, cfg, &env, s) {
                    Ok(dir) => Ok((slot, dir)),
                    Err(e) => {
                        let _ = sync::release(hc, m, &repo.key, slot, self.job);
                        Err(e)
                    }
                }
            });
            let (slot, dir) = prepared.inspect_err(|_| repo.unpin(self.job))?;
            if self.claude && cfg.defaults.trust {
                match crate::trust::ensure(hc, m, &repo.key, &env) {
                    // A person's terminal only: stdout is the job's line (or JSON).
                    Ok(crate::trust::Trust::Added) if ui::live() => {
                        ui::done(&format!("Claude Code on {} trusts this repository's trees now", m.name))
                    }
                    Ok(_) => {}
                    Err(e) if ui::live() => ui::step(Tone::Warn, &format!("Couldn't tell Claude Code on {} to trust the tree: {e}", m.name), &[]),
                    Err(_) => {}
                }
            }
            labels[TREE] = format!("{}/{slot}", repo.key).into();
            labels[BASE] = snap.sha.clone().into();
            cwd = Some(dir.clone());
            tree = Some((repo.key.clone(), slot, dir));
        }
        let mut argv = self.program.to_vec();
        if let Some(i) = task_at(&argv).filter(|_| self.kind == Kind::Agent && self.footer) {
            argv[i] += &footer(self.job, &m.name, self.snap.filter(|_| tree.is_some()), reply.as_deref());
        }
        let mut params = json!({
            "argv": argv, "name": self.job, "labels": labels, "env": env,
            "keep": self.keep, "outlive_parent": self.outlive,
        });
        if let Some(on) = &m.on {
            params["on"] = on.as_str().into();
        }
        if let Some(cwd) = &cwd {
            params["cwd"] = cwd.as_str().into();
        }
        if let Some(caller) = hc::caller() {
            params["caller"] = caller.into();
        }
        let s = ui::spin(format!("Starting {} on {}", self.job, m.name));
        let r = match hc.call("terminal.run", params) {
            Ok(r) => {
                s.clear();
                r
            }
            Err(e) => {
                // Without a terminal the error says it all, once.
                match ui::live() {
                    true => s.fail(format!("{} didn't start on {}", self.job, m.name)),
                    false => s.clear(),
                }
                if let (Some(repo), Some((key, slot, _))) = (self.repo, &tree) {
                    let _ = sync::release(hc, m, key, *slot, self.job);
                    repo.unpin(self.job);
                }
                return Err(Error::from(e).context(&format!("starting {} on {}", self.job, m.name)));
            }
        };
        let local = r["address"].as_str().unwrap_or_default().to_string();
        let address = match (&m.on, local.contains('/')) {
            (Some(on), false) => format!("{on}/{local}"),
            _ => local.clone(),
        };
        if let Some((key, slot, _)) = &tree {
            let there = pool::split_address(&local).1.to_string();
            if let Err(e) = sync::update(hc, m, key, *slot, self.job, |l| l.terminal = Some(there.clone())) {
                // It runs all the same, and gc knows it by its labels.
                eprintln!("herdr-pool: {} started, but its lease doesn't name its terminal: {e}", self.job);
            }
        }
        // Claude Code didn't take its task: if it is asking about trust after
        // all (its config was reset), the next run looks again.
        if self.claude && r["first_message"]["sent"] == false {
            if let Some(repo) = self.repo {
                crate::trust::forget(m, &repo.key);
            }
        }
        Ok(Started {
            job: self.job.into(),
            address,
            machine: m.name.clone(),
            dir: tree.map(|t| t.2).or(cwd),
            base: self.snap.map(|s| s.sha.clone()),
            dirty: self.snap.is_some_and(|s| s.dirty),
            reply,
            first_message: r["first_message"].clone(),
        })
    }

    /// The tree on `m`: snapshot checked out, dependencies installed.
    #[allow(clippy::too_many_arguments)]
    fn prepare(
        &self,
        hc: &mut Hc,
        m: &pool::Machine,
        repo: &Repo,
        snap: &Snap,
        slot: u32,
        cfg: &Config,
        env: &hc::Env,
        spin: ui::Spin,
    ) -> Result<String> {
        let branch = (self.kind == Kind::Agent).then(|| format!("pool/{}", self.job));
        let dir = sync::checkout(hc, m, &repo.key, slot, &snap.sha, branch.as_deref(), self.fresh, self.job, env)?;
        spin.ok(format!("Work tree {}#{slot} on {}", repo.key, m.name));
        if self.setup {
            if let Some((cmd, files)) = setup_for(cfg, repo, &snap.sha) {
                let s = ui::spin(format!("Installing dependencies on {}  {}", m.name, ui::dim(&cmd)));
                match sync::setup(hc, m, &repo.key, slot, &cmd, &files, env) {
                    Ok(Setup::Warm) => s.ok(format!("Dependencies already there  {}", ui::dim(&cmd))),
                    Ok(Setup::Ran) => s.ok(format!("Installed dependencies  {}", ui::dim(&cmd))),
                    Err(e) => {
                        match ui::live() {
                            true => s.fail(format!("Setup failed on {}", m.name)),
                            false => s.clear(),
                        }
                        return Err(e);
                    }
                }
            }
        }
        Ok(dir)
    }
}

impl Started {
    fn line(&self) -> String {
        let mut line = format!("{}  {}  on {}", self.job, self.address, self.machine);
        if let Some(base) = &self.base {
            line += &format!(" · tree {}{}", out::short(base), if self.dirty { "+uncommitted" } else { "" });
        } else if let Some(dir) = &self.dir {
            line += &format!(" · in {dir}");
        }
        if let Some(reply) = &self.reply {
            line += &format!(" · reports to {reply}");
        }
        if self.first_message["sent"] == false {
            let why = self.first_message["reason"].as_str().unwrap_or("not ready");
            line += &format!(
                "\n  task NOT sent ({why}): hc read {a}; answer any dialog (hc keys {a} enter), then hc send {a} \"<task>\"",
                a = self.address
            );
        }
        line
    }

    /// What a person reads: where it runs, and the next commands.
    fn card(&self, kind: Kind) {
        let g = ui::g();
        let lead = if self.first_message["sent"] == false { ui::warn(g.warn) } else { ui::good(g.dot) };
        println!("{lead} {} runs on {}  {}", ui::bold(&self.job), ui::bold(&self.machine), ui::dim(&self.address));
        let mut rows: Vec<(&str, String)> = Vec::new();
        if let Some(base) = &self.base {
            rows.push(("tree", format!("{}{}", out::short(base), if self.dirty { " with uncommitted changes" } else { "" })));
        } else if let Some(dir) = &self.dir {
            rows.push(("in", dir.clone()));
        }
        if let Some(reply) = &self.reply {
            rows.push(("reports", format!("to {reply}")));
        }
        if self.first_message["sent"] == false {
            let why = self.first_message["reason"].as_str().unwrap_or("not ready");
            rows.push(("task", ui::warn(format!("not sent ({why}): answer it with hc attach {}, then send the task", self.address))));
        }
        rows.push(("watch", ui::accent(format!("hc attach {}", self.address))));
        rows.push(("wait", ui::accent(format!("herdr-pool wait {}", self.job))));
        if self.base.is_some() && kind == Kind::Agent {
            rows.push(("bring", ui::accent(format!("herdr-pool pull {}", self.job))));
        }
        for (k, v) in rows {
            println!("  {}  {v}", ui::dim(format!("{k:<7}")));
        }
    }

    fn json(&self) -> Value {
        json!({
            "job": self.job, "address": self.address, "machine": self.machine, "dir": self.dir,
            "base": self.base, "uncommitted": self.dirty, "reports_to": self.reply, "first_message": self.first_message,
        })
    }
}

/// What the job reads at the end of its task: where it is and how to report.
fn footer(job: &str, machine: &str, snap: Option<&Snap>, reply: Option<&str>) -> String {
    let mut s = format!("\n\n[herdr-pool] You are job {job} on {machine}");
    if let Some(snap) = snap {
        let what = if snap.dirty { "a snapshot of the repository with uncommitted changes" } else { "a snapshot of the repository" };
        s += &format!(", in a work tree holding {what} ({}). Edit and commit there; never push: your work is collected from the tree", out::short(&snap.sha));
    }
    s += ". ";
    s += &match reply {
        Some(to) => format!("When done or stuck, report with: hc send {to} \"<short result>\" (put long output in a file and send its path)."),
        None => "When done, end with a short summary.".into(),
    };
    s
}

/// The repo's dependency install: configured, else from its lockfile.
fn setup_for(cfg: &Config, repo: &Repo, sha: &str) -> Option<(String, Vec<String>)> {
    let conf = cfg.repo(&repo.names, &repo.root);
    let files: Vec<String> = crate::git::git(&repo.root, &["ls-tree", "--name-only", sha])
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect();
    match conf.setup {
        Some(cmd) if cmd.trim().is_empty() => None,
        Some(cmd) => {
            let when = conf.setup_when.unwrap_or_else(|| sync::default_setup(&files).map(|d| d.1).unwrap_or_default());
            Some((cmd, when))
        }
        None => sync::default_setup(&files),
    }
}

/// `14 KB`, `3.2 MB`.
fn bytes(n: u64) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{:.0} KB", n as f64 / 1024.0),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

/// The program a template's command runs: its first word after any
/// `NAME=value` settings and `exec`/`env`.
fn program_of(command: &str) -> Option<&str> {
    command.split_whitespace().find(|w| !matches!(*w, "exec" | "env") && !w.split_once('=').is_some_and(|(k, _)| !k.contains('/')))
}

fn basename(program: &str) -> &str {
    Path::new(program).file_name().and_then(|f| f.to_str()).unwrap_or(program)
}

/// Where the task is: the last argument after the program that isn't a flag
/// (`claude --model opus "Fix it" --verbose`).
fn task_at(program: &[String]) -> Option<usize> {
    (1..program.len()).rev().find(|&i| !program[i].starts_with('-'))
}

/// A name from the task's first words: `fix-the-failing` for "Fix the failing
/// test".
pub fn job_name(program: &[String]) -> String {
    let task = task_at(program).map(|i| program[i].as_str()).unwrap_or_default();
    let words: Vec<String> = task
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() > 1)
        .take(3)
        .map(str::to_ascii_lowercase)
        .collect();
    let name = if words.is_empty() { basename(&program[0]).to_string() } else { words.join("-") };
    git_safe(name)
}

/// A name hc takes (1-32 of a-z 0-9 - _ ., starting with a letter or digit,
/// not shaped like an id such as `t7`) that is also a fine git ref component.
/// 26 at most, so a `-12` from `unique` still fits.
fn git_safe(name: String) -> String {
    if !name.chars().any(|c| c.is_ascii_alphanumeric()) {
        return "job".into();
    }
    let slug = crate::git::slug(&name).replace('.', "-");
    let mut s = slug.trim_start_matches(|c: char| !c.is_ascii_alphanumeric()).to_string();
    s.truncate(26);
    let s = s.trim_end_matches(['-', '_']);
    let id_shaped = s.strip_prefix('t').is_some_and(|r| {
        r.starts_with(|c: char| ('1'..='9').contains(&c)) && r.chars().all(|c| c.is_ascii_alphanumeric())
    });
    if id_shaped {
        format!("{s}-job")
    } else {
        s.into()
    }
}

fn unique(wanted: &str, taken: &HashSet<String>) -> String {
    if !taken.contains(wanted) {
        return wanted.into();
    }
    (2..).map(|n| format!("{wanted}-{n}")).find(|n| !taken.contains(n)).expect("some number is free")
}

/// Names in use: live terminals' names, past pool jobs hc still lists, and
/// jobs that still hold a tree anywhere.
fn taken_names(hc: &mut Hc, world: &World) -> HashSet<String> {
    let mut taken = HashSet::new();
    for t in &world.terminals {
        if let Some(n) = t["name"].as_str().filter(|_| pool::is_live(t)) {
            taken.insert(n.to_string());
        }
        if let Some(j) = pool::label(t, JOB) {
            taken.insert(j.to_string());
        }
    }
    for m in world.machines.iter().filter(|m| m.reachable()) {
        if let Ok((doc, _)) = sync::leases(hc, m) {
            taken.extend(doc.trees.values().flat_map(|s| s.values()).flatten().map(|l| l.job.clone()));
        }
    }
    taken
}

/// A job as found: its machine, its terminal if hc still has it, its tree if
/// it still holds one.
pub struct JobRef {
    pub job: String,
    pub machine: usize,
    pub terminal: Option<Value>,
    pub tree: Option<TreeRef>,
    /// It had a tree and gave it back (rm, gc): maybe another job's now.
    pub gave_back: bool,
}

pub struct TreeRef {
    pub key: String,
    pub slot: u32,
    pub base: String,
    pub lease: Lease,
}

impl JobRef {
    pub fn address(&self) -> Option<&str> {
        self.terminal.as_ref().and_then(|t| t["address"].as_str())
    }

    /// Its tree, or why there is none.
    pub fn tree(&self) -> Result<&TreeRef> {
        match (&self.tree, self.gave_back) {
            (Some(tree), _) => Ok(tree),
            (None, true) => Err(Error::new(format!("{} gave its work tree back already (herdr-pool rm or gc)", self.job))),
            (None, false) => Err(Error::new(format!("{} ran without a work tree", self.job))),
        }
    }
}

/// The terminal `local` on `m`, if it is still `job`'s (hc reuses ids).
fn terminal_of(hc: &mut Hc, m: &pool::Machine, local: &str, job: &str) -> Option<Value> {
    let t = hc.call("terminal.get", json!({"terminal": pool::address_on(m, local)})).ok()?;
    pool::belongs(&t, job).then_some(t)
}

/// Finds `name` (a job name or a terminal address) on any machine.
pub fn find(hc: &mut Hc, world: &World, name: &str) -> Result<JobRef> {
    let ours = |t: &&Value| pool::label(t, JOB).is_some() && pool::label(t, PROBE_LABEL).is_none();
    let by_address = world.terminals.iter().filter(ours).find(|t| t["address"] == name);
    let by_job = || {
        let mut all: Vec<&Value> = world.terminals.iter().filter(ours).filter(|t| pool::label(t, JOB) == Some(name)).collect();
        all.sort_by_key(|t| (pool::is_live(t), t["created_ms"].as_u64().unwrap_or(0)));
        all.last().copied()
    };
    if let Some(t) = by_address.or_else(by_job) {
        let job = pool::label(t, JOB).unwrap_or(name).to_string();
        let address = t["address"].as_str().unwrap_or_default();
        let machine = world.machine_of(address).and_then(|m| world.find(&m.name).ok()).unwrap_or(0);
        let labelled = pool::label(t, TREE).and_then(|v| v.rsplit_once('/')).and_then(|(key, slot)| Some((key.to_string(), slot.parse().ok()?)));
        let mut tree = None;
        if let Some((key, slot)) = &labelled {
            // Only while the job holds it: a tree given back may be another job's now.
            let (doc, _) = sync::leases(hc, &world.machines[machine])?;
            tree = doc.held(key, *slot).filter(|l| l.job == job).map(|l| TreeRef {
                key: key.clone(),
                slot: *slot,
                base: l.base.clone(),
                lease: l.clone(),
            });
        }
        let gave_back = labelled.is_some() && tree.is_none();
        return Ok(JobRef { job, machine, terminal: Some(t.clone()), tree, gave_back });
    }
    for (i, m) in world.machines.iter().enumerate().filter(|(_, m)| m.reachable()) {
        let Ok((doc, _)) = sync::leases(hc, m) else { continue };
        for (key, slots) in &doc.trees {
            for (slot, lease) in slots {
                if let Some(l) = lease.as_ref().filter(|l| l.job == name) {
                    // hc's list leaves out a machine that answers slowly: ask it.
                    let terminal = l.terminal.as_ref().and_then(|t| terminal_of(hc, m, t, &l.job));
                    let tree = TreeRef { key: key.clone(), slot: slot.parse().unwrap_or(0), base: l.base.clone(), lease: l.clone() };
                    return Ok(JobRef { job: name.into(), machine: i, terminal, tree: Some(tree), gave_back: false });
                }
            }
        }
    }
    Err(Error::new(format!("no job {name:?} (herdr-pool ls lists them)")))
}

pub fn ls(argv: &[String]) -> Result<i32> {
    let a = args::parse(argv, &["all", "json"], &[], false)?;
    if a.flag("help") {
        println!("herdr-pool ls [--all] [--json]: pool jobs on every machine (--all: finished ones too)");
        return Ok(0);
    }
    let cfg = config::load()?;
    let mut hc = Hc::connect()?;
    let world = World::discover(&mut hc, &cfg)?;
    let mut held: Vec<(usize, String, String, Lease)> = Vec::new();
    for (i, m) in world.machines.iter().enumerate().filter(|(_, m)| m.reachable()) {
        if let Ok((doc, _)) = sync::leases(&mut hc, m) {
            for (key, slots) in doc.trees {
                for (slot, lease) in slots {
                    if let Some(l) = lease.filter(|l| l.kind != "cleanup") {
                        held.push((i, key.clone(), slot, l));
                    }
                }
            }
        }
    }
    let mut rows = vec![["JOB", "ADDRESS", "MACHINE", "KIND", "STATE", "AGE", "TREE"].map(String::from).to_vec()];
    let mut json_rows = Vec::new();
    let (mut shown, mut shown_jobs) = (HashSet::new(), HashSet::new());
    for t in world.terminals.iter().filter(|t| pool::label(t, JOB).is_some() && pool::label(t, PROBE_LABEL).is_none()) {
        let job = pool::label(t, JOB).unwrap_or_default();
        let machine = world.machine_of(t["address"].as_str().unwrap_or_default()).map(|m| m.name.as_str()).unwrap_or("?");
        let lease = held.iter().find(|(i, _, _, l)| world.machines[*i].name == machine && l.job == job).map(|h| &h.3);
        if !pool::is_live(t) && lease.is_none() && !a.flag("all") {
            continue;
        }
        shown.insert(t["address"].as_str().unwrap_or_default().to_string());
        shown_jobs.insert((machine.to_string(), job.to_string()));
        let (row, json) = terminal_row(&world, t, lease);
        rows.push(row);
        json_rows.push(json);
    }
    // Trees whose job hc didn't list: a test lane between shards, a job
    // still being set up, one on a machine that answered too slowly, or
    // one whose terminal is gone while its tree still holds work.
    for (i, key, slot, l) in &held {
        let m = &world.machines[*i];
        let address = l.terminal.as_ref().map(|t| pool::address_on(m, t));
        if shown_jobs.contains(&(m.name.clone(), l.job.clone())) || address.is_some_and(|a| shown.contains(&a)) {
            continue;
        }
        if let Some(t) = l.terminal.as_ref().and_then(|t| terminal_of(&mut hc, m, t, &l.job)) {
            let (row, json) = terminal_row(&world, &t, Some(l));
            rows.push(row);
            json_rows.push(json);
            continue;
        }
        let state = match l.terminal {
            None if now_ms().saturating_sub(l.since) < 3_600_000 => "starting",
            _ => "gone",
        };
        let tree = describe_tree(Some(&format!("{key}/{slot}")), Some(&l.base), Some(l));
        json_rows.push(json!({"job": l.job, "address": null, "machine": m.name, "kind": l.kind, "state": state, "tree": tree}));
        rows.push(vec![l.job.clone(), "-".into(), m.name.clone(), l.kind.clone(), state.into(), out::ago(l.since), tree]);
    }
    if a.flag("json") {
        println!("{}", Value::Array(json_rows));
    } else if ui::pretty() {
        ls_pretty(&json_rows, &rows, a.flag("all"));
    } else if rows.len() == 1 {
        println!("no pool jobs{}", if a.flag("all") { "" } else { " running (--all for finished ones)" });
    } else {
        print!("{}", out::table(&rows));
    }
    Ok(0)
}

/// A job's state as a glyph and a colour: at work, waiting on you, done well, failed.
fn state_mark(state: &str) -> (String, String) {
    let g = ui::g();
    match state {
        "working" | "running" | "starting" => (ui::accent(g.dot), ui::accent(state)),
        "blocked" => (ui::warn(g.warn), ui::warn("blocked: needs you")),
        "idle" | "done" => (ui::good(g.dot), ui::good(state)),
        "exited" => (ui::good(g.ok), ui::good("exited")),
        "gone" => (ui::dim(g.ring), ui::dim("gone")),
        s if s.starts_with("exit ") || s.starts_with("signal") => (ui::bad(g.bad), ui::bad(s)),
        s => (ui::dim(g.ring), ui::dim(s)),
    }
}

fn ls_pretty(json_rows: &[Value], rows: &[Vec<String>], all: bool) {
    if json_rows.is_empty() {
        let more = if all { String::new() } else { ui::dim(" (--all shows finished ones)") };
        println!("{} No pool jobs running{more}", ui::dim(ui::g().ring));
        println!("  {} {}", ui::accent(ui::g().arrow), ui::dim("herdr-pool run claude \"<task>\""));
        return;
    }
    let mut t = ui::Table { flex: Some(5), ..ui::Table::default() };
    t.rows.push(["", "JOB", "MACHINE", "STATE", "AGE", "TREE", "ADDRESS"].map(ui::dim).to_vec());
    for (j, row) in json_rows.iter().zip(rows.iter().skip(1)) {
        let state = j["state"].as_str().unwrap_or_default();
        let (glyph, word) = state_mark(state);
        let kind = j["kind"].as_str().unwrap_or_default();
        let job = match kind {
            "agent" => ui::bold(j["job"].as_str().unwrap_or_default()),
            _ => j["job"].as_str().unwrap_or_default().to_string(),
        };
        let tree = j["tree"].as_str().unwrap_or("-");
        t.rows.push(vec![glyph, job, row[2].clone(), word, ui::dim(&row[5]), ui::dim(tree), ui::dim(&row[1])]);
    }
    t.print();
    let live = json_rows.iter().filter(|j| matches!(j["state"].as_str(), Some("working" | "running" | "starting" | "blocked" | "idle"))).count();
    println!(
        "{}",
        ui::dim(format!(
            "  {live} live {} hc attach <address> to watch one {} herdr-pool pull <job> brings its work",
            ui::g().sep,
            ui::g().sep
        ))
    );
}

fn terminal_row(world: &World, t: &Value, lease: Option<&Lease>) -> (Vec<String>, Value) {
    let job = pool::label(t, JOB).unwrap_or_default();
    let address = t["address"].as_str().unwrap_or_default();
    let machine = world.machine_of(address).map(|m| m.name.clone()).unwrap_or_else(|| "?".into());
    // A finished job without a lease gave its tree back: it isn't its tree now.
    let tree = match lease.is_none() && !pool::is_live(t) {
        true => "-".into(),
        false => describe_tree(pool::label(t, TREE), pool::label(t, BASE), lease),
    };
    let (kind, word) = (pool::label(t, KIND).unwrap_or("?"), pool::word(t));
    let json = json!({"job": job, "address": address, "machine": machine, "kind": kind, "state": word, "tree": tree});
    let age = out::ago(t["created_ms"].as_u64().unwrap_or(0));
    (vec![job.into(), address.into(), machine, kind.into(), word, age, tree], json)
}

fn describe_tree(tree: Option<&str>, base: Option<&str>, lease: Option<&Lease>) -> String {
    let Some(tree) = tree else { return "-".into() };
    let mut s = tree.replacen('/', "#", 1);
    if let Some(base) = base.filter(|b| !b.is_empty()) {
        s += &format!(" @{}", out::short(base));
    }
    if lease.is_some_and(|l| l.pulled.is_some()) {
        s += " pulled";
    }
    s
}

pub fn wait(argv: &[String]) -> Result<i32> {
    let a = args::parse(argv, &["any", "json"], &["timeout", "tail"], false)?;
    if a.flag("help") {
        println!(
            "herdr-pool wait [job...] [--any] [--timeout S] [--tail N]\n\nWaits until the jobs stop working (agents: their turn ends; commands: they exit).\nNo jobs: every live pool job that is not a test shard. Inside an hc terminal,\n`hc wait --children any` does this for your own children."
        );
        return Ok(0);
    }
    let cfg = config::load()?;
    let mut hc = Hc::connect()?;
    let world = World::discover(&mut hc, &cfg)?;
    let mut targets: Vec<(String, String)> = Vec::new();
    if a.rest.is_empty() {
        for t in world.terminals.iter().filter(|t| pool::is_live(t) && pool::label(t, JOB).is_some()) {
            if pool::label(t, KIND) != Some("shard") && pool::label(t, PROBE_LABEL).is_none() {
                targets.push((pool::label(t, JOB).unwrap_or_default().into(), t["address"].as_str().unwrap_or_default().into()));
            }
        }
        if targets.is_empty() {
            println!("no live pool jobs");
            return Ok(0);
        }
    }
    for name in &a.rest {
        let j = find(&mut hc, &world, name)?;
        let Some(address) = j.address() else { fail!("{name} has no terminal any more") };
        targets.push((j.job.clone(), address.to_string()));
    }
    let timeout = a.num::<f64>("timeout")?;
    let deadline = timeout.map(|s| Instant::now() + Duration::from_secs_f64(s));
    let (tx, rx) = std::sync::mpsc::channel();
    for (job, address) in targets.clone() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send((job, address.clone(), wait_one(&address, deadline)));
        });
    }
    drop(tx);
    let tail = a.num::<usize>("tail")?.unwrap_or(0);
    let pretty = ui::pretty() && !a.flag("json");
    // Who is still being waited for, for the board.
    let pending = std::sync::Arc::new(std::sync::Mutex::new(targets.clone()));
    let began = Instant::now();
    // The jobs still waited for, live, under how long it has been.
    let waiting = || {
        let pending = pending.clone();
        ui::board(move |frame| {
            let left = pending.lock().map(|p| p.clone()).unwrap_or_default();
            let mut lines = vec![ui::dim(format!("  waiting for {} {}", count(left.len(), "job"), ui::secs(began.elapsed().as_millis() as u64)))];
            lines.extend(left.iter().map(|(job, address)| format!("  {} {}  {}", ui::accent(frame), job, ui::dim(address))));
            lines
        })
    };
    let mut board = match pretty {
        true => waiting(),
        false => ui::board(|_| Vec::new()),
    };
    let mut results = Vec::new();
    for (job, address, r) in rx.iter() {
        let r = r.unwrap_or_else(|e| json!({"reason": "error", "error": e.msg}));
        if let Ok(mut p) = pending.lock() {
            p.retain(|(_, a)| *a != address);
        }
        let mut line = format!("{job}  {address}  {}", describe_wait(&r));
        let mut text = String::new();
        if tail > 0 && r["reason"] != "message" {
            if let Ok(read) = hc.call("terminal.read", json!({"terminal": address, "lines": tail})) {
                text = out::tail(read["text"].as_str().unwrap_or_default(), tail);
                line += &format!("\n{text}");
            }
        }
        let stop = a.flag("any") || r["reason"] == "message";
        results.push(json!({"job": job, "address": address, "wait": r}));
        if pretty {
            board.close();
            let (glyph, _) = state_mark(&wait_state(&r));
            println!("{glyph} {}  {}  {}", ui::bold(&job), describe_wait(&r), ui::dim(&address));
            for l in text.lines() {
                println!("  {} {}", ui::dim(ui::g().pipe), ui::fit(l, ui::columns().saturating_sub(5)));
            }
            if !stop && !pending.lock().map(|p| p.is_empty()).unwrap_or(true) {
                board = waiting();
            }
        } else if !a.flag("json") {
            println!("{line}");
        }
        if stop {
            break;
        }
    }
    board.close();
    if a.flag("json") {
        println!("{}", Value::Array(results));
    }
    Ok(0)
}

/// Waits on one terminal in check-ins of at most ten minutes, until it stops
/// working, a message for us arrives, or the deadline passes.
fn wait_one(address: &str, deadline: Option<Instant>) -> Result<Value> {
    let mut hc = Hc::connect()?;
    loop {
        let left = deadline.map(|d| d.saturating_duration_since(Instant::now()).as_secs_f64());
        let mut p = json!({"terminal": address, "timeout": left.unwrap_or(600.0).clamp(1.0, 600.0)});
        if let Some(caller) = hc::caller() {
            p["caller"] = caller.into();
        }
        let r = hc.call("terminal.wait", p)?;
        let timed_out = r["ended"] == false && r["reason"] == "timeout";
        if !timed_out || deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(r);
        }
    }
}

/// A wait's end as a job state word, for its glyph.
fn wait_state(r: &Value) -> String {
    match r["reason"].as_str().unwrap_or_default() {
        "exit" => match r["exit_code"].as_i64() {
            Some(0) => "exited".into(),
            Some(c) => format!("exit {c}"),
            None => "signal".into(),
        },
        "timeout" => "working".into(),
        "error" => "exit ?".into(),
        _ => r["status"].as_str().unwrap_or("idle").into(),
    }
}

fn describe_wait(r: &Value) -> String {
    match r["reason"].as_str().unwrap_or_default() {
        "exit" => match (r["exit_code"].as_i64(), r["signal"].as_i64()) {
            (Some(0), _) => "exited (0)".into(),
            (Some(c), _) => format!("exited ({c})"),
            (None, Some(s)) => format!("killed (signal {s})"),
            _ => "ended".into(),
        },
        "timeout" => format!("still {} (timeout)", r["status"].as_str().unwrap_or("working")),
        "message" => format!("stopped waiting: a message from {} is in your input", r["from"].as_str().unwrap_or("someone")),
        "error" => format!("error: {}", r["error"].as_str().unwrap_or_default()),
        _ => r["status"].as_str().unwrap_or("stopped").into(),
    }
}

pub fn pull(argv: &[String]) -> Result<i32> {
    let a = args::parse(argv, &["apply", "force"], &["branch"], false)?;
    if a.flag("help") || a.rest.len() != 1 {
        println!(
            "herdr-pool pull <job> [--apply] [--branch NAME] [--force]\n\nBrings the job's work (commits and uncommitted changes) back as branch\npool/<job>. --apply also applies it to your work tree. --force moves an\nexisting branch that has diverged."
        );
        return Ok(if a.flag("help") { 0 } else { 2 });
    }
    let repo = Repo::find(&std::env::current_dir()?).ok_or("run pull inside the job's repository")?;
    let cfg = config::load()?;
    let mut hc = Hc::connect()?;
    let world = World::discover(&mut hc, &cfg)?;
    let j = find(&mut hc, &world, &a.rest[0])?;
    let tree = j.tree()?;
    if tree.key != repo.key {
        fail!("{} worked on {}, not this repository ({})", j.job, tree.key, repo.key);
    }
    let base = repo.pinned(&j.job).unwrap_or_else(|| tree.base.clone());
    if repo.have(std::slice::from_ref(&base))?.is_empty() {
        fail!("the snapshot {} isn't in this repository any more", out::short(&base));
    }
    let m = &world.machines[j.machine];
    let branch = a.value("branch").map(String::from).unwrap_or_else(|| format!("pool/{}", j.job));
    let env = pool::env_for(&mut hc, m);
    let dst = format!("refs/heads/{branch}");
    let outcome = sync::bring_back(&mut hc, m, Some(&repo), &tree.key, tree.slot, &j.job, &base, &dst, a.flag("force"), &env);
    let res = match outcome.map_err(|e| e.context("bringing the work back"))? {
        Outcome::Same | Outcome::Changed(_) => {
            let state = j.terminal.as_ref().map(pool::word).unwrap_or_else(|| "gone".into());
            match ui::pretty() {
                true => println!("{} {} has no changes yet  {}", ui::dim(ui::g().ring), ui::bold(&j.job), ui::dim(format!("still {state}"))),
                false => println!("{}: no changes yet (still {state})", j.job),
            }
            return Ok(0);
        }
        Outcome::Fetched(res) => res,
    };
    let _ = sync::update(&mut hc, m, &tree.key, tree.slot, &j.job, |l| l.pulled = Some(res.clone()));
    let summary = repo.summary(&base, &res);
    if !ui::pretty() {
        println!("{branch}  {summary}  (from {}, base {})", m.name, out::short(&base));
        match a.flag("apply") {
            true => println!("{}", repo.apply(&base, &res)?),
            false => println!("next: git diff {}..{branch}  ·  herdr-pool pull {} --apply", out::short(&base), j.job),
        }
        return Ok(0);
    }
    let g = ui::g();
    println!("{} {} {} {}  {}", ui::good(g.ok), ui::bold(&j.job), ui::dim(g.arrow), ui::bold(&branch), ui::dim(format!("from {}, base {}", m.name, out::short(&base))));
    println!("  {}", summary);
    if a.flag("apply") {
        let applied = repo.apply(&base, &res)?;
        println!("{} {applied}", ui::good(g.ok));
        println!("  {} {}  {}", ui::accent(g.arrow), ui::accent("git diff"), ui::dim(format!("then herdr-pool rm {}", j.job)));
    } else {
        println!("  {} {}", ui::accent(g.arrow), ui::accent(format!("git diff {}..{branch}", out::short(&base))));
        println!("  {} {}  {}", ui::accent(g.arrow), ui::accent(format!("herdr-pool pull {} --apply", j.job)), ui::dim("into your work tree"));
    }
    Ok(0)
}

pub fn diff(argv: &[String]) -> Result<i32> {
    let a = args::parse(argv, &["stat"], &["lines"], false)?;
    if a.flag("help") || a.rest.len() != 1 {
        println!("herdr-pool diff <job> [--stat] [--lines N]: the job's changes so far, committed or not");
        return Ok(if a.flag("help") { 0 } else { 2 });
    }
    let cfg = config::load()?;
    let mut hc = Hc::connect()?;
    let world = World::discover(&mut hc, &cfg)?;
    let j = find(&mut hc, &world, &a.rest[0])?;
    let tree = j.tree()?;
    let m = &world.machines[j.machine];
    let env = pool::env_for(&mut hc, m);
    let (text, cut) = sync::diff(&mut hc, m, &tree.key, tree.slot, &tree.base, a.flag("stat"), &env)?;
    if text.trim().is_empty() {
        match ui::pretty() {
            true => println!("{} {} has no changes", ui::dim(ui::g().ring), ui::bold(&j.job)),
            false => println!("{}: no changes", j.job),
        }
    } else {
        let shown = out::head(&text, a.num("lines")?.unwrap_or(400));
        match ui::pretty() {
            true => shown.lines().for_each(|l| println!("{}", colour_diff(l))),
            false => println!("{shown}"),
        }
        if cut {
            println!("[the diff goes on past 8 MB; herdr-pool pull {} brings all of it]", j.job);
        }
    }
    Ok(0)
}

fn colour_diff(line: &str) -> String {
    match line {
        l if l.starts_with("+++") || l.starts_with("---") || l.starts_with("diff ") || l.starts_with("index ") => ui::bold(l),
        l if l.starts_with('+') => ui::good(l),
        l if l.starts_with('-') => ui::bad(l),
        l if l.starts_with("@@") => ui::accent(l),
        l => l.to_string(),
    }
}

pub fn cat(argv: &[String]) -> Result<i32> {
    let a = args::parse(argv, &[], &[], false)?;
    if a.flag("help") || a.rest.len() != 2 {
        println!("herdr-pool cat <job> <path>: a file from the job's work tree (a report it wrote, a build output)");
        return Ok(if a.flag("help") { 0 } else { 2 });
    }
    let cfg = config::load()?;
    let mut hc = Hc::connect()?;
    let world = World::discover(&mut hc, &cfg)?;
    let j = find(&mut hc, &world, &a.rest[0])?;
    let tree = j.tree()?;
    let m = &world.machines[j.machine];
    let env = pool::env_for(&mut hc, m);
    let bytes = sync::cat(&mut hc, m, &tree.key, tree.slot, &a.rest[1], &env)?;
    use std::io::Write;
    std::io::stdout().write_all(&bytes)?;
    Ok(0)
}

pub fn rm(argv: &[String]) -> Result<i32> {
    let a = args::parse(argv, &["force", "purge"], &[], false)?;
    if a.flag("help") || a.rest.is_empty() {
        println!(
            "herdr-pool rm <job...> [--force] [--purge]\n\nEnds the job and gives its tree back (kept warm for the next job; --purge\ndeletes it). Refuses while the tree has work you haven't pulled, unless --force."
        );
        return Ok(if a.flag("help") { 0 } else { 2 });
    }
    let cfg = config::load()?;
    let mut hc = Hc::connect()?;
    let world = World::discover(&mut hc, &cfg)?;
    let here = Repo::find(&std::env::current_dir()?);
    let mut code = 0;
    for name in &a.rest {
        let s = ui::spin(format!("Removing {name}"));
        match remove(&mut hc, &world, here.as_ref(), name, a.flag("force"), a.flag("purge")) {
            Ok(msg) if ui::live() && ui::pretty() => s.ok(capitalised(&msg)),
            Ok(msg) => {
                s.clear();
                println!("{msg}");
            }
            Err(e) => {
                s.clear();
                match ui::live() {
                    true => ui::error(&e.msg),
                    false => eprintln!("herdr-pool: {e}"),
                }
                code = 1;
            }
        }
    }
    Ok(code)
}

fn remove(hc: &mut Hc, world: &World, here: Option<&Repo>, name: &str, force: bool, purge: bool) -> Result<String> {
    let j = find(hc, world, name)?;
    let m = &world.machines[j.machine];
    // All of them: a test run leaves a terminal per failed shard, anywhere.
    let mut terminals: Vec<Value> = world.terminals.iter().filter(|t| pool::label(t, JOB) == Some(j.job.as_str())).cloned().collect();
    if let Some(t) = j.terminal.as_ref().filter(|t| !terminals.iter().any(|x| x["address"] == t["address"])) {
        terminals.push(t.clone());
    }
    let Some(tree) = &j.tree else {
        end(hc, &terminals)?;
        return Ok(format!("removed {} ({})", j.job, m.name));
    };
    let env = pool::env_for(hc, m);
    if !force {
        let state = sync::bring_back(hc, m, None, &tree.key, tree.slot, &j.job, &tree.base, "", false, &env)?;
        if let Outcome::Changed(res) = state {
            if tree.lease.pulled.as_deref() != Some(res.as_str()) {
                fail!("{} has work you haven't pulled: herdr-pool pull {} (or rm --force to drop it)", j.job, j.job);
            }
        }
    }
    // The tree is ours alone from here: no other rm or gc cleans it, and no
    // new job checks out into it until it is given back.
    let me = sync::cleaner(&tree.lease, &world.me.id);
    if !sync::swap(hc, m, &tree.key, tree.slot, Some(&j.job), Then::Held(me.clone()))? {
        fail!("{}'s tree changed hands just now (another rm or gc?): herdr-pool ls", j.job);
    }
    if let Err(e) = end(hc, &terminals) {
        let _ = sync::swap(hc, m, &tree.key, tree.slot, Some(&me.job), Then::Held(tree.lease.clone()));
        return Err(e);
    }
    let dropped = sync::drop_tree(hc, m, &tree.key, tree.slot, &j.job, purge, &env);
    let then = if purge && dropped.is_ok() { Then::Gone } else { Then::Free };
    sync::swap(hc, m, &tree.key, tree.slot, Some(&me.job), then)?;
    dropped?;
    if let Some(repo) = here.filter(|r| r.key == tree.key) {
        repo.unpin(&j.job);
    }
    Ok(format!("removed {} ({})", j.job, m.name))
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// Ends a job's terminals, then removes them.
fn end(hc: &mut Hc, terminals: &[Value]) -> Result<()> {
    for t in terminals.iter().filter(|t| pool::is_live(t)) {
        match hc.call("terminal.kill", json!({"terminal": t["address"]})) {
            Ok(_) => {}
            Err(e) if e.kind == "not_found" => {}
            Err(e) => return Err(Error::from(e).context(&format!("ending {}", t["address"].as_str().unwrap_or("its terminal")))),
        }
    }
    for t in terminals {
        let _ = hc.call("terminal.rm", json!({"terminal": t["address"]}));
    }
    Ok(())
}

pub fn gc(argv: &[String]) -> Result<i32> {
    let a = args::parse(argv, &["force", "purge"], &[], false)?;
    if a.flag("help") {
        println!(
            "herdr-pool gc [--force] [--purge]\n\nGives back trees of jobs that ended and whose work is pulled or empty, and\nremoves finished pool terminals. --force also drops unpulled work; --purge\nalso deletes free trees (their dependencies with them) to free disk."
        );
        return Ok(0);
    }
    let cfg = config::load()?;
    let mut hc = Hc::connect()?;
    let world = World::discover(&mut hc, &cfg)?;
    let here = Repo::find(&std::env::current_dir()?);
    let (purge, force) = (a.flag("purge"), a.flag("force"));
    let s = ui::spin("Cleaning up finished work");
    let (mut freed, mut purged) = (0, 0);
    let (mut kept, mut unsure, mut keep) = (Vec::new(), Vec::new(), HashSet::new());
    for m in world.machines.iter().filter(|m| m.reachable()) {
        let Ok((doc, _)) = sync::leases(&mut hc, m) else { continue };
        let env = pool::env_for(&mut hc, m);
        for (key, slots) in &doc.trees {
            for (slot, lease) in slots {
                let Ok(slot) = slot.parse::<u32>() else { continue };
                let Some(l) = lease else {
                    if purge && purge_free(&mut hc, &world, m, key, slot, &env) {
                        purged += 1;
                    }
                    continue;
                };
                if active(&mut hc, &world, m, l) {
                    continue;
                }
                // What it held for: a cleanup that never finished cleans up its job.
                let job = match l.kind.as_str() {
                    "cleanup" => l.job.strip_prefix('-').unwrap_or_default(),
                    _ => l.job.as_str(),
                };
                if l.kind != "shard" && l.kind != "cleanup" && !force {
                    match sync::bring_back(&mut hc, m, None, key, slot, job, &l.base, "", false, &env) {
                        Ok(Outcome::Same) => {}
                        Ok(Outcome::Changed(res)) if l.pulled.as_deref() == Some(res.as_str()) => {}
                        Ok(_) => {
                            kept.push(format!("{job} ({})", m.name));
                            keep.insert(job.to_string());
                            continue;
                        }
                        Err(e) => {
                            unsure.push(format!("{job} ({}): {}", m.name, e.msg));
                            keep.insert(job.to_string());
                            continue;
                        }
                    }
                }
                let me = sync::cleaner(l, &world.me.id);
                if !matches!(sync::swap(&mut hc, m, key, slot, Some(&l.job), Then::Held(me.clone())), Ok(true)) {
                    continue; // someone else is at it
                }
                let dropped = sync::drop_tree(&mut hc, m, key, slot, job, purge, &env);
                let then = if purge && dropped.is_ok() { Then::Gone } else { Then::Free };
                let _ = sync::swap(&mut hc, m, key, slot, Some(&me.job), then);
                if let Some(repo) = here.as_ref().filter(|r| r.key == *key) {
                    repo.unpin(job);
                }
                freed += 1;
            }
        }
    }
    let mut removed = 0;
    for t in world.terminals.iter().filter(|t| !pool::is_live(t)) {
        let probe = pool::label(t, PROBE_LABEL).is_some();
        if !probe && pool::label(t, JOB).is_none_or(|j| keep.contains(j)) {
            continue;
        }
        if hc.call("terminal.rm", json!({"terminal": t["address"]})).is_ok() {
            removed += 1;
        }
    }
    let mut msg = format!("gave back {freed} tree(s), removed {removed} finished terminal(s)");
    if purged > 0 {
        msg += &format!(", deleted {purged} free tree(s)");
    }
    if !ui::pretty() {
        s.clear();
        println!("{msg}");
        if !kept.is_empty() {
            println!("kept, with unpulled work: {} (herdr-pool pull <job>, or gc --force)", kept.join(", "));
        }
        if !unsure.is_empty() {
            println!("kept, couldn't look for unpulled work: {} (gc --force drops them)", unsure.join("; "));
        }
        return Ok(0);
    }
    s.clear();
    let g = ui::g();
    let mut parts = vec![format!("gave back {}", count(freed, "tree")), format!("removed {}", count(removed, "finished terminal"))];
    if purged > 0 {
        parts.push(format!("deleted {}", count(purged, "free tree")));
    }
    println!("{} {}", ui::good(g.ok), capitalised(&parts.join(&format!(" {} ", g.sep))));
    if !kept.is_empty() {
        println!("{} Kept, with work you haven't pulled: {}", ui::warn(g.warn), kept.join(", "));
        println!("  {} {}  {}", ui::accent(g.arrow), ui::accent("herdr-pool pull <job>"), ui::dim("or gc --force to drop it"));
    }
    if !unsure.is_empty() {
        println!("{} Kept, couldn't look for unpulled work: {}", ui::warn(g.warn), unsure.join("; "));
        println!("  {} {}", ui::accent(g.arrow), ui::dim("gc --force drops them"));
    }
    Ok(0)
}

fn count(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// Whether the job holding a lease may still be at work: its terminal runs
/// (or can't be asked about), or it has none yet and is being set up.
fn active(hc: &mut Hc, world: &World, m: &pool::Machine, l: &Lease) -> bool {
    match &l.terminal {
        Some(t) => match hc.call("terminal.get", json!({"terminal": pool::address_on(m, t)})) {
            Ok(t) => pool::is_live(&t) && pool::belongs(&t, &l.job),
            Err(e) => e.kind != "not_found",
        },
        None => {
            now_ms().saturating_sub(l.since) < 3_600_000
                || world.terminals.iter().any(|t| pool::is_live(t) && pool::belongs(t, &l.job))
        }
    }
}

/// Deletes a free tree, holding its slot meanwhile so nobody checks out
/// into a tree on its way out.
fn purge_free(hc: &mut Hc, world: &World, m: &pool::Machine, key: &str, slot: u32, env: &hc::Env) -> bool {
    let free = Lease { job: String::new(), kind: String::new(), terminal: None, base: String::new(), since: 0, by: String::new(), pulled: None };
    let me = sync::cleaner(&free, &world.me.id); // job `-`: no job's refs to drop
    if !matches!(sync::swap(hc, m, key, slot, None, Then::Held(me.clone())), Ok(true)) {
        return false;
    }
    let dropped = sync::drop_tree(hc, m, key, slot, "", true, env).is_ok();
    let then = if dropped { Then::Gone } else { Then::Free };
    let _ = sync::swap(hc, m, key, slot, Some(&me.job), then);
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn a_templates_program_skips_settings() {
        assert_eq!(program_of("claude --model opus"), Some("claude"));
        assert_eq!(program_of("FOO=1 BAR=x exec /opt/bin/claude"), Some("/opt/bin/claude"));
        assert_eq!(program_of("env A=b codex"), Some("codex"));
        assert_eq!(program_of("sh /tmp/x=y/fake.sh"), Some("sh"));
        assert_eq!(program_of(""), None);
    }

    #[test]
    fn names_come_from_the_task() {
        assert_eq!(job_name(&s(&["claude", "Fix the failing test in x.rs"])), "fix-the-failing");
        assert_eq!(job_name(&s(&["claude", "--model", "opus", "Review: auth.ts"])), "review-auth-ts");
        assert_eq!(job_name(&s(&["/usr/bin/cargo"])), "cargo");
        let taken: HashSet<String> = ["a".into(), "a-2".into()].into();
        assert_eq!(unique("a", &taken), "a-3");
        assert_eq!(git_safe("Weird Name!!".into()), "weird-name");
        // Names hc would refuse: shaped like an id, or not starting with a letter or digit.
        assert_eq!(git_safe("T2".into()), "t2-job");
        assert_eq!(git_safe("_x".into()), "x");
        assert_eq!(git_safe("!!".into()), "job");
        assert_eq!(job_name(&s(&["claude", "t42"])), "t42-job");
    }

    #[test]
    fn the_task_is_the_last_word_that_is_not_a_flag() {
        assert_eq!(task_at(&s(&["claude", "Fix it", "--verbose"])), Some(1));
        assert_eq!(task_at(&s(&["claude", "--model", "opus", "Fix it"])), Some(3));
        assert_eq!(task_at(&s(&["claude"])), None);
        assert_eq!(task_at(&s(&["claude", "--continue"])), None);
    }

    #[test]
    fn the_footer_says_where_and_how_to_report() {
        let snap = Snap { sha: "1234567890".into(), dirty: true };
        let f = footer("fix", "studio", Some(&snap), Some("laptop/t5"));
        assert!(f.contains("job fix on studio"));
        assert!(f.contains("uncommitted changes (1234567)"));
        assert!(f.contains("hc send laptop/t5"));
        assert!(footer("fix", "local", None, None).ends_with("end with a short summary."));
    }
}
