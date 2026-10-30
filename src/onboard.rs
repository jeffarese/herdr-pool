//! Getting machines into the pool and keeping them fit to work: `init` walks
//! through all of it, `add` takes one machine or picks from those found, and
//! `doctor --fix` puts right what it can. Each question has a default, so
//! `--yes` does the same with no one there.

use crate::args;
use crate::config::{self, Config};
use crate::discover::{self, Candidate, Probe, Reach};
use crate::error::{Error, Result};
use crate::fleet;
use crate::hc::{self, Hc};
use crate::pool::{Machine, World};
use crate::ui::{self, Item, Tone};
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Stdio};

pub const INIT_HELP: &str = "herdr-pool init [user@host ...] [--yes]

Sets the pool up, asking as it goes: hc on this machine; the machines to add
(found in your ssh config and on Tailscale, or the ones you name); hc
installed on them; ssh logins that work on their own; git identities; and the
agent skill for Claude Code. Run it again any time: it does only what is
missing.

  --yes    no questions: add only the machines named, fix what can be fixed";

pub const ADD_HELP: &str = "herdr-pool add [ssh-target] [--name N] [--tags a,b] [--max-agents N] [--max-jobs N]
herdr-pool add --via '<command>' --name N ...

Adds a machine: hc installed there when it lacks it, its pool settings
recorded, its login PATH asked and your hc templates copied. At a terminal,
with no target: pick from the machines in your ssh config and on Tailscale.
ssh must get in on its own (the pool reconnects without you), and add helps
with that when it can ask: a user name, the host key, your ssh key.";

/// A machine on its way in.
struct Joining {
    target: String,
    name: String,
    found: Probe,
    /// hc reaches it with this command instead of ssh (`--via`).
    via: bool,
    session: Option<String>,
    temp: bool,
    max_agents: Option<u32>,
    max_jobs: Option<u32>,
    tags: Vec<String>,
}

impl Joining {
    fn new(target: String, name: String, found: Probe) -> Joining {
        Joining { target, name, found, via: false, session: None, temp: false, max_agents: None, max_jobs: None, tags: Vec::new() }
    }
}

pub fn init(argv: &[String]) -> Result<i32> {
    let a = args::parse(argv, &["yes"], &[], false)?;
    if a.flag("help") {
        println!("{INIT_HELP}");
        return Ok(0);
    }
    let yes = a.flag("yes");
    if !yes && !ui::can_ask() {
        return Err(Error::usage("init asks as it goes, and there is no terminal to ask in: herdr-pool init --yes [user@host ...]"));
    }
    ui::intro("herdr-pool", "your machines as one pool for agents and tests");
    local_hc(yes)?;
    let cfg = config::load()?;
    let mut hc = Hc::connect()?;
    let world = World::discover(&mut hc, &cfg)?;
    ui::done(&pool_now(&world));
    let joining = match (a.rest.is_empty(), yes) {
        (false, _) => named(&a.rest, &world, yes)?,
        (true, true) => Vec::new(),
        (true, false) => choose(&world)?,
    };
    let (_, failed) = join(&mut hc, &cfg, joining, yes)?;
    let world = World::discover(&mut hc, &cfg)?;
    tune(&mut hc, &world, &cfg, yes, None)?;
    skill(yes)?;
    let code = finish(&mut hc, &cfg)?;
    // A machine that should have joined and didn't fails the run, for scripts.
    Ok(if failed > 0 { 1 } else { code })
}

