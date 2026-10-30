//! The machines themselves: how much room each has, a command on all of them,
//! getting them ready, and checking that they are.

use crate::args;
use crate::config::{self, Config};
use crate::error::{Error, Result};
use crate::hc::{self, Env, Hc};
use crate::out::{self, plural};
use crate::pool::{self, Kind, Machine, World, JOB};
use crate::ui::{self, Tone};
use serde_json::{json, Value};
use std::path::PathBuf;

pub fn status(argv: &[String]) -> Result<i32> {
    let a = args::parse(argv, &["json"], &[], false)?;
    if a.flag("help") {
        println!("herdr-pool status [--json]: every machine's state, CPU load, free memory and room");
        return Ok(0);
    }
    let cfg = config::load()?;
    let mut hc = Hc::connect()?;
    let mut world = World::discover(&mut hc, &cfg)?;
    let s = ui::spin("Asking every machine how busy it is");
    world.probe(|_| true);
    s.clear();
    let d = &cfg.defaults;
    let live_jobs = world.terminals.iter().filter(|t| pool::is_live(t) && pool::label(t, JOB).is_some()).count();
    if a.flag("json") {
        let machines: Vec<Value> = world
            .machines
            .iter()
            .map(|m| {
                let l = m.load.as_ref();
                json!({
                    "name": m.name, "host": m.host, "state": m.state, "why": m.why, "hc": m.version,
                    "disabled": m.conf.disabled, "tags": m.conf.tags,
                    "cpus": l.map(|l| l.ncpu), "load": l.map(|l| l.load1), "mem_free": l.map(|l| l.mem_free),
                    "mem_gb": l.map(|l| l.mem_gb), "model": l.map(|l| l.model.clone()),
                    "agents": m.agents, "max_agents": m.cap(Kind::Agent, d), "jobs": m.jobs, "max_jobs": m.cap(Kind::Job, d),
                })
            })
            .collect();
        println!("{}", json!({"machines": machines, "live_jobs": live_jobs}));
        return Ok(0);
    }
    if ui::pretty() {
        status_cards(&world, d, live_jobs);
        return Ok(0);
    }
    let mut rows = vec![["MACHINE", "STATE", "CPU", "MEM FREE", "AGENTS", "JOBS", "TAGS"].map(String::from).to_vec()];
    for m in &world.machines {
        let state = match () {
            _ if !m.reachable() => m.state.clone(),
            _ if m.conf.disabled => "disabled".into(),
            _ if m.on.is_some() && m.version != world.me.version => format!("ok (hc {})", m.version),
            _ => "ok".into(),
        };
        let (cpu, mem) = match &m.load {
            Some(l) => (format!("{:.0}% of {}", l.load1 / f64::from(l.ncpu.max(1)) * 100.0, l.ncpu), format!("{}%", l.mem_free)),
            None => ("-".into(), "-".into()),
        };
        let tags = if m.conf.tags.is_empty() { "-".into() } else { m.conf.tags.join(",") };
        let name = if m.on.is_none() { format!("local ({})", m.host) } else { m.name.clone() };
        rows.push(vec![
            name,
            state,
            cpu,
            mem,
            format!("{}/{}", m.agents, m.cap(Kind::Agent, d)),
            format!("{}/{}", m.jobs, m.cap(Kind::Job, d)),
            tags,
        ]);
    }
    print!("{}", out::table(&rows));
    for m in world.machines.iter().filter(|m| !m.reachable()) {
        println!("{}: {}{}", m.name, m.state, m.why.as_ref().map(|w| format!(" ({w})")).unwrap_or_default());
    }
    if world.machines.len() == 1 {
        println!("only this machine so far: herdr-pool init (or herdr-pool add <ssh-host>)");
    }
    println!("{live_jobs} pool job(s) running · herdr-pool ls");
    Ok(0)
}

/// What the pool calls a machine: `local` for this one.
pub fn label(m: &Machine, _world: &World) -> String {
    m.name.clone()
}

/// `M2 Max · 12 cores · 32 GB`, from what its probe said.
pub fn about(m: &Machine) -> String {
    let Some(l) = &m.load else { return String::new() };
    let mut parts = Vec::new();
    let model = l.model.trim_start_matches("Apple ").trim();
    if !model.is_empty() {
        parts.push(model.split(" @ ").next().unwrap_or(model).replace("(R)", "").replace("(TM)", "").replace(" CPU", ""));
    }
    parts.push(format!("{} cores", l.ncpu));
    if l.mem_gb > 0.0 {
        parts.push(format!("{:.0} GB", l.mem_gb));
    }
    parts.join(&format!(" {} ", ui::sep()))
}

