//! The pool: this machine and every machine hc has added, what each is doing
//! right now, and where the next piece of work should go.

use crate::config::{Config, Defaults, MachineConf};
use crate::error::{Error, Result};
use crate::hc::{Env, Hc};
use serde_json::{json, Value};

pub const LOCAL: &str = "local";

/// Labels on every terminal the pool starts: hc keeps them, so the pool needs
/// no registry of its own for what runs where.
pub const JOB: &str = "herdr-pool";
pub const KIND: &str = "herdr-pool.kind";
pub const TREE: &str = "herdr-pool.tree";
pub const BASE: &str = "herdr-pool.base";
pub const RUN: &str = "herdr-pool.run";
/// The short-lived terminal `reply_address` starts to learn a machine's name for us.
pub const PROBE_LABEL: &str = "herdr-pool.probe";

pub struct Me {
    pub id: String,
    pub name: String,
    pub version: String,
}

pub struct Machine {
    /// `local`, or what hc calls it (`studio` in `studio/t7`).
    pub name: String,
    /// What to pass as hc's `on`: None for this machine.
    pub on: Option<String>,
    pub id: String,
    pub host: String,
    /// How ssh reaches it (None: this machine, or one hc reaches `--via` a command).
    pub ssh: Option<String>,
    /// connected, connecting, needs_login or unreachable.
    pub state: String,
    pub why: Option<String>,
    pub version: String,
    pub conf: MachineConf,
    pub load: Option<Load>,
    pub agents: u32,
    pub jobs: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Load {
    pub ncpu: u32,
    pub load1: f64,
    /// Percent of memory free (macOS: memorystatus level; Linux: MemAvailable).
    pub mem_free: u32,
    /// All of it, in GB (0 when the machine didn't say).
    pub mem_gb: f64,
    /// `Apple M2 Max`, or empty.
    pub model: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Mostly waits on a model: limited by count and memory.
    Agent,
    /// Commands and test shards: limited by CPU.
    Job,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Agent => "agent",
            Kind::Job => "cmd",
        }
    }
}

pub struct World {
    pub me: Me,
    pub machines: Vec<Machine>,
    /// Every terminal here and on connected machines, finished ones too.
    pub terminals: Vec<Value>,
}

/// `studio/t7` -> (`studio`, `t7`); `t7` -> (`local`, `t7`).
pub fn split_address(address: &str) -> (&str, &str) {
    address.split_once('/').unwrap_or((LOCAL, address))
}

/// `t7` on `m` as our core reaches it: `studio/t7`, or `t7` here.
pub fn address_on(m: &Machine, local: &str) -> String {
    match &m.on {
        Some(on) => format!("{on}/{local}"),
        None => local.into(),
    }
}

pub fn label<'a>(t: &'a Value, key: &str) -> Option<&'a str> {
    t["labels"][key].as_str()
}

/// Whether terminal `t` is `job`'s: its own, or a shard run by that lane of a test.
pub fn belongs(t: &Value, job: &str) -> bool {
    label(t, JOB).is_some_and(|j| j == job || job.strip_prefix(j).is_some_and(|rest| rest.starts_with("-lane")))
}

pub fn is_live(t: &Value) -> bool {
    t["state"] == "live"
}

/// The word an agent last reported, else the terminal's state.
pub fn word(t: &Value) -> String {
    if !is_live(t) {
        return match (t["exit_code"].as_i64(), t["reason"].as_str()) {
            (Some(0), _) => "exited".into(),
            (Some(c), _) => format!("exit {c}"),
            (None, Some(r)) => r.into(),
            (None, None) => "finished".into(),
        };
    }
    t["status"]["state"].as_str().unwrap_or("running").into()
}