pub fn add(argv: &[String]) -> Result<i32> {
    let a = args::parse(argv, &["via", "temp", "yes"], &["name", "max-agents", "max-jobs", "tags", "session"], false)?;
    if a.flag("help") || a.rest.len() > 1 {
        println!("{ADD_HELP}");
        return Ok(if a.flag("help") { 0 } else { 2 });
    }
    let cfg = config::load()?;
    let mut hc = Hc::connect()?;
    let world = World::discover(&mut hc, &cfg)?;
    let Some(target) = a.rest.first() else {
        if !ui::can_ask() {
            return Err(Error::usage("add needs a machine (there is no terminal to pick one in): herdr-pool add user@host"));
        }
        ui::intro("herdr-pool", "add machines");
        let joining = choose(&world)?;
        let (added, failed) = join(&mut hc, &cfg, joining, false)?;
        let world = World::discover(&mut hc, &cfg)?;
        tune(&mut hc, &world, &cfg, false, Some(&added))?;
        let code = finish(&mut hc, &cfg)?;
        return Ok(if failed > 0 { 1 } else { code });
    };
    // The guided way only with someone to answer it; else hc's own, which asks nothing.
    let mut j = match (a.flag("via"), ui::can_ask()) {
        // hc's own way: it asks at the terminal itself, if there is one.
        (_, false) => {
            let name = match a.value("name") {
                Some(n) => n.to_string(),
                None if a.flag("via") => return Err(Error::usage("--via needs --name")),
                None => discover::machine_name(target.rsplit('@').next().unwrap_or(target).split('.').next().unwrap_or(target)),
            };
            let mut j = Joining::new(target.clone(), name, Probe::default());
            j.via = a.flag("via");
            j
        }
        (true, true) => {
            let name = a.value("name").ok_or_else(|| Error::usage("--via needs --name"))?.to_string();
            if let Some(why) = discover::bad_machine_name(&name) {
                return Err(Error::usage(format!("--name {name}: {why}")));
            }
            if world.machines.iter().any(|m| m.name == name) || name == crate::pool::LOCAL {
                return Err(Error::usage(format!("there is a machine called {name} already")));
            }
            let s = ui::spin(format!("Looking at {name} through {}", ui::dim(target)));
            let found = discover::probe_via(target);
            match &found.reach {
                Reach::Ready => s.clear(),
                Reach::Unsupported(why) => {
                    s.fail(format!("{name} runs {why}: the pool needs macOS or Linux"));
                    return Ok(1);
                }
                Reach::Offline(why) | Reach::Login(why) => s.warn(format!("Couldn't look at {name} first ({why}): adding it anyway")),
                Reach::Unchecked => s.clear(),
            }
            let mut j = Joining::new(target.clone(), name, found);
            j.via = true;
            j
        }
        (false, true) => {
            let host = target.rsplit('@').next().unwrap_or(target).to_ascii_lowercase();
            if let Some(name) = pool_member(&world, &listed(), target, &host) {
                ui::done(&format!("{target} is in the pool already, as {name}"));
                return Ok(0);
            }
            let s = ui::spin(format!("Trying {target}"));
            let mut c = typed(target);
            c.found = discover::probe(target);
            s.clear();
            let mut taken: Vec<String> = world.machines.iter().map(|m| m.name.clone()).collect();
            match reach(&mut c, &mut taken, a.value("name"), a.flag("yes"))? {
                Some(j) => j,
                None => return Ok(1),
            }
        }
    };
    j.session = a.value("session").map(String::from);
    j.temp = a.flag("temp");
    j.max_agents = a.num("max-agents")?;
    j.max_jobs = a.num("max-jobs")?;
    j.tags = a.list("tags");
    let name = j.name.clone();
    let added = match ui::can_ask() {
        false => join_plainly(&mut hc, &cfg, j)?,
        true => join(&mut hc, &cfg, vec![j], a.flag("yes"))?.0,
    };
    if added.is_empty() {
        return Ok(1);
    }
    if ui::can_ask() {
        let world = World::discover(&mut hc, &cfg)?;
        tune(&mut hc, &world, &cfg, a.flag("yes"), Some(&added))?;
    } else {
        println!("next: herdr-pool doctor --on {name}");
    }
    Ok(0)
}

/// Whether to make a change: yes with --yes, the person's answer at a
/// terminal, and no when there is no one to ask (said, with what does it).
fn agree(yes: bool, question: &str, about: &str, instead: &str) -> Result<bool> {
    match (yes, ui::can_ask()) {
        (true, _) => Ok(true),
        (false, true) => ui::confirm(question, about, true),
        (false, false) => {
            let what = question.trim_end_matches('?');
            ui::step(Tone::Warn, &format!("{what}: skipped, with no one to ask"), &[ui::dim(instead)]);
            Ok(false)
        }
    }
}

/// hc on this machine, installed if missing and wanted.
fn local_hc(yes: bool) -> Result<()> {
    let bin = hc::binary();
    if let Some(v) = hc_version(&bin) {
        ui::done(&format!("hc {v} runs here {}", ui::dim(format!("({})", tilde(&bin.to_string_lossy())))));
        return Ok(());
    }
    let install = agree(
        yes,
        "hc (herdr-core) isn't installed here. Install it now?",
        "with its official installer, into ~/.local/bin",
        "curl -fsSL https://releases.herdr.dev/core/install.sh | sh",
    )?;
    if !install {
        return Err(Error::new(hc::INSTALL_HINT));
    }
    let s = ui::spin("Installing hc");
    let out = Command::new("sh").args(["-c", "curl -fsSL https://releases.herdr.dev/core/install.sh | sh"]).stdin(Stdio::null()).output()?;
    let bin = hc::binary();
    match (out.status.success(), hc_version(&bin)) {
        (true, Some(v)) => {
            s.ok(format!("Installed hc {v} {}", ui::dim(format!("({})", tilde(&bin.to_string_lossy())))));
            Ok(())
        }
        _ => {
            s.fail("hc didn't install");
            let text = String::from_utf8_lossy(if out.stderr.is_empty() { &out.stdout } else { &out.stderr }).into_owned();
            Err(Error::new(crate::out::tail(&text, 6)))
        }
    }
}

pub fn hc_version(bin: &Path) -> Option<String> {
    let out = Command::new(bin).arg("--version").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    out.status.success().then(|| text.split_whitespace().last().unwrap_or_default().to_string()).filter(|v| !v.is_empty())
}

/// `~/x` for a path under home.
pub fn tilde(path: &str) -> String {
    let home = config::home();
    match Path::new(path).strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.into(),
    }
}

