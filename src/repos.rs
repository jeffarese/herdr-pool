//! What the pool keeps of your repositories on each machine, and deleting
//! it. A copy is a bare mirror with the repository's history (every branch
//! of origin where the machine can fetch it, else what your snapshots
//! needed) and the work trees jobs ran in, dependencies and build output
//! included. `rm` and `gc` give trees back but keep them; `repos rm` deletes
//! the lot.

use crate::args;
use crate::config;
use crate::error::{Error, Result};
use crate::fleet;
use crate::git::{self, Repo};
use crate::hc::Hc;
use crate::jobs::{self, GaveBack};
use crate::out::{self, bytes, plural};
use crate::pool::{self, Machine, World, JOB};
use crate::sync::{self, Deleted, Lease};
use crate::ui::{self, Tone};
use serde_json::json;
use std::collections::BTreeSet;
use std::path::Path;

const HELP: &str = "herdr-pool repos [--on M,N] [--json]
herdr-pool repos rm [repo...] [--all] [--on M,N] [--force] [--agent-logs]

What the pool keeps of your repositories on each machine: a mirror with
their history (every branch of origin, where a machine can fetch it) and
the work trees jobs ran in, dependencies included. rm and gc keep both
(gc --purge deletes free trees); repos rm deletes it all.

repos rm deletes this repository's copy (or those named: a path, or a name
as repos lists it) on every machine, or on the --on ones; --all deletes
every repository's. A copy stays where a job still runs in it or holds
work you haven't pulled (--force drops that work). Claude Code's session
logs of agents that worked in it stay unless --agent-logs.";

pub fn repos(argv: &[String]) -> Result<i32> {
    match argv.first().map(String::as_str) {
        Some("rm") => rm(&argv[1..]),
        Some("ls") => list(&argv[1..]),
        _ => list(argv),
    }
}

/// One repository's copy on one machine.
struct Copy {
    key: String,
    mirror: u64,
    trees: u64,
    tree_count: usize,
    /// Jobs holding one of its trees.
    jobs: Vec<String>,
    /// Last used, epoch ms (0: can't tell).
    used: u64,
    origin: String,
}

/// What a machine holds: its copies, and every repository its lease
/// document names (an entry can outlive its files).
struct Held {
    copies: Vec<Copy>,
    leased: BTreeSet<String>,
}

impl Held {
    fn keys(&self) -> BTreeSet<String> {
        self.copies.iter().map(|c| c.key.clone()).chain(self.leased.iter().cloned()).collect()
    }
}

/// A repository key as the pool makes them (`owner-repo`, `folder-1a2b3c4`):
/// nothing that could reach outside its folder.
fn is_key(k: &str) -> bool {
    !k.is_empty()
        && !k.starts_with(['.', '-'])
        && !k.contains("..")
        && k.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn parse_copies(text: &str) -> Vec<Copy> {
    let n = |s: &str| s.trim().parse::<u64>().unwrap_or(0);
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.splitn(6, '\t').collect();
            if f.len() < 5 || !is_key(f[0]) {
                return None;
            }
            Some(Copy {
                key: f[0].into(),
                mirror: n(f[1]) * 1024,
                trees: n(f[2]) * 1024,
                tree_count: n(f[3]) as usize,
                jobs: Vec::new(),
                used: n(f[4]) * 1000,
                origin: f.get(5).map(|o| o.trim().to_string()).unwrap_or_default(),
            })
        })
        .collect()
}

/// What each machine holds, all asked at once; for one out of reach, why.
fn survey<'w>(machines: &[&'w Machine]) -> Vec<(&'w Machine, Result<Held>)> {
    std::thread::scope(|s| {
        let handles: Vec<_> = machines.iter().map(|m| (*m, s.spawn(move || held_on(m)))).collect();
        handles.into_iter().map(|(m, h)| (m, h.join().unwrap_or_else(|_| Err(Error::new("looking there failed"))))).collect()
    })
}