impl World {
    pub fn discover(hc: &mut Hc, cfg: &Config) -> Result<World> {
        let status = hc.call("server.status", json!({}))?;
        let me = Me {
            id: status["core"]["id"].as_str().unwrap_or_default().into(),
            name: status["core"]["name"].as_str().unwrap_or_default().into(),
            version: status["version"].as_str().unwrap_or_default().into(),
        };
        let list = hc.call("terminal.list", json!({"all": true}))?;
        let terminals = list["terminals"].as_array().cloned().unwrap_or_default();
        let mut local = cfg.machine(LOCAL);
        local.disabled |= !cfg.defaults.local;
        let mut machines = vec![Machine {
            name: LOCAL.into(),
            on: None,
            id: me.id.clone(),
            host: me.name.clone(),
            ssh: None,
            state: "connected".into(),
            why: None,
            version: me.version.clone(),
            conf: local,
            load: None,
            agents: 0,
            jobs: 0,
        }];
        for m in list["machines"].as_array().into_iter().flatten() {
            let name = m["name"].as_str().unwrap_or_default().to_string();
            machines.push(Machine {
                on: Some(name.clone()),
                id: m["id"].as_str().unwrap_or_default().into(),
                host: m["host"].as_str().unwrap_or_default().into(),
                ssh: m["target"].as_str().filter(|_| m["via"] != true).map(String::from),
                state: m["state"].as_str().unwrap_or("unknown").into(),
                why: m["why"].as_str().map(String::from),
                version: m["version"].as_str().unwrap_or_default().into(),
                conf: cfg.machine(&name),
                name,
                load: None,
                agents: 0,
                jobs: 0,
            });
        }
        let mut world = World { me, machines, terminals };
        world.count();
        Ok(world)
    }

    /// What each machine runs now: pool work by its kind, and anyone's agent
    /// that is working (yours, started by hand, share the machine too).
    fn count(&mut self) {
        for t in self.terminals.iter().filter(|t| is_live(t)) {
            let (machine, _) = split_address(t["address"].as_str().unwrap_or_default());
            let Some(m) = self.machines.iter_mut().find(|m| m.name == machine) else { continue };
            match label(t, JOB) {
                Some(_) if label(t, KIND) == Some("agent") => m.agents += 1,
                Some(_) => m.jobs += 1,
                None if t["status"]["state"] == "working" => m.agents += 1,
                None => {}
            }
        }
    }

    pub fn find(&self, name: &str) -> Result<usize> {
        let found = self.machines.iter().position(|m| m.name == name || (m.on.is_none() && name == self.me.name));
        found.ok_or_else(|| {
            let names: Vec<&str> = self.machines.iter().map(|m| m.name.as_str()).collect();
            Error::new(format!("no machine {name:?} (the pool has: {})", names.join(", ")))
        })
    }

    pub fn machine_of(&self, address: &str) -> Option<&Machine> {
        let (name, _) = split_address(address);
        self.machines.iter().find(|m| m.name == name)
    }

    /// Ask every machine `pick` keeps how loaded it is, all at once.
    pub fn probe(&mut self, pick: impl Fn(&Machine) -> bool) {
        let loads: Vec<(usize, Option<Load>)> = std::thread::scope(|s| {
            let handles: Vec<_> = self
                .machines
                .iter()
                .enumerate()
                .filter(|(_, m)| m.reachable() && pick(m))
                .map(|(i, m)| {
                    let on = m.on.clone();
                    (i, s.spawn(move || probe(on.as_deref())))
                })
                .collect();
            handles.into_iter().map(|(i, h)| (i, h.join().ok().flatten())).collect()
        });
        for (i, load) in loads {
            self.machines[i].load = load;
        }
    }

    /// The best machine for one more `kind`, among those with room.
    pub fn pick(&self, kind: Kind, needs: &[String], d: &Defaults) -> Result<usize> {
        match self.spread(kind, needs, 1, d).first() {
            Some(i) => Ok(*i),
            None => Err(Error::new(self.why_full(kind, needs, d))),
        }
    }

    /// Up to `n` places for `kind`, best first, a machine again only while it
    /// has room: the lanes a sharded run spreads over.
    pub fn spread(&self, kind: Kind, needs: &[String], n: usize, d: &Defaults) -> Vec<usize> {
        let mut extra = vec![0u32; self.machines.len()];
        let mut lanes = Vec::new();
        while lanes.len() < n {
            let best = self
                .machines
                .iter()
                .enumerate()
                .filter(|(i, m)| m.eligible(needs) && m.used(kind) + extra[*i] < m.cap(kind, d))
                .map(|(i, m)| (i, m.score(kind, d, extra[i])))
                .min_by(|a, b| a.1.total_cmp(&b.1));
            let Some((i, _)) = best else { break };
            extra[i] += 1;
            lanes.push(i);
        }
        lanes
    }