fn pool_now(world: &World) -> String {
    let others: Vec<String> = world
        .machines
        .iter()
        .filter(|m| m.on.is_some())
        .map(|m| match m.reachable() {
            true => m.name.clone(),
            false => format!("{} {}", m.name, ui::warn(format!("({})", m.state.replace('_', " ")))),
        })
        .collect();
    match others.is_empty() {
        true => format!("The pool: this machine ({}) so far", world.me.name),
        false => format!("The pool: this machine ({}), {}", world.me.name, others.join(", ")),
    }
}

/// Whether `target` (or the host it reaches) is a machine hc has already.
fn in_pool(world: &World, listed: &[(String, String)], target: &str, host: &str) -> bool {
    pool_member(world, listed, target, host).is_some()
}

/// The pool's name for the machine `target` (or `host`) reaches, if it has one.
fn pool_member(world: &World, listed: &[(String, String)], target: &str, host: &str) -> Option<String> {
    let (target, host) = (short_host(target), short_host(host));
    let by_target = listed.iter().find(|(t, _)| short_host(t) == target || short_host(t) == host).map(|(_, n)| n.clone());
    by_target.or_else(|| {
        world.machines.iter().find(|m| m.on.is_some() && (short_host(&m.host) == host || m.name == target)).map(|m| m.name.clone())
    })
}

/// `me@studio.local` -> `studio`; an address stays whole (`100.101.1.2`).
fn short_host(s: &str) -> String {
    let h = s.rsplit('@').next().unwrap_or(s).trim_matches(['[', ']']);
    match h.parse::<std::net::IpAddr>() {
        Ok(_) => h.to_ascii_lowercase(),
        Err(_) => h.split('.').next().unwrap_or(h).to_ascii_lowercase(),
    }
}

/// hc's machines: (target, name).
fn listed() -> Vec<(String, String)> {
    let Ok(mut hc) = Hc::connect() else { return Vec::new() };
    let Ok(r) = hc.call("machine.list", serde_json::json!({})) else { return Vec::new() };
    r["machines"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| (m["target"].as_str().unwrap_or_default().to_string(), m["name"].as_str().unwrap_or_default().to_string()))
        .collect()
}

fn typed(target: &str) -> Candidate {
    let host = target.rsplit('@').next().unwrap_or(target).to_ascii_lowercase();
    Candidate {
        target: target.into(),
        host,
        from_ssh_config: false,
        on_tailnet: false,
        os: None,
        online: true,
        known: true,
        found: Probe::default(),
    }
}

/// Finds machines and asks which should join.
fn choose(world: &World) -> Result<Vec<Joining>> {
    let s = ui::spin("Looking in your ssh config and on Tailscale");
    let me = discover::here();
    let listed = listed();
    let skip = |target: &str, host: &str| {
        in_pool(world, &listed, target, host) || matches!(host, "localhost" | "127.0.0.1" | "::1") || host == me.name.to_ascii_lowercase()
    };
    let mut cands = discover::gather(&config::home(), &skip);
    let known = cands.iter().filter(|c| c.known && c.found.reach == Reach::Unchecked).count();
    if known > 0 {
        s.set(format!("Asking the {} ssh knows what they are", plural(known, "machine")));
    }
    discover::probe_known(&mut cands);
    // This machine under another name, and one machine found twice.
    let mut seen = vec![me.id.clone()];
    cands.retain(|c| {
        let id = &c.found.id;
        if id.is_empty() {
            return true;
        }
        let new = !seen.contains(id);
        seen.push(id.clone());
        new
    });
    cands.retain(|c| c.found.name.is_empty() || !world.machines.iter().any(|m| m.on.is_some() && m.host.eq_ignore_ascii_case(&c.found.name)));
    cands.sort_by_key(rank);
    let count = |f: &dyn Fn(&Reach) -> bool| cands.iter().filter(|c| f(&c.found.reach)).count();
    let ready = count(&|r| *r == Reach::Ready);
    let login = count(&|r| matches!(r, Reach::Login(_)));
    let fresh = count(&|r| *r == Reach::Unchecked);
    let mut parts = Vec::new();
    if ready > 0 {
        parts.push(format!("{ready} ready"));
    }
    if login > 0 {
        parts.push(format!("{login} want a login"));
    }
    if fresh > 0 {
        parts.push(format!("{fresh} new to ssh"));
    }
    let others = cands.len() - ready - login - fresh;
    if others > 0 {
        parts.push(format!("{others} out of reach"));
    }
    match cands.is_empty() {
        true => s.ok("No other machines in your ssh config or on Tailscale"),
        false => s.ok(format!("Found {}: {}", plural(cands.len(), "machine"), parts.join(", "))),
    }
    let mut items: Vec<Item> = cands.iter().map(item).collect();
    items.push(Item { label: "another machine".into(), hint: "type how ssh reaches it".into(), off: None });
    let on: Vec<bool> = cands.iter().map(|c| c.found.reach == Reach::Ready && c.found.hc.is_some()).collect();
    let picked = ui::pick("Which machines should join the pool?", &items, &on)?;
    let mut taken: Vec<String> = world.machines.iter().map(|m| m.name.clone()).collect();
    taken.extend(listed.iter().map(|(_, n)| n.clone()));
    let mut joining = Vec::new();
    for i in picked {
        let mut c = match cands.get(i) {
            Some(c) => c.clone(),
            None => {
                let target = ui::input("How does ssh reach it?", "", "user@host, or a Host from ~/.ssh/config", |t| match t {
                    "" => Err("type a host".into()),
                    t if t.contains(char::is_whitespace) => Err("no spaces: user@host".into()),
                    _ => Ok(()),
                })?;
                let s = ui::spin(format!("Trying {target}"));
                let mut c = typed(&target);
                c.found = discover::probe(&target);
                s.clear();
                c
            }
        };
        if let Some(j) = reach(&mut c, &mut taken, None, false)? {
            joining.push(j);
        }
    }
    Ok(joining)
}

