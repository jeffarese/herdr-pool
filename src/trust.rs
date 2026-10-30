//! Claude Code asks once per folder whether you trust it, and an agent
//! started in a new work tree would sit at that question. It keys a work
//! tree's trust by the tree's repository, which for a pool tree is the mirror
//! the pool keeps on that machine (`~/.herdr-pool/repos/<repo>.git`), so one
//! record per repository and machine covers all of its trees. The record is
//! the one Claude Code's own message suggests,
//! `projects[<path>].hasTrustDialogAccepted`, in the config the agent reads.
//! The pool adds it for repositories you send work from, only where Claude
//! Code is set up already, and remembers having done so.

use crate::error::{Error, Result};
use crate::hc::{Env, Hc};
use crate::pool::Machine;
use serde_json::{json, Value};

/// The mirror's real path and Claude Code's config there, whole: the
/// first lines say what was found.
const READ: &str = r#"case $1 in "~"|"~/"*) root="$HOME${1#"~"}" ;; *) root=$1 ;; esac
mirror="$root/repos/$2.git"
[ -d "$mirror" ] || { echo "no mirror at $mirror" >&2; exit 3; }
real=$(cd "$mirror" && pwd -P)
f="${CLAUDE_CONFIG_DIR:-$HOME}/.claude.json"
if [ -L "$f" ]; then printf 'link\n%s\n' "$real"; exit 0; fi
if [ ! -f "$f" ]; then printf 'none\n%s\n' "$real"; exit 0; fi
printf 'file\n%s\n%s\n%s\n' "$real" "$f" "$(cksum < "$f")"
cat "$f"
"#;

/// Puts the new config in place unless it changed since it was read: aside
/// first (same mode), checked whole, then renamed over it. One herdr-pool
/// at a time (a lock a crash left behind goes after a minute); Claude Code
/// itself takes no lock, so the check is all there is against it.
const WRITE: &str = r#"f=$1 sum=$2 size=$3 lock="$1.herdr-pool.lock" n=0
until mkdir "$lock" 2>/dev/null; do
  find "$lock" -maxdepth 0 -mmin +1 -exec rmdir {} \; 2>/dev/null
  n=$((n + 1))
  [ "$n" -lt 50 ] || { echo busy; exit 5; }
  sleep 0.2
done
trap 'rmdir "$lock" 2>/dev/null' EXIT
[ "$(cksum < "$f")" = "$sum" ] || { echo changed; exit 4; }
t="$f.herdr-pool.$$"
cp -p "$f" "$t" && cat > "$t" || { rm -f "$t"; exit 1; }
[ "$(wc -c < "$t" | tr -d ' ')" = "$size" ] || { rm -f "$t"; echo short; exit 6; }
mv -f "$t" "$f"
"#;

pub enum Trust {
    /// Recorded before (here or by Claude Code itself).
    Already,
    Added,
    /// No Claude Code config there: its first run asks other things anyway.
    NoClaude,
}

/// Makes sure Claude Code on `m` trusts repository `key`'s trees.
pub fn ensure(hc: &mut Hc, m: &Machine, key: &str, env: &Env) -> Result<Trust> {
    let done = crate::state::load().machines.get(&m.id).is_some_and(|k| k.trusted.contains(&record(m, key)));
    if done {
        return Ok(Trust::Already);
    }
    for _ in 0..3 {
        let read = hc.sh(m.on.as_deref(), READ, &[m.root(), key], env, 60.0)?;
        if !read.ok() {
            return Err(Error::new(read.why()));
        }
        if read.truncated {
            return Err(Error::new("Claude Code's config there is over 8 MB: trust the repository by hand"));
        }
        let text = String::from_utf8_lossy(&read.stdout);
        let mut head = text.splitn(5, '\n');
        let (kind, real) = (head.next().unwrap_or_default(), head.next().unwrap_or_default().to_string());
        match kind {
            "none" => return Ok(Trust::NoClaude),
            "link" => return Err(Error::new("Claude Code's config there is a symlink: not touching it")),
            _ => {}
        }
        let (file, sum, body) = (head.next().unwrap_or_default(), head.next().unwrap_or_default(), head.next().unwrap_or_default());
        let mut config: Value = serde_json::from_str(body).map_err(|e| Error::new(format!("can't read {file} there: {e}")))?;
        let Some(changed) = trusting(&mut config, &real) else {
            remember(m, key);
            return Ok(Trust::Already);
        };
        let size = changed.len().to_string();
        let argv = ["sh".to_string(), "-c".into(), WRITE.into(), "herdr-pool".into(), file.into(), sum.into(), size];
        let wrote = hc.exec(m.on.as_deref(), &argv, None, env, Some(changed.as_bytes()), 60.0)?;
        if wrote.ok() {
            remember(m, key);
            return Ok(Trust::Added);
        }
        match wrote.code {
            // Changed meanwhile, or the copy arrived cut short: read it again.
            Some(4 | 6) => {}
            Some(5) => return Err(Error::new("another herdr-pool holds Claude Code's config there: try again")),
            _ => return Err(Error::new(wrote.why())),
        }
    }
    Err(Error::new("Claude Code kept changing its config there: try again"))
}