    fn why_full(&self, kind: Kind, needs: &[String], d: &Defaults) -> String {
        let what = match kind {
            Kind::Agent => "agents",
            Kind::Job => "jobs",
        };
        let rows: Vec<String> = self
            .machines
            .iter()
            .map(|m| {
                let state = match () {
                    _ if m.conf.disabled => "disabled".to_string(),
                    _ if !m.reachable() => m.state.clone(),
                    _ if !needs.iter().all(|t| m.conf.tags.contains(t)) => "lacks a tag".into(),
                    _ => format!("{what} {}/{}", m.used(kind), m.cap(kind, d)),
                };
                format!("{} {state}", m.name)
            })
            .collect();
        format!(
            "no machine has room ({}). Free some (herdr-pool ls; herdr-pool rm <job>), raise max_{what} in the config, or wait with --wait-slot",
            rows.join(", ")
        )
    }
}

impl Machine {
    pub fn reachable(&self) -> bool {
        self.on.is_none() || self.state == "connected"
    }

    pub fn eligible(&self, needs: &[String]) -> bool {
        self.reachable() && !self.conf.disabled && needs.iter().all(|t| self.conf.tags.contains(t))
    }

    pub fn cap(&self, kind: Kind, d: &Defaults) -> u32 {
        match kind {
            Kind::Agent => self.conf.max_agents.unwrap_or(d.max_agents),
            Kind::Job => self.conf.max_jobs.unwrap_or(d.max_jobs),
        }
    }

    pub fn used(&self, kind: Kind) -> u32 {
        match kind {
            Kind::Agent => self.agents,
            Kind::Job => self.jobs,
        }
    }

    /// Lower is better: how full it is against its cap, how busy its CPUs and
    /// memory are, favouring this machine a little (nothing to copy) and
    /// following the configured weight.
    pub fn score(&self, kind: Kind, d: &Defaults, extra: u32) -> f64 {
        let cpu = self.load.as_ref().map_or(0.5, |l| l.load1 / f64::from(l.ncpu.max(1)));
        let mem = self.load.as_ref().map_or(0.5, |l| f64::from(100 - l.mem_free.min(100)) / 100.0);
        let fill = f64::from(self.used(kind) + extra) / f64::from(self.cap(kind, d).max(1));
        let raw = match kind {
            Kind::Agent => fill + 0.5 * cpu + 0.3 * mem,
            Kind::Job => cpu + 0.5 * fill + 0.3 * mem,
        };
        let raw = if self.on.is_none() { (raw - 0.05).max(0.0) } else { raw };
        raw / self.conf.weight.unwrap_or(1.0).max(0.01)
    }

    pub fn root(&self) -> &str {
        self.conf.root.as_deref().unwrap_or("~/.herdr-pool")
    }
}

/// One line, about 20 ms: CPUs, 1-minute load, percent of memory free, then
/// all memory in bytes and the chip (which has spaces, so it comes last).
const PROBE: &str = r#"PATH="$PATH:/usr/sbin:/sbin"
case "$(uname -s)" in
Darwin) echo "$(sysctl -n hw.ncpu) $(sysctl -n vm.loadavg | awk '{print $2}') $(sysctl -n kern.memorystatus_level 2>/dev/null || echo 50) $(sysctl -n hw.memsize 2>/dev/null || echo 0) $(sysctl -n machdep.cpu.brand_string 2>/dev/null)" ;;
Linux) echo "$(nproc) $(cut -d' ' -f1 /proc/loadavg) $(awk '/^MemTotal/{t=$2}/^MemAvailable/{a=$2}END{if(t)printf "%d %d",a*100/t,t*1024; else print 50, 0}' /proc/meminfo) $(awk -F': *' '/^model name/{print $2; exit}' /proc/cpuinfo 2>/dev/null)" ;;
*) echo "1 0 50" ;;
esac"#;

fn probe(on: Option<&str>) -> Option<Load> {
    let mut hc = Hc::connect().ok()?;
    let out = hc.sh(on, PROBE, &[], &Env::new(), 15.0).ok().filter(|o| o.ok())?;
    parse_load(&out.text())
}

fn parse_load(text: &str) -> Option<Load> {
    let mut it = text.split_whitespace();
    Some(Load {
        ncpu: it.next()?.parse().ok()?,
        load1: it.next()?.parse().ok()?,
        mem_free: it.next()?.parse().ok()?,
        mem_gb: it.next().and_then(|b| b.parse::<f64>().ok()).map_or(0.0, |b| b / 1_073_741_824.0),
        model: it.collect::<Vec<_>>().join(" "),
    })
}