fn held_on(m: &Machine) -> Result<Held> {
    if !m.reachable() {
        return Err(Error::new(match m.state.as_str() {
            "unreachable" => "out of reach".into(),
            state => state.replace('_', " "),
        }));
    }
    let mut hc = Hc::connect()?;
    let env = pool::env_for(&mut hc, m);
    let mut copies = parse_copies(&sync::copies(&mut hc, m, &env)?);
    let (doc, _) = sync::leases(&mut hc, m)?;
    for c in &mut copies {
        c.jobs = doc
            .trees
            .get(&c.key)
            .into_iter()
            .flatten()
            .filter(|(slot, _)| slot.as_str() != sync::CLOSED)
            .filter_map(|(_, l)| l.as_ref().filter(|l| l.kind != "cleanup"))
            .map(|l| l.job.clone())
            .collect();
    }
    let leased = doc.trees.keys().filter(|k| is_key(k)).cloned().collect();
    Ok(Held { copies, leased })
}

fn list(argv: &[String]) -> Result<i32> {
    let a = args::parse(argv, &["json"], &["on"], false)?;
    if a.flag("help") {
        println!("{HELP}");
        return Ok(0);
    }
    let cfg = config::load()?;
    let mut hc = Hc::connect()?;
    let world = World::discover(&mut hc, &cfg)?;
    let machines = fleet::wanted(&world, &a)?;
    let here = Repo::find(&std::env::current_dir()?).map(|r| r.key);
    let s = ui::spin(format!("Looking at {}", plural(machines.len(), "machine")));
    let found = survey(&machines);
    s.clear();
    if a.flag("json") {
        let (mut copies, mut away) = (Vec::new(), Vec::new());
        for (m, held) in &found {
            match held {
                Ok(h) => copies.extend(h.copies.iter().map(|c| {
                    json!({
                        "machine": m.name, "repo": c.key, "bytes": c.mirror + c.trees, "mirror_bytes": c.mirror,
                        "trees_bytes": c.trees, "trees": c.tree_count, "jobs": c.jobs, "used": c.used,
                        "origin": c.origin, "here": here.as_deref() == Some(c.key.as_str()),
                    })
                })),
                Err(e) => away.push(json!({"machine": m.name, "why": e.msg})),
            }
        }
        println!("{}", json!({"copies": copies, "unreachable": away}));
        return Ok(0);
    }
    if !ui::pretty() {
        let mut any = false;
        for (m, held) in &found {
            match held {
                Ok(h) => {
                    for c in &h.copies {
                        any = true;
                        let jobs = if c.jobs.is_empty() { String::new() } else { format!("  {}", plural(c.jobs.len(), "job")) };
                        let used = if c.used == 0 { "?".into() } else { format!("{} ago", out::ago(c.used)) };
                        println!(
                            "{}  {}  {}  {}{jobs}  used {used}  {}",
                            m.name,
                            c.key,
                            bytes(c.mirror + c.trees),
                            plural(c.tree_count, "tree"),
                            if c.origin.is_empty() { "no origin" } else { &c.origin }
                        );
                    }
                }
                Err(e) => println!("{}  {}: can't look", m.name, e.msg),
            }
        }
        if !any {
            println!("no repository copies on the pool's machines");
        }
        return Ok(0);
    }
    pretty_list(&found, here.as_deref());
    Ok(0)
}