/// Ready ones first (with hc, then without), then those that need you,
/// then those out of reach.
fn rank(c: &Candidate) -> (u8, String) {
    let r = match &c.found.reach {
        Reach::Ready if c.found.hc.is_some() => 0,
        Reach::Ready => 1,
        Reach::Login(_) => 2,
        Reach::Unchecked => 3,
        Reach::Offline(_) => 4,
        Reach::Unsupported(_) => 5,
    };
    (r, c.target.clone())
}

fn item(c: &Candidate) -> Item {
    let p = &c.found;
    let (hint, off) = match &p.reach {
        Reach::Ready => {
            let hc = match &p.hc {
                Some(v) => format!("hc {v}"),
                None => "no hc yet".into(),
            };
            (format!("{} {} {hc}", p.summary(), ui::g().sep), None)
        }
        Reach::Login(_) => ("ssh wants a login: pick it to sort that out".to_string(), None),
        Reach::Unchecked => ("new to ssh: pick it to check its host key".to_string(), None),
        Reach::Offline(why) => (String::new(), Some(why.clone())),
        Reach::Unsupported(what) => (String::new(), Some(format!("{what}: the pool runs on macOS and Linux"))),
    };
    let via = match (c.from_ssh_config, c.on_tailnet) {
        (true, true) => " ssh config, Tailscale",
        (true, false) => " ssh config",
        (false, true) => " Tailscale",
        (false, false) => "",
    };
    let hint = match (&off, via.is_empty()) {
        (None, false) => format!("{hint}  {}", ui::dim(format!("·{via}"))),
        _ => hint,
    };
    Item { label: c.target.clone(), hint, off }
}

/// Gets ssh into `c` with no questions, which the pool needs to reconnect
/// on its own, asking you what that takes. Then its name. None: skipped.
fn reach(c: &mut Candidate, taken: &mut Vec<String>, name: Option<&str>, yes: bool) -> Result<Option<Joining>> {
    let mut target = c.target.clone();
    let mut looked = false;
    for _ in 0..6 {
        match c.found.reach.clone() {
            Reach::Ready => break,
            Reach::Offline(why) | Reach::Unsupported(why) => {
                ui::step(Tone::Warn, &format!("Skipping {target}: {why}"), &[]);
                return Ok(None);
            }
            _ if yes || !ui::can_ask() => {
                let why = match &c.found.reach {
                    Reach::Login(why) => why.clone(),
                    _ => "new to ssh".into(),
                };
                ui::step(Tone::Warn, &format!("Skipping {target}: {why}"), &[ui::dim(format!("ssh {target} once by hand, then add it again"))]);
                return Ok(None);
            }
            Reach::Unchecked if !looked => {
                looked = true;
                ui::step(Tone::Warn, &format!("First login to {target}"), &[ui::dim("ssh shows the host key to check, and may ask for a password")]);
                let _ = Command::new("ssh").args(["-o", "ConnectTimeout=8", &target, "true"]).status();
            }
            Reach::Unchecked => {
                ui::step(Tone::Warn, &format!("Skipping {target}: ssh still can't get in"), &[]);
                return Ok(None);
            }
            Reach::Login(why) => {
                let host = target.rsplit('@').next().unwrap_or(&target).to_string();
                ui::step(Tone::Warn, &format!("{target} wants a login"), &[ui::dim(&why)]);
                let choices = [
                    Item { label: "Install my ssh key there".into(), hint: "ssh-copy-id: you type the password once".into(), off: None },
                    Item { label: "Log in as another user".into(), hint: format!("user@{host}"), off: None },
                    Item { label: "Skip it".into(), ..Item::default() },
                ];
                match ui::select(&format!("How should the pool log in to {host}?"), &choices)? {
                    0 => copy_id(&target)?,
                    1 => {
                        let user = ui::input(&format!("User on {host}"), &local_user(), "", |u| match u.is_empty() || u.contains(['@', ' ']) {
                            true => Err("just the user name".into()),
                            false => Ok(()),
                        })?;
                        target = format!("{user}@{host}");
                    }
                    _ => return Ok(None),
                }
            }
        }
        let s = ui::spin(format!("Trying {target}"));
        c.found = discover::probe(&target);
        s.clear();
    }
    if c.found.reach != Reach::Ready {
        ui::step(Tone::Warn, &format!("Skipping {target}: ssh still can't get in"), &[]);
        return Ok(None);
    }
    c.target = target.clone();
    let suggested = unique(&c.suggested_name(), taken);
    let name = match name {
        Some(n) => n.to_string(),
        None if yes || !ui::can_ask() => suggested,
        None => {
            let taken_now = taken.clone();
            ui::input(&format!("A name for {target} ({})", c.found.summary()), &suggested, "", |n| {
                if let Some(why) = discover::bad_machine_name(n) {
                    return Err(why);
                }
                match n == crate::pool::LOCAL || taken_now.iter().any(|t| t == n) {
                    true => Err(format!("{n} is taken")),
                    false => Ok(()),
                }
            })?
        }
    };
    taken.push(name.clone());
    Ok(Some(Joining::new(target, name, c.found.clone())))
}