/// The PATH a person gets in a login shell there. A core started over ssh
/// passes on ssh's bare PATH, where Homebrew, node and the agents are missing.
const LOGIN_PATH: &str = r#"s=${SHELL:-}
[ -n "$s" ] || s=$(dscl . -read "/Users/$(id -un)" UserShell 2>/dev/null | awk '{print $2}')
[ -n "$s" ] || s=$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f7)
[ -x "$s" ] || s=/bin/sh
p='printf "\n__HERDR_POOL_PATH__%s__\n" "$PATH"'
"$s" -lic "$p" </dev/null 2>/dev/null || "$s" -lc "$p" </dev/null 2>/dev/null"#;

pub fn parse_login_path(text: &str) -> Option<String> {
    let start = text.rfind("__HERDR_POOL_PATH__")? + "__HERDR_POOL_PATH__".len();
    let rest = &text[start..];
    let end = rest.rfind("__")?;
    Some(rest[..end].to_string()).filter(|p| !p.is_empty())
}

/// The environment work runs with on `m`: here, ours; there, its login PATH
/// (asked once, then remembered).
pub fn env_for(hc: &mut Hc, m: &Machine) -> Env {
    let mut env = Env::new();
    if m.on.is_none() {
        if let Ok(path) = std::env::var("PATH") {
            env.insert("PATH".into(), path);
        }
        return env;
    }
    let known = crate::state::load().machines.get(&m.id).and_then(|k| k.path.clone());
    if let Some(path) = known.or_else(|| capture_path(hc, m)) {
        env.insert("PATH".into(), path);
    }
    env
}

pub fn capture_path(hc: &mut Hc, m: &Machine) -> Option<String> {
    let out = hc.sh(m.on.as_deref(), LOGIN_PATH, &[], &Env::new(), 30.0).ok()?;
    let path = parse_login_path(&out.text())?;
    let id = m.id.clone();
    let saved = path.clone();
    crate::state::update(|st| {
        let k = st.machines.entry(id).or_default();
        k.path = Some(saved);
        k.path_ms = crate::state::now_ms();
    });
    Some(path)
}

/// Asks that machine's own hc for a terminal's record: everything that comes
/// through our core is translated to how we name things, which is not what
/// a program there must type.
const GET_THERE: &str = r#"p=$(command -v herdr-core 2>/dev/null) || p=$HOME/.local/bin/herdr-core
"$p" get "$1" --json"#;