/// A block per machine, one table across them all so the columns line up.
fn pretty_list(found: &[(&Machine, Result<Held>)], here: Option<&str>) {
    let g = ui::g();
    let cols = ui::columns();
    let mut rows = Vec::new();
    for (_, held) in found {
        for c in held.iter().flat_map(|h| &h.copies) {
            let mine = here == Some(c.key.as_str());
            let origin = ui::dim(if c.origin.is_empty() { "no origin".into() } else { home(&c.origin) });
            rows.push(vec![
                if mine { ui::bold(&c.key) } else { c.key.clone() },
                bytes(c.mirror + c.trees),
                ui::dim(plural(c.tree_count, "tree")),
                if c.jobs.is_empty() { String::new() } else { ui::accent(plural(c.jobs.len(), "job")) },
                ui::dim(if c.used == 0 { "?".into() } else { format!("{} ago", out::ago(c.used)) }),
                if mine { format!("{origin}  {}", ui::accent("this repository")) } else { origin },
            ]);
        }
    }
    let table = ui::Table { rows, right: vec![1], flex: Some(5), indent: 6 }.render(cols.saturating_sub(1));
    let mut lines = table.into_iter();
    let name_w = found.iter().map(|(m, _)| ui::width(&m.name)).max().unwrap_or(5);
    let (mut total, mut count, mut on) = (0, 0, 0);
    println!();
    for (m, held) in found {
        let name = ui::pad(&ui::bold(&m.name), name_w);
        let mut about = Vec::new();
        if m.on.is_none() {
            about.push(format!("this machine ({})", m.host));
        }
        match held {
            Ok(h) => {
                let size: u64 = h.copies.iter().map(|c| c.mirror + c.trees).sum();
                about.push(match h.copies.len() {
                    0 => "nothing kept".into(),
                    n => format!("{} {} {}", plural(n, "repo"), g.sep, bytes(size)),
                });
                println!("  {} {name}  {}", ui::good(g.dot), ui::dim(about.join(&format!(" {} ", g.sep))));
                for _ in &h.copies {
                    if let Some(l) = lines.next() {
                        println!("{l}");
                    }
                }
                if !h.copies.is_empty() {
                    (total, count, on) = (total + size, count + h.copies.len(), on + 1);
                }
            }
            Err(e) => {
                about.push(ui::bad(&e.msg));
                println!("  {} {name}  {}", ui::bad(g.ring), ui::dim(about.join(&format!(" {} ", g.sep))));
                println!("      {}", ui::dim("its copies can't be listed until it is back"));
            }
        }
    }
    println!();
    if count == 0 {
        println!("  {}", ui::dim("No repository is copied to any machine yet."));
        return;
    }
    let copies = if count == 1 { "1 copy".to_string() } else { format!("{count} copies") };
    println!("  {}", ui::dim(format!("{copies} {} {} on {}", g.sep, bytes(total), plural(on, "machine"))));
    println!(
        "  {} {}  {}",
        ui::accent(g.arrow),
        ui::accent("herdr-pool repos rm [repo]"),
        ui::dim("deletes a repository's copies (--all: every one)")
    );
}

/// `/Users/me/x` as `~/x`.
fn home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && path.starts_with(&format!("{h}/")) => format!("~{}", &path[h.len()..]),
        _ => path.into(),
    }
}