fn unique(name: &str, taken: &[String]) -> String {
    let free = |n: &str| n != crate::pool::LOCAL && !taken.iter().any(|t| t == n);
    if free(name) {
        return name.into();
    }
    (2..).map(|i| format!("{name}-{i}")).find(|n| free(n)).expect("a number is free")
}

fn local_user() -> String {
    std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_default()
}

/// Your ssh key into `target`'s authorized_keys (making a key first if you
/// have none), so ssh gets in without asking from then on.
fn copy_id(target: &str) -> Result<()> {
    let ssh = config::home().join(".ssh");
    let has_key = ["id_ed25519.pub", "id_ecdsa.pub", "id_rsa.pub"].iter().any(|k| ssh.join(k).is_file());
    if !has_key {
        if !ui::confirm("This machine has no ssh key yet. Make one?", "ssh-keygen -t ed25519: it asks for a passphrase", true)? {
            return Ok(());
        }
        let key = ssh.join("id_ed25519");
        let _ = Command::new("ssh-keygen").args(["-t", "ed25519", "-f"]).arg(&key).status();
    }
    ui::note(&[ui::dim(format!("ssh-copy-id {target}: type the password for {target}"))]);
    let _ = Command::new("ssh-copy-id").arg(target).status();
    Ok(())
}

/// Machines named on the command line.
fn named(targets: &[String], world: &World, yes: bool) -> Result<Vec<Joining>> {
    let mut taken: Vec<String> = world.machines.iter().map(|m| m.name.clone()).collect();
    let listed = listed();
    let mut joining = Vec::new();
    for target in targets {
        let host = target.rsplit('@').next().unwrap_or(target).to_ascii_lowercase();
        if let Some(name) = pool_member(world, &listed, target, &host) {
            ui::done(&format!("{target} is in the pool already, as {name}"));
            continue;
        }
        let s = ui::spin(format!("Trying {target}"));
        let mut c = typed(target);
        c.found = discover::probe(target);
        s.clear();
        if let Some(j) = reach(&mut c, &mut taken, None, yes)? {
            joining.push(j);
        }
    }
    Ok(joining)
}