/// A card per machine: what it is, its CPU and memory as meters, its room.
fn status_cards(world: &World, d: &config::Defaults, live_jobs: usize) {
    let cols = ui::columns();
    let name_w = world.machines.iter().map(|m| ui::width(&m.name)).max().unwrap_or(5).max(5);
    let g = ui::g();
    println!();
    for m in &world.machines {
        let (glyph, state) = match () {
            _ if !m.reachable() => (ui::bad(g.ring), m.state.replace('_', " ")),
            _ if m.conf.disabled => (ui::dim(g.ring), "disabled: only with --on".into()),
            _ => (ui::good(g.dot), String::new()),
        };
        let mut what = Vec::new();
        if m.on.is_none() {
            what.push(format!("this machine ({})", m.host));
        }
        if m.reachable() {
            let a = about(m);
            if !a.is_empty() {
                what.push(a);
            }
            if m.on.is_some() {
                let hc = format!("hc {}", m.version);
                what.push(if m.version == world.me.version { hc } else { ui::warn(format!("{hc}, {} here", world.me.version)) });
            }
        } else {
            what.push(ui::bad(&state));
        }
        if m.conf.disabled {
            what.push(ui::dim(&state));
        }
        let tags = m.conf.tags.iter().map(ui::accent).collect::<Vec<_>>().join(" ");
        let head = format!("  {glyph} {}  {}", ui::pad(&ui::bold(&m.name), name_w), ui::dim(what.join(&format!(" {} ", g.sep))));
        let room = cols.saturating_sub(ui::width(&head) + 4);
        match tags.is_empty() || ui::width(&tags) > room {
            true => println!("{}", ui::fit(&head, cols - 1)),
            false => println!("{head}{}{tags}", " ".repeat(cols.saturating_sub(ui::width(&head) + ui::width(&tags) + 2).clamp(2, 6))),
        }
        let lead = " ".repeat(4 + name_w + 2);
        if !m.reachable() {
            let why = m.why.clone().unwrap_or_default();
            if !why.is_empty() {
                println!("{lead}{}", ui::fit(&ui::dim(&why), cols.saturating_sub(lead.len() + 1)));
            }
            let (fix, more) = remedy(m);
            let text = match more {
                Some(more) => format!("{fix}  {}", ui::dim(more)),
                None => fix,
            };
            for (i, l) in ui::wrap(&text, cols.saturating_sub(lead.len() + 3)).iter().enumerate() {
                let arrow = if i == 0 { ui::accent(g.arrow) } else { " ".into() };
                println!("{lead}{arrow} {l}");
            }
            println!();
            continue;
        }
        let room = |used: u32, cap: u32, what: &str| {
            let text = format!("{used}/{cap} {what}");
            match (used, used >= cap) {
                (_, true) => ui::warn(text),
                (0, _) => ui::dim(text),
                _ => text,
            }
        };
        let rooms = format!(
            "{} {} {}",
            room(m.agents, m.cap(Kind::Agent, d), "agents"),
            ui::dim(g.sep),
            room(m.jobs, m.cap(Kind::Job, d), "jobs")
        );
        // The widest meters that leave the line on one row.
        let line = |w: usize| {
            let mut line = Vec::new();
            if let Some(l) = &m.load {
                let cpu = l.load1 / f64::from(l.ncpu.max(1));
                let mem = f64::from(100 - l.mem_free.min(100)) / 100.0;
                for (what, share) in [("cpu", cpu), ("mem", mem)] {
                    let pct = ui::pad_left(&format!("{:.0}%", share * 100.0), 4);
                    line.push(match w {
                        0 => format!("{} {}", ui::dim(what), ui::paint(ui::level(share), pct)),
                        w => format!("{} {} {}", ui::dim(what), ui::meter(share, w, ui::level(share)), ui::paint(ui::level(share), pct)),
                    });
                }
            }
            line.push(rooms.clone());
            line.join("   ")
        };
        let shown = [16, 10, 6, 0].into_iter().map(line).find(|l| lead.len() + ui::width(l) < cols);
        println!("{lead}{}", shown.unwrap_or_else(|| ui::fit(&line(0), cols.saturating_sub(lead.len() + 1))));
        println!();
    }
    let up: Vec<&Machine> = world.machines.iter().filter(|m| m.reachable() && !m.conf.disabled).collect();
    let cores: u32 = up.iter().filter_map(|m| m.load.as_ref()).map(|l| l.ncpu).sum();
    let mem: f64 = up.iter().filter_map(|m| m.load.as_ref()).map(|l| l.mem_gb).sum();
    let mut foot = vec![format!("{} up", plural(up.len(), "machine"))];
    if cores > 0 {
        foot.push(format!("{cores} cores"));
    }
    if mem > 0.0 {
        foot.push(format!("{mem:.0} GB"));
    }
    foot.push(format!("{} running", plural(live_jobs, "job")));
    println!("  {}", ui::dim(foot.join(&format!(" {} ", g.sep))));
    if world.machines.len() == 1 {
        println!("  {} add your other machines: {}", ui::accent(g.arrow), ui::bold("herdr-pool init"));
    } else if live_jobs > 0 {
        println!("  {} {}", ui::accent(g.arrow), ui::dim("herdr-pool ls"));
    }
    println!();
}