fn rm(argv: &[String]) -> Result<i32> {
    let a = args::parse(argv, &["all", "force", "agent-logs"], &["on"], false)?;
    if a.flag("help") {
        println!("{HELP}");
        return Ok(0);
    }
    let (all, force, logs) = (a.flag("all"), a.flag("force"), a.flag("agent-logs"));
    if all && !a.rest.is_empty() {
        return Err(Error::new("name repositories, or --all, not both"));
    }
    let cfg = config::load()?;
    let mut hc = Hc::connect()?;
    let world = World::discover(&mut hc, &cfg)?;
    let machines = fleet::wanted(&world, &a)?;
    let here = Repo::find(&std::env::current_dir()?);
    let s = ui::spin(format!("Looking at {}", plural(machines.len(), "machine")));
    let found = survey(&machines);
    s.clear();
    let known: BTreeSet<String> = found.iter().filter_map(|(_, h)| h.as_ref().ok()).flat_map(Held::keys).collect();
    let keys = match all {
        true => None,
        false => Some(resolve(&a.rest, here.as_ref(), &known, logs)?),
    };
    let (mut code, mut acted, mut freed, mut deleted) = (0, false, 0, 0);
    // Session logs that stay: machine, repository (None: any, after --all), path.
    let mut left: Vec<(String, Option<String>, String)> = Vec::new();
    for (m, held) in &found {
        let h = match held {
            Ok(h) => h,
            Err(e) => {
                code = 1;
                line(Tone::Warn, format!("{}: {}, its copies stay until you run this again", m.name, e.msg), format!("{}: {}, its copies stay", m.name, e.msg));
                continue;
            }
        };
        let there = h.keys();
        let todo: Vec<String> = match &keys {
            None => there.iter().cloned().collect(),
            Some(ks) => ks.iter().filter(|k| logs || there.contains(*k)).cloned().collect(),
        };
        let mut whole = true;
        for key in &todo {
            let copy = h.copies.iter().find(|c| &c.key == key);
            let s = ui::spin(format!("Deleting {key} on {}", m.name));
            match delete_on(&mut hc, &world, m, key, force, logs, here.as_ref()) {
                Done::Deleted(d) => {
                    left.extend(d.logs.iter().map(|p| (m.name.clone(), Some(key.clone()), p.clone())));
                    if !there.contains(key) && d.logs_deleted.is_empty() {
                        s.clear(); // nothing of it there
                        continue;
                    }
                    acted = true;
                    freed += d.bytes;
                    deleted += usize::from(there.contains(key));
                    deleted_line(s, &m.name, key, copy, &d);
                }
                Done::Kept(holds) => {
                    (code, whole) = (1, false);
                    acted = true;
                    s.clear();
                    let plain = format!("{}: kept {key}: {}", m.name, holds.join("; "));
                    match ui::live() {
                        true => ui::step(Tone::Warn, &format!("Kept {key} on {}", m.name), &holds.iter().map(ui::dim).collect::<Vec<_>>()),
                        false => println!("{plain}"),
                    }
                }
                Done::Failed(e) => {
                    (code, whole) = (1, false);
                    acted = true;
                    match ui::live() {
                        true => s.fail(format!("Couldn't delete {key} on {}: {e}", m.name)),
                        false => {
                            s.clear();
                            println!("{}: couldn't delete {key}: {e}", m.name);
                        }
                    }
                }
            }
        }
        // Every repository's copy went: the pool's folder there goes too.
        if all && whole {
            let env = pool::env_for(&mut hc, m);
            match sync::sweep(&mut hc, m, logs, &env) {
                Ok(d) => {
                    left.extend(d.logs.iter().map(|p| (m.name.clone(), None, p.clone())));
                    if !d.logs_deleted.is_empty() {
                        acted = true;
                        let n = plural(d.logs_deleted.len(), "Claude Code session log");
                        line(Tone::Good, format!("Deleted {n} on {}", m.name), format!("{}: deleted {n}", m.name));
                    }
                }
                Err(e) => {
                    code = 1;
                    line(Tone::Bad, format!("Couldn't clear the pool's folder on {}: {e}", m.name), format!("{}: couldn't clear the pool's folder: {e}", m.name));
                }
            }
        }
    }
    if !acted && code == 0 {
        let what = match &keys {
            Some(k) => format!("copies of {} on the pool's machines", k.join(", ")),
            None => "repository copies on the pool's machines".into(),
        };
        line(Tone::Good, format!("No {what}"), format!("no {what}"));
    } else if deleted > 1 && ui::live() {
        ui::done(&format!("Freed {} in all", bytes(freed)));
    }
    left.sort();
    left.dedup_by(|x, y| x.2 == y.2);
    if !left.is_empty() {
        // What deletes them, the copies being gone: the keys, as given.
        let again = match left.iter().any(|(_, k, _)| k.is_none()) {
            true => "herdr-pool repos rm --all --agent-logs".to_string(),
            false => {
                let keys: BTreeSet<&str> = left.iter().filter_map(|(_, k, _)| k.as_deref()).collect();
                format!("herdr-pool repos rm {} --agent-logs", keys.into_iter().collect::<Vec<_>>().join(" "))
            }
        };
        match ui::live() {
            true => {
                let mut by: Vec<(String, usize)> = Vec::new();
                for (m, _, _) in &left {
                    match by.iter_mut().find(|(n, _)| n == m) {
                        Some((_, c)) => *c += 1,
                        None => by.push((m.clone(), 1)),
                    }
                }
                let mut under: Vec<String> = by.iter().map(|(m, n)| ui::dim(format!("{m}: {}", plural(*n, "log")))).collect();
                under.push(format!("{} {}  {}", ui::accent(ui::g().arrow), ui::accent(&again), ui::dim("deletes them")));
                ui::step(Tone::Warn, "Claude Code's session logs of agents that worked there stay", &under);
            }
            false => {
                for (m, _, p) in &left {
                    println!("{m}: Claude Code session log stays: {p} ({again} deletes it)");
                }
            }
        }
    }
    Ok(code)
}