/// Adds them: hc installed where missing (once you say so), the machine
/// recorded, its login PATH and your templates. The names added.
fn join(hc: &mut Hc, cfg: &Config, mut joining: Vec<Joining>, yes: bool) -> Result<(Vec<String>, usize)> {
    let version = hc_version(&hc::binary()).unwrap_or_default();
    let how = "a copy of this one, or the release for its system, into ~/.local/bin there";
    // Machines that were looked at and lack hc: one question for all of them.
    let missing: Vec<String> = joining.iter().filter(|j| j.found.reach == Reach::Ready && j.found.hc.is_none()).map(|j| j.name.clone()).collect();
    let mut install: Vec<String> = Vec::new();
    if !missing.is_empty() {
        match agree(yes, &format!("Install hc {version} on {}?", words(&missing)), how, "herdr-pool add <machine> --yes")? {
            true => install = missing,
            false => {
                ui::step(Tone::Warn, &format!("Leaving out {}: the pool needs hc there", words(&missing)), &[]);
                joining.retain(|j| !missing.contains(&j.name));
            }
        }
    }
    let templates = fleet::local_templates();
    let (mut added, mut failed) = (Vec::new(), 0);
    for j in joining {
        let with_hc = yes || install.contains(&j.name);
        let mut s = ui::spin(match with_hc && j.found.hc.is_none() {
            true => format!("Installing hc on {} and adding it", j.name),
            false => format!("Adding {}", j.name),
        });
        let mut tried = machine_add(&j, with_hc);
        // Only hc can tell that some machines lack it, or have one too old
        // to work with this one: ask then.
        let lacking = match &tried {
            Err(r) if !with_hc && (r.kind == "hc_missing" || r.kind == "incompatible") => Some(r.kind.clone()),
            _ => None,
        };
        if let Some(kind) = lacking {
            s.clear();
            let q = match kind.as_str() {
                "hc_missing" => format!("{} has no hc. Install hc {version} there?", j.name),
                _ => format!("{} has an hc that can't work with this one. Put hc {version} there?", j.name),
            };
            if !agree(yes, &q, how, "herdr-pool add <machine> --yes")? {
                ui::step(Tone::Warn, &format!("Left out {}: the pool needs hc there", j.name), &[]);
                continue;
            }
            s = ui::spin(format!("Installing hc on {} and adding it", j.name));
            tried = machine_add(&j, true);
        }
        if let Err(e) = tried {
            failed += 1;
            match ui::live() {
                true => {
                    s.fail(format!("{} wasn't added", j.name));
                    ui::note(&e.msg.lines().map(ui::dim).collect::<Vec<_>>());
                }
                false => s.fail(format!("{} wasn't added: {}", j.name, e.msg.lines().collect::<Vec<_>>().join(" / "))),
            }
            continue;
        }
        config::add_machine(&j.name, j.max_agents, j.max_jobs, &j.tags)?;
        s.set(format!("Asking {} for its login PATH", j.name));
        let world = World::discover(hc, cfg)?;
        let ready = world.find(&j.name).and_then(|i| fleet::prepare(hc, &world.machines[i], &templates));
        let what = match j.found.summary() {
            s if s == "?" => String::new(),
            s => format!("  {}", ui::dim(s)),
        };
        match ready {
            Ok(_) => s.ok(format!("Added {}{what}", j.name)),
            Err(e) => s.warn(format!("Added {}, but couldn't get it ready: {e}", j.name)),
        }
        added.push(j.name);
    }
    Ok((added, failed))
}

/// Why hc wouldn't add a machine: its error's kind (`hc_missing`,
/// `incompatible`, ...) and what it said.
struct Refused {
    kind: String,
    msg: String,
}