/// What to do about a machine the pool can't use, and a note on it.
pub fn remedy(m: &Machine) -> (String, Option<String>) {
    match (m.state.as_str(), &m.ssh) {
        ("needs_login", _) => (format!("hc machine login {}", m.name), Some("ssh needs you once: a password, a code or its host key".into())),
        ("connecting", _) => ("it is connecting: try again in a moment".into(), None),
        (_, Some(target)) => ("is it on and online? The pool reconnects once it answers".into(), Some(format!("check with ssh {target}"))),
        (_, None) => ("is it on? The pool reconnects once its --via command reaches it".into(), None),
    }
}

/// Machines `--on`/`--needs` keep (disabled ones too: these are your
/// commands, not placement), reachable or not.
pub fn wanted<'w>(world: &'w World, a: &args::Args) -> Result<Vec<&'w Machine>> {
    let on = a.list("on");
    let needs = a.list("needs");
    for name in &on {
        world.find(name)?;
    }
    Ok(world
        .machines
        .iter()
        .filter(|m| on.is_empty() || on.contains(&m.name) || (m.on.is_none() && on.contains(&world.me.name)))
        .filter(|m| needs.iter().all(|t| m.conf.tags.contains(t)))
        .collect())
}

/// The reachable ones of those.
fn chosen<'w>(world: &'w World, a: &args::Args) -> Result<Vec<&'w Machine>> {
    Ok(wanted(world, a)?.into_iter().filter(|m| m.reachable()).collect())
}