/// A line with no spinner before it: marked for people, plain for programs.
fn line(tone: Tone, pretty: String, plain: String) {
    match ui::live() {
        true => ui::step(tone, &pretty, &[]),
        false => println!("{plain}"),
    }
}

/// The line for a copy deleted, or only its agents' session logs.
fn deleted_line(s: ui::Spin, machine: &str, key: &str, copy: Option<&Copy>, d: &Deleted) {
    let logs = (!d.logs_deleted.is_empty()).then(|| plural(d.logs_deleted.len(), "agent session log"));
    let (pretty, plain) = match (copy.is_some() || d.bytes > 0, logs) {
        (false, Some(l)) => (format!("Deleted {l} of {key} on {machine}"), format!("{machine}: deleted {l} of {key}")),
        (_, logs) => {
            let mut what = vec![bytes(d.bytes)];
            if let Some(c) = copy.filter(|c| c.tree_count > 0) {
                what.push(plural(c.tree_count, "tree"));
            }
            what.extend(logs);
            let pretty = format!("Deleted {key} on {machine}  {}", ui::dim(what.join(&format!(" {} ", ui::g().sep))));
            (pretty, format!("{machine}: deleted {key} ({})", what.join(", ")))
        }
    };
    match ui::live() {
        true => s.ok(pretty),
        false => {
            s.clear();
            println!("{plain}");
        }
    }
}

