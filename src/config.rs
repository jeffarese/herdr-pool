//! `~/.config/herdr-pool/config.toml`: what hc doesn't know about your
//! machines (how much each should take, what it is good for) and about your
//! repositories (how to install their dependencies, how to run their tests).
//! Machines themselves are hc's: `hc machine add` (or `herdr-pool add`).

use crate::error::{Error, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub defaults: Defaults,
    /// By hc machine name; `local` is this machine.
    pub machine: BTreeMap<String, MachineConf>,
    /// By repository: `org/repo` from its origin, or its folder name.
    pub repo: BTreeMap<String, RepoConf>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
    /// Agents a machine takes at once (yours started by hand count when working).
    pub max_agents: u32,
    /// Commands and test shards a machine takes at once: they use its CPUs.
    pub max_jobs: u32,
    /// Whether this machine is part of the pool.
    pub local: bool,
    /// Programs that are agents: they get a task footer and count as agents.
    pub agents: Vec<String>,
    /// Lines of a failed shard or job to show.
    pub tail: usize,
    /// Let Claude Code agents start in pool trees without asking whether
    /// you trust the folder (see trust.rs): your repositories, your machines.
    pub trust: bool,
}

impl Default for Defaults {
    fn default() -> Defaults {
        Defaults {
            max_agents: 8,
            max_jobs: 2,
            local: true,
            agents: [
                "claude", "codex", "pi", "gemini", "opencode", "amp", "cursor-agent", "aider", "goose", "crush", "droid",
                "qwen",
            ]
            .map(String::from)
            .to_vec(),
            tail: 30,
            trust: true,
        }
    }
}

#[derive(Deserialize, Default, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct MachineConf {
    pub max_agents: Option<u32>,
    pub max_jobs: Option<u32>,
    /// What it is good for: `--needs big` only picks machines tagged `big`.
    pub tags: Vec<String>,
    /// Above 1 favours it, below 1 spares it.
    pub weight: Option<f64>,
    /// Never place work here (it stays reachable with `--on`).
    pub disabled: bool,
    /// Where repositories and work trees live there (default `~/.herdr-pool`).
    pub root: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct RepoConf {
    /// Installs dependencies in a work tree; empty disables the default.
    pub setup: Option<String>,
    /// Files whose change means setup must run again.
    pub setup_when: Option<Vec<String>>,
    /// The test command, with `{shard}` and `{total}`.
    pub test: Option<String>,
}

pub fn dir() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d).join("herdr-pool"),
        None => home().join(".config/herdr-pool"),
    }
}

pub fn path() -> PathBuf {
    std::env::var_os("HERDR_POOL_CONFIG").map(PathBuf::from).unwrap_or_else(|| dir().join("config.toml"))
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
}

pub fn load() -> Result<Config> {
    let path = path();
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text).map_err(|e| e.context(&path.display().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(Error::new(format!("{}: {e}", path.display()))),
    }
}

pub fn parse(text: &str) -> Result<Config> {
    toml::from_str(text).map_err(|e| Error::new(e.message().to_string()))
}

impl Config {
    pub fn machine(&self, name: &str) -> MachineConf {
        self.machine.get(name).cloned().unwrap_or_default()
    }

    /// Repo settings: this config's section wins over the repository's own
    /// `.herdr-pool.toml`, field by field.
    pub fn repo(&self, names: &[String], root: &Path) -> RepoConf {
        let own = std::fs::read_to_string(root.join(".herdr-pool.toml"))
            .ok()
            .and_then(|t| toml::from_str::<RepoConf>(&t).ok())
            .unwrap_or_default();
        let mine = names
            .iter()
            .find_map(|n| self.repo.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.clone()))
            .unwrap_or_default();
        RepoConf {
            setup: mine.setup.or(own.setup),
            setup_when: mine.setup_when.or(own.setup_when),
            test: mine.test.or(own.test),
        }
    }
}

/// A starting config for `herdr-pool add`: appends a machine's section unless
/// one is there.
pub fn add_machine(name: &str, max_agents: Option<u32>, max_jobs: Option<u32>, tags: &[String]) -> Result<bool> {
    let path = path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if parse(&text)?.machine.contains_key(name) {
        return Ok(false);
    }
    let mut section = format!("\n[machine.{}]\n", toml_key(name));
    if let Some(n) = max_agents {
        section += &format!("max_agents = {n}\n");
    }
    if let Some(n) = max_jobs {
        section += &format!("max_jobs = {n}\n");
    }
    if !tags.is_empty() {
        let tags: Vec<String> = tags.iter().map(|t| format!("{t:?}")).collect();
        section += &format!("tags = [{}]\n", tags.join(", "));
    }
    let mut text = if text.is_empty() { STARTER.to_string() } else { text };
    text.push_str(&section);
    parse(&text)?; // never write a file we can't read back
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, text)?;
    Ok(true)
}

fn toml_key(name: &str) -> String {
    match name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        true => name.into(),
        false => format!("{name:?}"),
    }
}

const STARTER: &str = "# herdr-pool: see `herdr-pool help config`. Machines come from `hc machine ls`;
# a section here only adds pool facts about one.

[defaults]
max_agents = 8   # agents a machine takes at once
max_jobs = 2     # commands and test shards at once (they use its CPUs)
";

pub const HELP: &str = "~/.config/herdr-pool/config.toml (all optional):

  [defaults]
  max_agents = 8          agents a machine takes at once
  max_jobs = 2            commands and test shards at once
  local = true            this machine is part of the pool
  agents = [\"claude\", \"codex\", ...]   programs that are agents
  tail = 30               lines shown from a failed shard
  trust = true            Claude Code trusts pool trees of repos you run (false: it asks)

  [machine.studio]        an hc machine name; `local` is this one
  max_agents = 12
  max_jobs = 4
  tags = [\"big\"]          run --needs big / test --needs big
  weight = 1.5            above 1 favours it
  disabled = true         only with --on
  root = \"~/.herdr-pool\"  where its trees live

  [repo.\"org/repo\"]       by origin path or folder name; also .herdr-pool.toml in the repo
  setup = \"npm ci\"        default: from the lockfile (npm, pnpm, yarn, bun); \"\" for none
  setup_when = [\"package-lock.json\"]
  test = \"npx vitest run --shard={shard}/{total}\"
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_the_defaults() {
        let c = parse("").unwrap();
        assert_eq!(c.defaults.max_agents, 8);
        assert!(c.defaults.agents.contains(&"claude".to_string()));
    }

    #[test]
    fn machines_and_repos_read_back() {
        let c = parse(
            "[defaults]\nmax_jobs = 3\n[machine.studio]\ntags = [\"big\"]\nweight = 2.0\n[repo.\"Acme/web\"]\ntest = \"npm t\"\n",
        )
        .unwrap();
        assert_eq!(c.defaults.max_jobs, 3);
        assert_eq!(c.defaults.max_agents, 8, "unset fields keep their default");
        assert_eq!(c.machine("studio").tags, vec!["big"]);
        assert_eq!(c.machine("other").tags.len(), 0);
        let dir = std::env::temp_dir();
        assert_eq!(c.repo(&["acme/web".into()], &dir).test.as_deref(), Some("npm t"));
    }

    #[test]
    fn a_typo_is_an_error_not_a_silent_default() {
        assert!(parse("[defaults]\nmax_agent = 3\n").is_err());
    }
}