pub fn each(argv: &[String]) -> Result<i32> {
    let a = args::parse(argv, &["json"], &["on", "needs", "timeout", "lines"], true)?;
    if a.flag("help") || a.rest.is_empty() {
        println!(
            "herdr-pool each [--on M,N] [--needs T] [--timeout S] [--lines N] -- <command>\n\nRuns a shell command on every machine at once (with each one's login PATH)\nand prints the end of what each wrote. Exits 1 if any failed."
        );
        return Ok(if a.flag("help") { 0 } else { 2 });
    }
    let line = match a.rest.len() {
        1 => a.rest[0].clone(),
        _ => out::shell_join(&a.rest),
    };
    let cfg = config::load()?;
    let mut hc = Hc::connect()?;
    let world = World::discover(&mut hc, &cfg)?;
    let machines = chosen(&world, &a)?;
    let timeout = a.num::<f64>("timeout")?.unwrap_or(120.0);
    let lines = a.num::<usize>("lines")?.unwrap_or(20);
    let s = ui::spin(format!("Running on {}: {line}", plural(machines.len(), "machine")));
    let results: Vec<(&Machine, Result<hc::ExecOut>)> = std::thread::scope(|s| {
        let handles: Vec<_> = machines
            .iter()
            .map(|m| {
                let line = &line;
                (*m, s.spawn(move || -> Result<hc::ExecOut> {
                    let mut hc = Hc::connect()?;
                    let env = pool::env_for(&mut hc, m);
                    let argv = ["sh".to_string(), "-c".into(), line.clone()];
                    Ok(hc.exec(m.on.as_deref(), &argv, None, &env, None, timeout)?)
                }))
            })
            .collect();
        handles.into_iter().map(|(m, h)| (m, h.join().unwrap_or_else(|_| Err(Error::new("crashed"))))).collect()
    });
    s.clear();
    let mut code = 0;
    let mut json_out = Vec::new();
    let pretty = ui::pretty() && !a.flag("json");
    for (m, r) in results {
        match r {
            Ok(o) => {
                if !o.ok() {
                    code = 1;
                }
                let status = match (o.timed_out, o.code) {
                    (true, _) => "timed out".to_string(),
                    (_, Some(c)) => format!("exit {c}"),
                    _ => "killed".into(),
                };
                if a.flag("json") {
                    json_out.push(json!({"machine": m.name, "ok": o.ok(), "result": status, "stdout": o.text(), "stderr": String::from_utf8_lossy(&o.stderr)}));
                    continue;
                }
                let text = out::tail(&o.text(), lines);
                let err = if o.ok() { String::new() } else { out::tail(&String::from_utf8_lossy(&o.stderr), lines) };
                if pretty {
                    let glyph = if o.ok() { ui::good(ui::g().ok) } else { ui::bad(ui::g().bad) };
                    let status = if o.ok() { ui::dim(&status) } else { ui::bad(&status) };
                    println!("{glyph} {}  {status}", ui::bold(&m.name));
                    for l in text.lines().chain(err.lines()) {
                        println!("  {} {}", ui::dim(ui::g().pipe), ui::fit(&l.replace('\t', "  "), ui::columns().saturating_sub(5)));
                    }
                    continue;
                }
                println!("== {}  {status}", m.name);
                if !text.is_empty() {
                    println!("{text}");
                }
                if !err.is_empty() {
                    println!("{err}");
                }
            }
            Err(e) => {
                code = 1;
                match (a.flag("json"), pretty) {
                    (true, _) => json_out.push(json!({"machine": m.name, "ok": false, "result": "error", "error": e.msg})),
                    (false, true) => println!("{} {}  {}", ui::bad(ui::g().bad), ui::bold(&m.name), ui::bad(&e.msg)),
                    (false, false) => println!("== {}  error: {e}", m.name),
                }
            }
        }
    }
    // Machines it couldn't run on: said, though they don't fail the run.
    for m in wanted(&world, &a)?.into_iter().filter(|m| !m.reachable()) {
        let state = m.state.replace('_', " ");
        match (a.flag("json"), pretty) {
            (true, _) => json_out.push(json!({"machine": m.name, "ok": false, "result": "skipped", "why": state})),
            (false, true) => println!("{} {}  {}", ui::dim(ui::g().ring), ui::bold(&m.name), ui::dim(format!("skipped: {state}"))),
            (false, false) => println!("== {}  skipped: {state}", m.name),
        }
    }
    if a.flag("json") {
        println!("{}", Value::Array(json_out));
    }
    Ok(code)
}

/// What a machine has: git and the agents (with versions), who git commits
/// as, and free disk.
const CHECK: &str = r#"for c in "$@"; do
  if command -v "$c" >/dev/null 2>&1; then
    v=$("$c" --version </dev/null 2>/dev/null | head -n 1)
    printf '%s\t%s\n' "$c" "${v:-?}"
  fi
done
printf '@email\t%s\n' "$(git config --global user.email 2>/dev/null)"
printf '@name\t%s\n' "$(git config --global user.name 2>/dev/null)"
printf '@disk\t%s\n' "$(df -Pk "$HOME" 2>/dev/null | awk 'NR==2{printf "%d", $4/1048576}')"
"#;

pub struct Health {
    /// Programs found, with their versions: git first, then the agents.
    pub tools: Vec<(String, String)>,
    pub email: String,
    pub user: String,
    pub disk_gb: Option<u64>,
}

impl Health {
    pub fn has_git(&self) -> bool {
        self.tools.iter().any(|(t, _)| t == "git")
    }

    pub fn agents(&self) -> Vec<(String, String)> {
        self.tools.iter().filter(|(t, _)| t != "git").cloned().collect()
    }
}

fn parse_health(text: &str) -> Health {
    let mut h = Health { tools: Vec::new(), email: String::new(), user: String::new(), disk_gb: None };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('\t') else { continue };
        match k {
            "@email" => h.email = v.trim().into(),
            "@name" => h.user = v.trim().into(),
            "@disk" => h.disk_gb = v.trim().parse().ok(),
            _ => h.tools.push((k.into(), version_of(v))),
        }
    }
    h
}

