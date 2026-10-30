//! What this machine remembers about the others, to skip asking twice: the
//! PATH a login shell gets there (cores started over ssh have a bare one), and
//! what that core calls this one (`laptop` in `laptop/t5`), which is where its
//! terminals send reports. A cache: losing it costs one question per machine.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct State {
    /// By the other core's permanent id.
    pub machines: BTreeMap<String, Known>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct Known {
    pub path: Option<String>,
    pub path_ms: u64,
    /// What it calls this core, and which core (by id) that was for.
    pub calls_me: Option<String>,
    pub calls_me_for: Option<String>,
    /// Repositories whose trees Claude Code there trusts (see trust.rs).
    pub trusted: Vec<String>,
}

pub fn path() -> PathBuf {
    if let Some(p) = std::env::var_os("HERDR_POOL_STATE").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::config::home().join(".local/state"));
    base.join("herdr-pool/state.json")
}

pub fn load() -> State {
    std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Read, change and write back, so two runs at once lose at most a cache entry.
pub fn update(f: impl FnOnce(&mut State)) {
    let mut st = load();
    f(&mut st);
    let path = path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, serde_json::to_vec_pretty(&st).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