/// Where a terminal on `m` reaches `caller` (our terminal): `t5` here,
/// `laptop/t5` from a machine that calls this core `laptop` (or its id, when
/// that name is taken there). Only that machine knows the name, and only
/// once something there has a parent here: the first time, a short-lived
/// terminal there asks, then the answer is remembered.
pub fn reply_address(hc: &mut Hc, world: &World, m: &Machine, caller: &str) -> Option<String> {
    let Some(on) = m.on.as_deref() else { return Some(caller.into()) };
    let known = crate::state::load().machines.get(&m.id).cloned().unwrap_or_default();
    if let (Some(name), Some(true)) = (known.calls_me, known.calls_me_for.as_ref().map(|f| *f == world.me.id)) {
        return Some(format!("{name}/{caller}"));
    }
    // Gone once it ends: at the kill below, or by itself if that fails.
    let probe = json!({
        "argv": ["sleep", "30"], "on": on, "caller": caller, "no_notify": true, "rm": true,
        "outlive_parent": true, "labels": {PROBE_LABEL: "1"},
    });
    let address = hc.call("terminal.run", probe).ok()?["address"].as_str()?.to_string();
    let env = env_for(hc, m);
    let seen = hc.sh(Some(on), GET_THERE, &[split_address(&address).1], &env, 30.0);
    let _ = hc.call("terminal.kill", json!({"terminal": address}));
    let _ = hc.call("terminal.rm", json!({"terminal": address}));
    let record: Value = serde_json::from_slice(&seen.ok().filter(|o| o.ok())?.stdout).ok()?;
    let name = record["parent"].as_str()?.strip_suffix(&format!("/{caller}"))?.to_string();
    let (id, me) = (m.id.clone(), world.me.id.clone());
    let saved = name.clone();
    crate::state::update(|st| {
        let k = st.machines.entry(id).or_default();
        k.calls_me = Some(saved);
        k.calls_me_for = Some(me);
    });
    Some(format!("{name}/{caller}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(name: &str, local: bool, agents: u32, jobs: u32, load: Option<Load>) -> Machine {
        Machine {
            name: name.into(),
            on: (!local).then(|| name.to_string()),
            id: name.into(),
            host: name.into(),
            ssh: None,
            state: "connected".into(),
            why: None,
            version: "0.1.14".into(),
            conf: MachineConf::default(),
            load,
            agents,
            jobs,
        }
    }

    fn world(machines: Vec<Machine>) -> World {
        World { me: Me { id: "me".into(), name: "laptop".into(), version: "0.1.14".into() }, machines, terminals: vec![] }
    }

    #[test]
    fn a_busy_machine_loses_to_an_idle_one() {
        let busy = Load { ncpu: 10, load1: 9.0, mem_free: 20, ..Load::default() };
        let idle = Load { ncpu: 10, load1: 0.5, mem_free: 80, ..Load::default() };
        let w = world(vec![machine("local", true, 0, 0, Some(busy)), machine("studio", false, 0, 0, Some(idle))]);
        let d = Defaults::default();
        assert_eq!(w.machines[w.pick(Kind::Job, &[], &d).unwrap()].name, "studio");
        assert_eq!(w.machines[w.pick(Kind::Agent, &[], &d).unwrap()].name, "studio");
    }

    #[test]
    fn shards_spread_before_they_stack() {
        let even = Load { ncpu: 8, load1: 1.0, mem_free: 60, ..Load::default() };
        let w = world(vec![machine("local", true, 0, 0, Some(even.clone())), machine("studio", false, 0, 0, Some(even))]);
        let d = Defaults::default(); // max_jobs 2
        let lanes = w.spread(Kind::Job, &[], 5, &d);
        assert_eq!(lanes.len(), 4, "two machines with room for two each");
        assert_ne!(lanes[0], lanes[1], "the second shard goes to the other machine");
    }

    #[test]
    fn full_tagged_and_unreachable_machines_are_skipped() {
        let mut down = machine("mini", false, 0, 0, None);
        down.state = "unreachable".into();
        let mut big = machine("studio", false, 0, 0, None);
        big.conf.tags = vec!["big".into()];
        let full = machine("local", true, 8, 0, None);
        let w = world(vec![full, big, down]);
        let d = Defaults::default();
        assert_eq!(w.machines[w.pick(Kind::Agent, &["big".into()], &d).unwrap()].name, "studio");
        assert_eq!(w.machines[w.pick(Kind::Agent, &[], &d).unwrap()].name, "studio");
        let none = w.pick(Kind::Agent, &["gpu".into()], &d).err().unwrap();
        assert!(none.msg.contains("no machine has room"), "{}", none.msg);
    }

    #[test]
    fn weight_tips_a_tie() {
        let even = Load { ncpu: 8, load1: 2.0, mem_free: 50, ..Load::default() };
        let mut a = machine("a", false, 0, 0, Some(even.clone()));
        let mut b = machine("b", false, 0, 0, Some(even));
        a.conf.weight = Some(1.0);
        b.conf.weight = Some(2.0);
        let w = world(vec![a, b]);
        assert_eq!(w.machines[w.pick(Kind::Job, &[], &Defaults::default()).unwrap()].name, "b");
    }

    #[test]
    fn load_and_login_path_parse() {
        let l = parse_load("18 9.82 77\n").unwrap();
        assert_eq!((l.ncpu, l.mem_free), (18, 77));
        let l = parse_load("12 1.5 60 34359738368 Apple M2 Max\n").unwrap();
        assert_eq!((l.mem_gb, l.model.as_str()), (32.0, "Apple M2 Max"));
        let noisy = "Welcome!\n\n__HERDR_POOL_PATH__/opt/homebrew/bin:/usr/bin__\n";
        assert_eq!(parse_login_path(noisy).as_deref(), Some("/opt/homebrew/bin:/usr/bin"));
        assert_eq!(parse_login_path("nothing"), None);
    }

    #[test]
    fn addresses_split_by_machine() {
        assert_eq!(split_address("studio/t7"), ("studio", "t7"));
        assert_eq!(split_address("t7"), ("local", "t7"));
    }
}