/// Every machine in `ms` checked at once (`refresh`: its login PATH asked
/// again first).
pub fn health_of<'m>(ms: &[&'m Machine], cfg: &Config, refresh: bool) -> Vec<(&'m Machine, Result<Health>)> {
    let mut programs = vec!["git".to_string()];
    programs.extend(cfg.defaults.agents.iter().cloned());
    std::thread::scope(|s| {
        let handles: Vec<_> = ms
            .iter()
            .map(|m| {
                let programs = &programs;
                (*m, s.spawn(move || -> Result<Health> {
                    let mut hc = Hc::connect()?;
                    if refresh && m.on.is_some() {
                        pool::capture_path(&mut hc, m);
                    }
                    let env = pool::env_for(&mut hc, m);
                    let args: Vec<&str> = programs.iter().map(String::as_str).collect();
                    let o = hc.sh(m.on.as_deref(), CHECK, &args, &env, 60.0)?;
                    match o.ok() {
                        true => Ok(parse_health(&o.text())),
                        false => Err(Error::new(o.why())),
                    }
                }))
            })
            .collect();
        handles.into_iter().map(|(m, h)| (m, h.join().unwrap_or_else(|_| Err(Error::new("crashed"))))).collect()
    })
}

/// Who git commits as here: (name, email), when both are set.
pub fn git_identity() -> Option<(String, String)> {
    let get = |k: &str| {
        let out = std::process::Command::new("git").args(["config", "--global", k]).output().ok()?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|v| !v.is_empty())
    };
    Some((get("user.name")?, get("user.email")?))
}