/// The config with `path` trusted, or None when it is already.
fn trusting(config: &mut Value, path: &str) -> Option<String> {
    if config["projects"][path]["hasTrustDialogAccepted"] == true {
        return None;
    }
    if !config.is_object() {
        *config = json!({});
    }
    if !config["projects"].is_object() {
        config["projects"] = json!({});
    }
    let project = &mut config["projects"][path];
    if !project.is_object() {
        // What Claude Code writes for a project it hasn't seen.
        *project = json!({
            "allowedTools": [], "mcpContextUris": [], "mcpServers": {}, "enabledMcpjsonServers": [],
            "disabledMcpjsonServers": [], "hasTrustDialogAccepted": false,
            "hasClaudeMdExternalIncludesApproved": false, "hasClaudeMdExternalIncludesWarningShown": false,
        });
    }
    project["hasTrustDialogAccepted"] = true.into();
    serde_json::to_string_pretty(config).ok()
}

/// What is remembered: the repository under the pool's root there (another
/// root is another mirror, which Claude Code hasn't been told about).
fn record(m: &Machine, key: &str) -> String {
    format!("{}|{key}", m.root())
}

fn remember(m: &Machine, key: &str) {
    let (id, rec) = (m.id.clone(), record(m, key));
    crate::state::update(|st| {
        let k = st.machines.entry(id).or_default();
        if !k.trusted.contains(&rec) {
            k.trusted.push(rec);
        }
    });
}

/// Looks again next time: Claude Code didn't take its task there, which a
/// config reset since (trust gone) would explain.
pub fn forget(m: &Machine, key: &str) {
    let (id, rec) = (m.id.clone(), record(m, key));
    crate::state::update(|st| {
        if let Some(k) = st.machines.get_mut(&id) {
            k.trusted.retain(|t| *t != rec);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_is_added_once_and_the_rest_kept_in_order() {
        let mut c: Value = serde_json::from_str(r#"{"numStartups":3,"projects":{"/a":{"allowedTools":["x"]}},"theme":"dark"}"#).unwrap();
        let out = trusting(&mut c, "/r/repos/k.git").unwrap();
        assert!(out.find("numStartups").unwrap() < out.find("theme").unwrap(), "keys keep their order");
        assert_eq!(c["projects"]["/r/repos/k.git"]["hasTrustDialogAccepted"], true);
        assert_eq!(c["projects"]["/r/repos/k.git"]["allowedTools"], json!([]));
        assert_eq!(c["projects"]["/a"]["allowedTools"], json!(["x"]), "other projects untouched");
        assert!(trusting(&mut c, "/r/repos/k.git").is_none(), "already trusted: nothing to write");
        let mut known: Value = serde_json::from_str(r#"{"projects":{"/p":{"allowedTools":["y"],"hasTrustDialogAccepted":false}}}"#).unwrap();
        trusting(&mut known, "/p").unwrap();
        assert_eq!(known["projects"]["/p"], json!({"allowedTools":["y"],"hasTrustDialogAccepted":true}));
        let mut empty = json!({});
        assert!(trusting(&mut empty, "/x").unwrap().contains("\"hasTrustDialogAccepted\": true"));
    }
}