/// The repositories named (a path to one, a key as `repos` lists it, or
/// just its name), or this one.
fn resolve(names: &[String], here: Option<&Repo>, known: &BTreeSet<String>, logs: bool) -> Result<Vec<String>> {
    if names.is_empty() {
        return match here {
            Some(r) => Ok(vec![r.key.clone()]),
            None => Err(Error::new("not in a git repository: name the repositories (herdr-pool repos lists them), or --all")),
        };
    }
    let mut keys = Vec::new();
    for name in names {
        let path = Path::new(name);
        let key = match path.is_dir().then(|| Repo::find(path)).flatten() {
            Some(r) => r.key,
            None => match known_key(name, known) {
                Ok(k) => k,
                // Its copies can be gone already: a key as given still finds its logs.
                Err(_) if logs && is_key(name) => name.clone(),
                Err(e) => return Err(e),
            },
        };
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    Ok(keys)
}

fn known_key(name: &str, known: &BTreeSet<String>) -> Result<String> {
    if known.contains(name) {
        return Ok(name.into());
    }
    let s = git::slug(&name.replace('/', "-"));
    if known.contains(&s) {
        return Ok(s);
    }
    let near: Vec<&str> =
        known.iter().filter(|k| k.ends_with(&format!("-{s}")) || k.starts_with(&format!("{s}-"))).map(String::as_str).collect();
    match near.as_slice() {
        [k] => Ok(k.to_string()),
        [] => Err(Error::new(format!("no copies of {name:?} on the pool's machines (herdr-pool repos lists them)"))),
        many => Err(Error::new(format!("{name:?} could be {}: name one", many.join(", ")))),
    }
}

enum Done {
    Deleted(Deleted),
    /// What still needs it there.
    Kept(Vec<String>),
    Failed(String),
}

/// This run's mark on a repository it deletes (`ls` leaves cleanups out).
fn mark(by: &str) -> Lease {
    Lease {
        job: format!("-repos-rm-{}", std::process::id()),
        kind: "cleanup".into(),
        terminal: None,
        base: String::new(),
        since: crate::state::now_ms(),
        by: by.into(),
        pulled: None,
    }
}

/// Deletes `key`'s copy on `m`. Closed first, so no run starts a tree of
/// it meanwhile; trees of jobs that ended go back as gc would give them
/// back; a job still at work, or work not pulled (without `force`), keeps
/// the copy.
fn delete_on(hc: &mut Hc, world: &World, m: &Machine, key: &str, force: bool, logs: bool, here: Option<&Repo>) -> Done {
    if !is_key(key) {
        return Done::Failed(format!("{key:?} isn't a repository key"));
    }
    let env = pool::env_for(hc, m);
    let mark = mark(&world.me.id);
    match sync::close(hc, m, key, &mark) {
        Ok(true) => {}
        Ok(false) => return Done::Kept(vec!["another repos rm is deleting it there right now".into()]),
        Err(e) => return Done::Failed(e.msg),
    }
    let doc = match sync::leases(hc, m) {
        Ok((doc, _)) => doc,
        Err(e) => {
            let _ = sync::reopen(hc, m, key, &mark);
            return Done::Failed(e.msg);
        }
    };
    let (mut freed, mut holds) = (Vec::new(), Vec::new());
    for (slot, lease) in doc.trees.get(key).into_iter().flatten() {
        let (Ok(slot), Some(l)) = (slot.parse::<u32>(), lease) else { continue };
        match jobs::give_back(hc, world, m, key, slot, l, force, false, &env, here) {
            GaveBack::Freed(job) => freed.push(job),
            GaveBack::Busy(job) => holds.push(format!("{job} is still at work there (herdr-pool rm {job} ends it)")),
            GaveBack::Kept(job) => holds.push(format!("{job} has work you haven't pulled (herdr-pool pull {job}, or --force drops it)")),
            GaveBack::Unsure(job, why) => holds.push(format!("{job}: couldn't tell whether its work was pulled ({why}); --force drops it")),
        }
    }
    if !holds.is_empty() {
        let _ = sync::reopen(hc, m, key, &mark);
        return Done::Kept(holds);
    }
    match sync::delete_copy(hc, m, key, logs, &env) {
        Ok(d) => {
            let _ = sync::forget_copy(hc, m, key, &mark);
            crate::trust::forget(m, key);
            // Their finished terminals go too, as gc takes them.
            for t in world.terminals.iter().filter(|t| !pool::is_live(t)) {
                let on_m = world.machine_of(t["address"].as_str().unwrap_or_default()).is_some_and(|x| x.name == m.name);
                if on_m && pool::label(t, JOB).is_some_and(|j| freed.iter().any(|f| f == j)) {
                    let _ = hc.call("terminal.rm", json!({"terminal": t["address"]}));
                }
            }
            Done::Deleted(d)
        }
        Err(e) => {
            let _ = sync::reopen(hc, m, key, &mark);
            Done::Failed(e.msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_stay_inside_their_folder() {
        for k in ["owner-repo", "repo-1a2b3c4", "my_repo.v2"] {
            assert!(is_key(k), "{k}");
        }
        for k in ["", ".", "..", "../x", "a/b", ".hidden", "-x", "a..b", "a b"] {
            assert!(!is_key(k), "{k}");
        }
    }

    #[test]
    fn names_find_their_copy() {
        let known: BTreeSet<String> = ["acme-api", "acme-web", "notes-1a2b3c4"].iter().map(|s| s.to_string()).collect();
        assert_eq!(known_key("acme/api", &known).unwrap(), "acme-api");
        assert_eq!(known_key("api", &known).unwrap(), "acme-api");
        assert_eq!(known_key("notes", &known).unwrap(), "notes-1a2b3c4");
        assert!(known_key("acme", &known).unwrap_err().msg.contains("could be"));
        assert!(known_key("nope", &known).is_err());
    }

    #[test]
    fn listed_copies_are_read() {
        let copies = parse_copies("owner-repo\t10\t20\t2\t1700000000\tgit@h:o/r.git\n../bad\t1\t1\t1\t1\t\nlocal-1\t1\t0\t0\t0\t\n");
        assert_eq!(copies.len(), 2);
        assert_eq!((copies[0].mirror, copies[0].trees, copies[0].tree_count), (10240, 20480, 2));
        assert_eq!((copies[0].used, copies[0].origin.as_str()), (1_700_000_000_000, "git@h:o/r.git"));
        assert_eq!(copies[1].origin, "");
    }
}