pub fn set_identity(hc: &mut Hc, m: &Machine, name: &str, email: &str) -> Result<()> {
    let env = pool::env_for(hc, m);
    let o = hc.sh(m.on.as_deref(), r#"git config --global user.name "$1" && git config --global user.email "$2""#, &[name, email], &env, 30.0)?;
    match o.ok() {
        true => Ok(()),
        false => Err(Error::new(o.why())),
    }
}

/// One finding of `doctor`: how bad, about what, and what puts it right.
struct Finding {
    tone: Tone,
    what: String,
    fix: Option<String>,
}

pub fn doctor(argv: &[String]) -> Result<i32> {
    let a = args::parse(argv, &["refresh", "fix", "yes"], &["on", "needs"], false)?;
    if a.flag("help") {
        println!(
            "herdr-pool doctor [--on M,N] [--needs T] [--refresh] [--fix [--yes]]\n\nChecks hc, git, the agents and disk on every machine. --refresh asks each\nmachine's login PATH again; --fix puts right what it can on those machines\n(git identity, hc versions, logins, the agent skill): it asks first, and\nwith no terminal to ask in it changes only what --yes allows."
        );
        return Ok(0);
    }
    let (cfg, config_problem) = match config::load() {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(e.msg)),
    };
    let mut hc = Hc::connect()?;
    let world = World::discover(&mut hc, &cfg)?;
    if a.flag("fix") {
        let names: Vec<String> = wanted(&world, &a)?.iter().map(|m| m.name.clone()).collect();
        fix(&mut hc, &world, &cfg, a.flag("yes"), &names)?;
        if ui::live() {
            eprintln!();
        }
    }
    let world = World::discover(&mut hc, &cfg)?;
    let wanted = wanted(&world, &a)?;
    let reachable: Vec<&Machine> = wanted.iter().copied().filter(|m| m.reachable()).collect();
    let s = ui::spin(format!("Checking {}", plural(wanted.len(), "machine")));
    let reports = health_of(&reachable, &cfg, a.flag("refresh"));
    s.clear();
    let skills = installed_skills();
    let stale_skill = skills.iter().any(|p| std::fs::read_to_string(p).is_ok_and(|s| s != SKILL));
    // Findings per machine, in the pool's order, then this machine's own.
    let mut found: Vec<(String, Vec<Finding>, String)> = Vec::new();
    for m in &wanted {
        let mut f = Vec::new();
        if !m.reachable() {
            let fix = match remedy(m) {
                (fix, _) if m.state == "needs_login" => format!("{fix} (or doctor --fix)"),
                (fix, Some(more)) => format!("{fix} ({more})"),
                (fix, None) => fix,
            };
            let state = m.state.replace('_', " ");
            f.push(Finding { tone: Tone::Bad, what: m.why.clone().unwrap_or_else(|| state.clone()), fix: Some(fix) });
            found.push((m.name.clone(), f, state));
            continue;
        }
        let Some((_, report)) = reports.iter().find(|(r, _)| r.name == m.name) else { continue };
        let h = match report {
            Ok(h) => h,
            Err(e) => {
                f.push(Finding { tone: Tone::Bad, what: format!("couldn't check it: {e}"), fix: None });
                found.push((m.name.clone(), f, "error".into()));
                continue;
            }
        };
        // hc, git and disk first: the agents' list can be long.
        let mut parts = vec![format!("hc {}", if m.on.is_some() { &m.version } else { &world.me.version })];
        parts.extend(h.tools.iter().filter(|(t, _)| t == "git").map(|(t, v)| format!("{t} {v}")));
        if let Some(gb) = h.disk_gb {
            parts.push(format!("{gb} GB free"));
        }
        parts.extend(h.agents().iter().map(|(t, v)| format!("{t} {v}")));
        let summary = parts.join(&format!(" {} ", ui::sep()));
        if !h.has_git() {
            f.push(Finding { tone: Tone::Bad, what: "git is missing (or not on its login PATH)".into(), fix: Some("install git there".into()) });
        }
        if h.agents().is_empty() {
            let names = cfg.defaults.agents.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
            f.push(Finding {
                tone: Tone::Bad,
                what: format!("none of the agents ({names}, ...) is on its login PATH"),
                fix: Some("install one there and log in once; doctor --refresh if it is installed".into()),
            });
        }
        if h.email.is_empty() {
            f.push(Finding {
                tone: Tone::Bad,
                what: "git user.email is unset there, so agents can't commit".into(),
                fix: Some("git config --global user.email ... (or doctor --fix)".into()),
            });
        }
        if h.disk_gb.is_some_and(|gb| gb < 10) {
            f.push(Finding { tone: Tone::Warn, what: "under 10 GB free".into(), fix: Some("herdr-pool gc --purge frees warm trees".into()) });
        }
        if m.on.is_some() && m.version != world.me.version {
            f.push(Finding {
                tone: Tone::Warn,
                what: format!("hc {} there, {} here", m.version, world.me.version),
                fix: Some(format!("hc machine update {} (or doctor --fix)", m.name)),
            });
        }
        found.push((m.name.clone(), f, summary));
    }
    let mut local = Vec::new();
    if let Some(e) = &config_problem {
        local.push(Finding { tone: Tone::Bad, what: format!("config: {e}"), fix: Some(format!("fix {}", config::path().display())) });
    }
    if skills.is_empty() {
        local.push(Finding { tone: Tone::Warn, what: "the agent skill isn't installed".into(), fix: Some("herdr-pool skill install".into()) });
    } else if stale_skill {
        local.push(Finding { tone: Tone::Warn, what: "the agent skill is out of date".into(), fix: Some("herdr-pool skill install".into()) });
    }
    let problems: usize = found.iter().map(|(_, f, _)| f.len()).sum::<usize>() + local.len();
    if !ui::pretty() {
        // Plain: one line per machine, then the problems, as agents expect.
        println!("hc {} ({}) · config {}", world.me.version, hc::binary().display(), config::path().display());
        if !skills.is_empty() {
            println!("skill: {}", skills.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "));
        }
        let mut lines = Vec::new();
        for (name, f, summary) in &found {
            match reports.iter().any(|(m, r)| m.name == *name && r.is_ok()) {
                true => println!("{name}  ok{}", if summary.is_empty() { String::new() } else { format!(" · {summary}") }),
                false => println!("{name}  {summary}"),
            }
            lines.extend(f.iter().map(|x| format!("{name}: {}{}", x.what, x.fix.as_ref().map(|f| format!(": {f}")).unwrap_or_default())));
        }
        lines.extend(local.iter().map(|x| format!("{}{}", x.what, x.fix.as_ref().map(|f| format!(": {f}")).unwrap_or_default())));
        match lines.is_empty() {
            true => println!("all good"),
            false => lines.iter().for_each(|l| println!("- {l}")),
        }
        return Ok(if problems == 0 { 0 } else { 1 });
    }
    let g = ui::g();
    let cols = ui::columns();
    let mark = |tone: Tone| match tone {
        Tone::Bad => ui::bad(g.bad),
        Tone::Warn => ui::warn(g.warn),
        _ => ui::good(g.ok),
    };
    let worst = |f: &[Finding]| f.iter().map(|x| x.tone).find(|t| *t == Tone::Bad).or_else(|| f.first().map(|x| x.tone)).unwrap_or(Tone::Good);
    // A finding: wrapped under its mark, what puts it right under that.
    let show = |x: &Finding| {
        let room = cols.saturating_sub(12).max(24);
        for (i, l) in ui::wrap(&x.what, room).iter().enumerate() {
            println!("      {} {l}", if i == 0 { mark(x.tone) } else { " ".into() });
        }
        for (i, l) in x.fix.iter().flat_map(|f| ui::wrap(f, room)).enumerate() {
            println!("        {} {}", if i == 0 { ui::accent(g.arrow) } else { " ".into() }, ui::dim(l));
        }
    };
    let name_w = found.iter().map(|(n, _, _)| ui::width(n)).max().unwrap_or(0);
    println!();
    for (name, f, summary) in &found {
        let head = format!("  {} {}  ", mark(worst(f)), ui::pad(&ui::bold(name), name_w));
        let summary = ui::fit(summary, cols.saturating_sub(ui::width(&head) + 1));
        let summary = if reports.iter().any(|(m, r)| m.name == *name && r.is_ok()) { ui::dim(summary) } else { ui::bad(summary) };
        println!("{head}{summary}");
        f.iter().for_each(show);
    }
    // This machine's own part: the skill, the config.
    let mut here = Vec::new();
    if !skills.is_empty() {
        here.push(format!("agent skill in {}", plural(skills.len(), "Claude Code folder")));
    }
    here.push(format!("config {}", crate::onboard::tilde(&config::path().to_string_lossy())));
    println!();
    println!("  {} {}", mark(worst(&local)), ui::dim(ui::fit(&here.join(&format!(" {} ", g.sep)), cols.saturating_sub(5))));
    local.iter().for_each(show);
    println!();
    match problems {
        0 => println!("  {} {}", ui::good(g.ok), ui::good("All good: every machine is ready for agents and tests")),
        n => println!(
            "  {} {}  {}",
            ui::bad(g.bad),
            ui::bad(plural(n, "problem")),
            ui::dim(if a.flag("fix") { "the rest needs you (see above)" } else { "herdr-pool doctor --fix puts right what it can" })
        ),
    }
    println!();
    Ok(if problems == 0 { 0 } else { 1 })
}

/// `doctor --fix`: logins, git identities, hc versions, the skill.
/// Only on the machines named in `only`. Nothing changes without a yes:
/// `--yes`, or the person's answer at a terminal.
fn fix(hc: &mut Hc, world: &World, cfg: &Config, yes: bool, only: &[String]) -> Result<()> {
    let ask = !yes && ui::can_ask();
    for m in world.machines.iter().filter(|m| m.state == "needs_login" && only.contains(&m.name)) {
        let go = match ask {
            true => ui::confirm(&format!("Log in to {} again?", m.name), "hc machine login: ssh may ask for a password or a code", true)?,
            false => false,
        };
        if go {
            let _ = std::process::Command::new(hc::binary()).args(["machine", "login", &m.name]).status();
        } else {
            ui::step(Tone::Warn, &format!("{} needs you to log in: hc machine login {}", m.name, m.name), &[]);
        }
    }
    let world = World::discover(hc, cfg)?;
    crate::onboard::tune(hc, &world, cfg, yes, Some(only))?;
    crate::onboard::skill(yes)
}

/// `git version 2.39.5 (Apple Git-154)` -> `2.39.5`.
fn version_of(text: &str) -> String {
    text.split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()) && w.contains('.'))
        .unwrap_or(text.trim())
        .trim_start_matches('v')
        .to_string()
}