/// `hc machine add` with no one to ask: by now ssh gets in on its own.
/// `install`: put this hc there if it has none, or one too old.
fn machine_add(j: &Joining, install: bool) -> std::result::Result<Value, Refused> {
    let mut cmd = Command::new(hc::binary());
    cmd.args(["machine", "add", "--json", "--name", &j.name]);
    if install {
        cmd.arg("--install");
    }
    match j.via {
        true => cmd.args(["--via", &j.target]),
        false => cmd.arg(&j.target),
    };
    if let Some(s) = &j.session {
        cmd.args(["--session", s]);
    }
    if j.temp {
        cmd.arg("--temp");
    }
    let out = cmd.stdin(Stdio::null()).output().map_err(|e| Refused { kind: String::new(), msg: format!("can't run hc: {e}") })?;
    if out.status.success() {
        return Ok(serde_json::from_slice(&out.stdout).unwrap_or_default());
    }
    // hc --json says {"error": {"kind", "message", "hint"}}.
    for text in [&out.stderr, &out.stdout] {
        if let Some(e) = serde_json::from_slice::<Value>(text).ok().and_then(|v| v.get("error").cloned()) {
            let (kind, msg) = (e["kind"].as_str().unwrap_or_default(), e["message"].as_str().unwrap_or("hc refused"));
            return Err(Refused { kind: kind.into(), msg: msg.into() });
        }
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let text = if err.trim().is_empty() { String::from_utf8_lossy(&out.stdout) } else { err };
    Err(Refused { kind: String::new(), msg: crate::out::tail(&text, 6) })
}

/// `add` without a terminal (or `--via`): hc's own output, as before.
fn join_plainly(hc: &mut Hc, cfg: &Config, j: Joining) -> Result<Vec<String>> {
    let mut cmd = Command::new(hc::binary());
    cmd.args(["machine", "add", "--install", "--name", &j.name]);
    match j.via {
        true => cmd.args(["--via", &j.target]),
        false => cmd.arg(&j.target),
    };
    if let Some(s) = &j.session {
        cmd.args(["--session", s]);
    }
    if j.temp {
        cmd.arg("--temp");
    }
    if !cmd.status()?.success() {
        fail!("hc machine add failed (see above)");
    }
    if config::add_machine(&j.name, j.max_agents, j.max_jobs, &j.tags)? {
        println!("{}: added to {}", j.name, config::path().display());
    }
    let world = World::discover(hc, cfg)?;
    let i = world.find(&j.name)?;
    let msg = fleet::prepare(hc, &world.machines[i], &fleet::local_templates())?;
    println!("{}: {msg}", j.name);
    Ok(vec![j.name])
}

fn words(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// `0.1.9` < `0.1.14`.
pub fn older(a: &str, b: &str) -> bool {
    let nums = |v: &str| v.split(['.', '-']).map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    nums(a) < nums(b)
}

/// What every machine (or the ones in `only`) needs to run agents well, put
/// right where it can be: git's identity, hc's version. The rest is said.
pub fn tune(hc: &mut Hc, world: &World, cfg: &Config, yes: bool, only: Option<&[String]>) -> Result<()> {
    let wanted: Vec<&Machine> =
        world.machines.iter().filter(|m| m.reachable() && only.is_none_or(|o| o.contains(&m.name))).collect();
    if wanted.is_empty() {
        return Ok(());
    }
    let s = ui::spin("Checking git and the agents on each machine");
    let reports = fleet::health_of(&wanted, cfg, false);
    s.clear();
    let mut fine = true;
    let me = fleet::git_identity();
    let nameless: Vec<&Machine> = reports.iter().filter(|(_, r)| r.as_ref().is_ok_and(|h| h.email.is_empty())).map(|(m, _)| *m).collect();
    if !nameless.is_empty() {
        fine = false;
        let names: Vec<String> = nameless.iter().map(|m| m.name.clone()).collect();
        match &me {
            None => ui::step(
                Tone::Warn,
                &format!("git has no user.email on {} (nor here): agents can't commit there", words(&names)),
                &[ui::dim("git config --global user.email you@example.com, on each")],
            ),
            Some((n, e)) => {
                let q = format!("Set git's user on {} to {n} <{e}>?", words(&names));
                if agree(yes, &q, "agents commit there, and git needs a name to", "herdr-pool doctor --fix --yes sets it")? {
                    for m in &nameless {
                        match fleet::set_identity(hc, m, n, e) {
                            Ok(()) => ui::done(&format!("git on {} commits as {n} <{e}>", m.name)),
                            Err(err) => ui::step(Tone::Bad, &format!("Couldn't set git's user on {}: {err}", m.name), &[]),
                        }
                    }
                }
            }
        }
    }
    let ours = &world.me.version;
    let behind: Vec<&Machine> = wanted.iter().copied().filter(|m| m.on.is_some() && !m.version.is_empty() && older(&m.version, ours)).collect();
    if !behind.is_empty() {
        fine = false;
        let names: Vec<String> = behind.iter().map(|m| format!("{} ({})", m.name, m.version)).collect();
        if agree(yes, &format!("Update hc on {} to {ours}?", words(&names)), "its terminals keep running", "herdr-pool doctor --fix --yes updates it")? {
            for m in behind {
                let s = ui::spin(format!("Updating hc on {}", m.name));
                let out = Command::new(hc::binary()).args(["machine", "update", "--json", &m.name]).stdin(Stdio::null()).output()?;
                match out.status.success() {
                    true => s.ok(format!("{} runs hc {ours}", m.name)),
                    false => s.fail(format!("Couldn't update hc on {}: {}", m.name, crate::out::tail(&String::from_utf8_lossy(&out.stderr), 2))),
                }
            }
        }
    }
    for (m, r) in &reports {
        match r {
            Err(e) => {
                fine = false;
                ui::step(Tone::Bad, &format!("Couldn't check {}: {e}", m.name), &[]);
            }
            Ok(h) if !h.has_git() => {
                fine = false;
                ui::step(Tone::Bad, &format!("{} has no git on its login PATH", m.name), &[ui::dim("install git there (macOS: xcode-select --install)")]);
            }
            Ok(h) if h.agents().is_empty() => {
                fine = false;
                ui::step(
                    Tone::Warn,
                    &format!("{} has no agent on its login PATH ({})", m.name, cfg.defaults.agents.iter().take(4).cloned().collect::<Vec<_>>().join(", ")),
                    &[ui::dim("install one there and log in once, e.g. curl -fsSL https://claude.ai/install.sh | bash")],
                );
            }
            Ok(_) => {}
        }
    }
    if fine {
        let agents: Vec<String> = reports
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .flat_map(|h| h.agents().into_iter().map(|(a, _)| a))
            .fold(Vec::new(), |mut v, a| {
                if !v.contains(&a) {
                    v.push(a);
                }
                v
            });
        let who = match wanted.len() {
            1 => format!("{} is ready", wanted[0].name),
            n => format!("All {n} machines are ready"),
        };
        let has = std::iter::once("git".to_string()).chain(agents).collect::<Vec<_>>().join(", ");
        ui::done(&format!("{who}  {}", ui::dim(has)));
    }
    Ok(())
}

/// The agent skill in every Claude Code folder here, current.
pub fn skill(yes: bool) -> Result<()> {
    let mut dirs = fleet::claude_dirs();
    let had_claude = !dirs.is_empty();
    if dirs.is_empty() {
        dirs.push(config::home().join(".claude/skills"));
    }
    let file = |d: &Path| d.join("herdr-pool/SKILL.md");
    let current = |d: &Path| std::fs::read_to_string(file(d)).is_ok_and(|s| s == fleet::SKILL);
    let stale: Vec<&Path> = dirs.iter().map(|d| d.as_path()).filter(|d| file(d).is_file() && !current(d)).collect();
    let missing: Vec<&Path> = dirs.iter().map(|d| d.as_path()).filter(|d| !file(d).is_file()).collect();
    if !stale.is_empty() {
        let q = format!("The agent skill changed. Update it in {}?", plural(stale.len(), "Claude Code folder"));
        if agree(yes, &q, "your copies are replaced with this version's", "herdr-pool skill install")? {
            for d in &stale {
                std::fs::write(file(d), fleet::SKILL)?;
            }
            ui::done(&format!("Updated the agent skill in {}", plural(stale.len(), "place")));
        }
    }
    if missing.is_empty() {
        if stale.is_empty() {
            ui::done("The agent skill is installed for Claude Code");
        }
        return Ok(());
    }
    let where_: Vec<String> = missing.iter().map(|d| tilde(&d.to_string_lossy())).collect();
    let q = format!("Install the herdr-pool skill for Claude Code ({})?", where_.join(", "));
    let about = "it teaches agents the pool: about 700 tokens, read only when a task calls for it";
    let go = match yes {
        true => had_claude,
        false => agree(false, &q, about, "herdr-pool skill install")?,
    };
    if !go {
        if yes || ui::can_ask() {
            ui::step(Tone::Warn, "No agent skill: herdr-pool skill install, when you want it", &[]);
        }
        return Ok(());
    }
    for d in &missing {
        std::fs::create_dir_all(d.join("herdr-pool"))?;
        std::fs::write(file(d), fleet::SKILL)?;
    }
    ui::done(&format!("Installed the agent skill in {}", where_.join(", ")));
    Ok(())
}

fn plural(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// The pool as it is now, and what to try.
fn finish(hc: &mut Hc, cfg: &Config) -> Result<i32> {
    let mut world = World::discover(hc, cfg)?;
    world.probe(|_| true);
    let up: Vec<&Machine> = world.machines.iter().filter(|m| m.reachable()).collect();
    let cores: u32 = up.iter().filter_map(|m| m.load.as_ref()).map(|l| l.ncpu).sum();
    let mem: f64 = up.iter().filter_map(|m| m.load.as_ref()).map(|l| l.mem_gb).sum();
    let width = world.machines.iter().map(|m| ui::width(&fleet::label(m, &world))).max().unwrap_or(0);
    let mut lines: Vec<String> = world
        .machines
        .iter()
        .map(|m| {
            let dot = if m.reachable() { ui::good(ui::g().dot) } else { ui::warn(ui::g().ring) };
            let about = match m.reachable() {
                true => fleet::about(m),
                false => m.state.replace('_', " "),
            };
            format!("{dot} {}  {}", ui::pad(&fleet::label(m, &world), width), ui::dim(about))
        })
        .collect();
    if cfg.defaults.trust && !fleet::claude_dirs().is_empty() {
        lines.push(String::new());
        lines.push(ui::dim("Claude Code started by the pool is told to trust its work tree, so it won't stop to ask"));
        lines.push(ui::dim("(trust = false turns that off: herdr-pool help config)"));
    }
    lines.push(String::new());
    lines.push(ui::dim("Try"));
    for t in ["herdr-pool status", "herdr-pool run claude \"Fix the flaky test in src/auth.ts\"", "herdr-pool test -- npx vitest run"] {
        lines.push(format!("  {}", ui::accent(t)));
    }
    let size = match (cores, mem) {
        (0, _) => String::new(),
        (c, m) if m > 0.0 => format!(" · {c} cores · {m:.0} GB"),
        (c, _) => format!(" · {c} cores"),
    };
    let count = match up.len() == world.machines.len() {
        true => plural(up.len(), "machine"),
        false => format!("{} of {} machines up", up.len(), world.machines.len()),
    };
    ui::step(Tone::Good, &format!("Your pool: {count}{size}"), &lines);
    ui::outro(&format!("Ready. Agents learn the pool from the skill; you, from {}", ui::bold("herdr-pool help")));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_shortened_but_addresses_kept_whole() {
        assert_eq!(short_host("me@studio.local"), "studio");
        assert_eq!(short_host("Studio"), "studio");
        assert_eq!(short_host("100.101.102.103"), "100.101.102.103");
        assert_eq!(short_host("me@192.168.1.20"), "192.168.1.20");
        assert_eq!(short_host("[fe80::1]"), "fe80::1");
        assert_ne!(short_host("100.101.1.1"), short_host("100.64.0.9"));
    }

    #[test]
    fn versions_compare_by_number() {
        assert!(older("0.1.9", "0.1.14"));
        assert!(!older("0.1.14", "0.1.14"));
        assert!(!older("0.2.0", "0.1.14"));
    }

    #[test]
    fn names_stay_unique() {
        let taken = vec!["studio".to_string(), "studio-2".to_string()];
        assert_eq!(unique("studio", &taken), "studio-3");
        assert_eq!(unique("mini", &taken), "mini");
        assert_eq!(unique("local", &taken), "local-2");
    }

    #[test]
    fn lists_read_as_words() {
        assert_eq!(words(&["a".into()]), "a");
        assert_eq!(words(&["a".into(), "b".into(), "c".into()]), "a, b and c");
    }
}