/// Gets machines ready again: their login PATH asked, your hc templates
/// copied over (a template runs from the machine's own copy).
pub fn refresh(argv: &[String]) -> Result<i32> {
    let a = args::parse(argv, &[], &["on", "needs"], false)?;
    if a.flag("help") {
        println!("herdr-pool refresh [--on M,N]: asks each machine's login PATH again and copies your hc templates there");
        return Ok(0);
    }
    let cfg = config::load()?;
    let mut hc = Hc::connect()?;
    let world = World::discover(&mut hc, &cfg)?;
    let templates = local_templates();
    let mut code = 0;
    for m in chosen(&world, &a)?.into_iter().filter(|m| m.on.is_some()) {
        let s = ui::spin(format!("Refreshing {}", m.name));
        match prepare(&mut hc, m, &templates) {
            Ok(msg) if ui::live() => s.ok(format!("{}  {}", m.name, ui::dim(msg))),
            Ok(msg) => println!("{}: {msg}", m.name),
            Err(e) => {
                code = 1;
                match ui::live() {
                    true => s.fail(format!("{}: {e}", m.name)),
                    false => println!("{}: {e}", m.name),
                }
            }
        }
    }
    Ok(code)
}

pub fn prepare(hc: &mut Hc, m: &Machine, templates: &[(String, Vec<u8>)]) -> Result<String> {
    let path = pool::capture_path(hc, m).ok_or("couldn't read its login shell's PATH")?;
    if !templates.is_empty() {
        // Where hc reads templates there: its core's config home, not always ~/.config.
        let dir = hc.sh(m.on.as_deref(), r#"printf '%s' "${XDG_CONFIG_HOME:-$HOME/.config}/herdr-core/templates""#, &[], &Env::new(), 30.0)?;
        let dir = dir.text();
        if !dir.starts_with('/') {
            fail!("couldn't tell where hc keeps templates there");
        }
        for (name, body) in templates {
            hc.write_file(m.on.as_deref(), &format!("{dir}/{name}"), body)?;
        }
    }
    let copied = match templates.len() {
        0 => "no hc templates to copy".to_string(),
        n => format!("{} copied", plural(n, "hc template")),
    };
    Ok(format!("login PATH has {} entries {} {copied}", path.split(':').count(), ui::sep()))
}

pub fn local_templates() -> Vec<(String, Vec<u8>)> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| config::home().join(".config"));
    let Ok(dir) = std::fs::read_dir(base.join("herdr-core/templates")) else { return Vec::new() };
    let mut out: Vec<(String, Vec<u8>)> = dir
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
        .filter_map(|e| Some((e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).ok()?)))
        .collect();
    out.sort();
    out
}

pub const SKILL: &str = include_str!("../skills/herdr-pool/SKILL.md");

/// The skill without its frontmatter: what an agent reads.
pub fn skill_body() -> &'static str {
    SKILL.strip_prefix("---").and_then(|r| r.split_once("\n---\n")).map(|(_, body)| body.trim_start()).unwrap_or(SKILL)
}

/// Every `~/.claude*/skills` folder: one per Claude Code config you use.
pub fn claude_dirs() -> Vec<PathBuf> {
    let home = config::home();
    let Ok(entries) = std::fs::read_dir(&home) else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".claude"))
        .map(|e| e.path().join("skills"))
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

pub fn installed_skills() -> Vec<PathBuf> {
    claude_dirs().into_iter().map(|d| d.join("herdr-pool/SKILL.md")).filter(|p| p.is_file()).collect()
}

pub fn skill(argv: &[String]) -> Result<i32> {
    let a = args::parse(argv, &[], &["dir"], false)?;
    match a.rest.first().map(String::as_str) {
        None if !a.flag("help") => {
            println!("{}", skill_body());
            Ok(0)
        }
        Some("install") => {
            let mut dirs: Vec<PathBuf> = a.list("dir").into_iter().map(PathBuf::from).collect();
            if dirs.is_empty() {
                dirs = claude_dirs();
            }
            if dirs.is_empty() {
                dirs.push(config::home().join(".claude/skills"));
            }
            for d in dirs {
                let file = d.join("herdr-pool/SKILL.md");
                std::fs::create_dir_all(file.parent().expect("has a parent"))?;
                std::fs::write(&file, SKILL)?;
                match ui::pretty() {
                    true => println!("{} {}", ui::good(ui::g().ok), crate::onboard::tilde(&file.to_string_lossy())),
                    false => println!("{}", file.display()),
                }
            }
            Ok(0)
        }
        _ => {
            println!(
                "herdr-pool skill            print the agent skill (also: herdr-pool --skill)\nherdr-pool skill install    install it for Claude Code in every ~/.claude*/skills (or --dir D)"
            );
            Ok(if a.flag("help") { 0 } else { 2 })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_come_out_of_version_lines() {
        assert_eq!(version_of("git version 2.39.5 (Apple Git-154)"), "2.39.5");
        assert_eq!(version_of("2.1.314 (Claude Code)"), "2.1.314");
        assert_eq!(version_of("codex-cli 0.165.0"), "0.165.0");
        assert_eq!(version_of("?"), "?");
    }

    #[test]
    fn health_reads_tools_and_identity() {
        let h = parse_health("git\tgit version 2.39.5\nclaude\t2.1.314 (Claude Code)\n@email\tme@x.io\n@name\tMe\n@disk\t120\n");
        assert!(h.has_git());
        assert_eq!(h.agents(), vec![("claude".to_string(), "2.1.314".to_string())]);
        assert_eq!((h.email.as_str(), h.user.as_str(), h.disk_gb), ("me@x.io", "Me", Some(120)));
        let bare = parse_health("@email\t\n@disk\t\n");
        assert!(!bare.has_git() && bare.email.is_empty() && bare.disk_gb.is_none());
    }

    #[test]
    fn the_skill_has_frontmatter_and_a_body() {
        assert!(SKILL.starts_with("---\nname: herdr-pool\n"));
        assert!(skill_body().starts_with("# herdr-pool"));
        assert!(SKILL.len() < 6000, "the skill stays small: it costs context in every session that loads it");
    }
}
