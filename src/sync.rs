//! The far side of a job: a bare mirror of the repository per machine, work
//! trees leased from it, dependencies installed once per tree, and the work
//! packed up to come back. Everything runs as small sh scripts through hc, so
//! a machine needs only git and hc.
//!
//! Trees stay after a job ("warm"): the next job there checks out its own
//! snapshot but keeps ignored files (node_modules, target/), and setup only
//! runs again when the lockfile changed.
//!
//! Leases live in an hc document on each machine (`doc.set` is compare and
//! swap), so two runs at once never get the same tree and nothing extra has
//! to run anywhere.

use crate::error::{Error, Result};
use crate::git::Repo;
use crate::hc::{Env, Hc};
use crate::pool::Machine;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const DOC: &str = "herdr-pool";

/// Raw bytes per call: hc takes up to 8 MB in one exec's input or output.
const CHUNK_MB: usize = 6;

#[derive(Serialize, Deserialize, Default)]
pub struct Doc {
    /// Repo key -> tree slot -> who holds it (None: free, still warm).
    #[serde(default)]
    pub trees: BTreeMap<String, BTreeMap<String, Option<Lease>>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Lease {
    pub job: String,
    /// agent, cmd or shard.
    pub kind: String,
    /// Its terminal on that machine (`t7`), once started.
    pub terminal: Option<String>,
    pub base: String,
    pub since: u64,
    /// The core that started it.
    pub by: String,
    /// The result last brought back, if any.
    #[serde(default)]
    pub pulled: Option<String>,
}

impl Doc {
    /// Who holds `slot` of `key`, if anyone.
    pub fn held(&self, key: &str, slot: u32) -> Option<&Lease> {
        self.trees.get(key)?.get(&slot.to_string())?.as_ref()
    }
}

/// What `rm` and `gc` put on a tree while they clean it up, so nobody else
/// cleans it too or checks out into it meanwhile. Job names start with a
/// letter or digit, so this one is never a job's.
pub fn cleaner(of: &Lease, by: &str) -> Lease {
    Lease {
        job: format!("-{}", of.job),
        kind: "cleanup".into(),
        terminal: None,
        since: crate::state::now_ms(),
        by: by.into(),
        ..of.clone()
    }
}

pub fn leases(hc: &mut Hc, m: &Machine) -> Result<(Doc, u64)> {
    let (value, version) = hc.doc_get(m.on.as_deref(), DOC)?;
    let doc = match value.is_null() {
        true => Doc::default(),
        false => serde_json::from_value(value).map_err(|e| Error::new(format!("{}: bad lease document: {e}", m.name)))?,
    };
    Ok((doc, version))
}

/// Changes the lease document, retrying when someone else changed it first.
fn edit<T>(hc: &mut Hc, m: &Machine, mut change: impl FnMut(&mut Doc) -> Result<T>) -> Result<T> {
    for _ in 0..30 {
        let (mut doc, version) = leases(hc, m)?;
        let before = serde_json::to_value(&doc).expect("lease document serializes");
        let out = change(&mut doc)?;
        let value = serde_json::to_value(&doc).expect("lease document serializes");
        if value == before {
            return Ok(out);
        }
        match hc.doc_set(m.on.as_deref(), DOC, &value, version) {
            Ok(_) => return Ok(out),
            Err(e) if e.kind == "conflict" => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::new(format!("{}: the lease document keeps changing; try again", m.name)))
}

/// The lease document's mark on a repository whose copy is being deleted
/// there (`repos rm`): `take` hands out none of its trees meanwhile. Not a
/// number, so nothing counts it as a tree.
pub const CLOSED: &str = "closed";

/// The mark of a `repos rm` that died stops counting after this.
const CLOSED_FOR_MS: u64 = 10 * 60 * 1000;

fn closed(slots: &BTreeMap<String, Option<Lease>>) -> Option<&Lease> {
    slots.get(CLOSED)?.as_ref().filter(|l| crate::state::now_ms().saturating_sub(l.since) < CLOSED_FOR_MS)
}

/// A tree for `lease.job`: the lowest free (warm) slot, else a new one.
pub fn take(hc: &mut Hc, m: &Machine, key: &str, lease: &Lease) -> Result<u32> {
    edit(hc, m, |doc| {
        let slots = doc.trees.entry(key.into()).or_default();
        if closed(slots).is_some() {
            return Err(Error::new(format!("{}: this repository's copy there is being deleted (repos rm); try again in a moment", m.name)));
        }
        let free = slots.iter().filter(|(_, l)| l.is_none()).filter_map(|(s, _)| s.parse::<u32>().ok()).min();
        let slot = free.unwrap_or_else(|| slots.keys().filter_map(|s| s.parse::<u32>().ok()).max().unwrap_or(0) + 1);
        slots.insert(slot.to_string(), Some(lease.clone()));
        Ok(slot)
    })
}

/// Changes the lease on `slot` if `job` still holds it.
pub fn update(hc: &mut Hc, m: &Machine, key: &str, slot: u32, job: &str, f: impl Fn(&mut Lease)) -> Result<()> {
    edit(hc, m, |doc| {
        if let Some(Some(l)) = doc.trees.get_mut(key).and_then(|s| s.get_mut(&slot.to_string())) {
            if l.job == job {
                f(l);
            }
        }
        Ok(())
    })
}

/// What a slot becomes in a `swap`.
pub enum Then {
    /// Free: the tree stays on disk, warm for the next job.
    Free,
    Held(Lease),
    /// No slot any more (its tree was deleted).
    Gone,
}

/// If `job` holds `slot` (None: if it is free), makes it `then`, in one step
/// nobody can come between. Says whether it did.
pub fn swap(hc: &mut Hc, m: &Machine, key: &str, slot: u32, job: Option<&str>, then: Then) -> Result<bool> {
    let slot = slot.to_string();
    edit(hc, m, |doc| {
        let Some(slots) = doc.trees.get_mut(key) else { return Ok(false) };
        let holds = match slots.get(&slot) {
            Some(entry) => entry.as_ref().map(|l| l.job.as_str()) == job,
            None => false,
        };
        if holds {
            match &then {
                Then::Free => slots.insert(slot.clone(), None),
                Then::Held(l) => slots.insert(slot.clone(), Some(l.clone())),
                Then::Gone => slots.remove(&slot),
            };
        }
        Ok(holds)
    })
}

/// Marks `key` closed on `m` with `mark`. False when another `repos rm`
/// has it marked.
pub fn close(hc: &mut Hc, m: &Machine, key: &str, mark: &Lease) -> Result<bool> {
    edit(hc, m, |doc| {
        let slots = doc.trees.entry(key.into()).or_default();
        if closed(slots).is_some_and(|l| l.job != mark.job) {
            return Ok(false);
        }
        slots.insert(CLOSED.into(), Some(mark.clone()));
        Ok(true)
    })
}

/// Takes `mark` off `key` again: nothing was deleted.
pub fn reopen(hc: &mut Hc, m: &Machine, key: &str, mark: &Lease) -> Result<()> {
    edit(hc, m, |doc| {
        let empty = match doc.trees.get_mut(key) {
            Some(slots) => {
                if slots.get(CLOSED).is_some_and(|l| l.as_ref().is_some_and(|l| l.job == mark.job)) {
                    slots.remove(CLOSED);
                }
                slots.is_empty()
            }
            None => false,
        };
        if empty {
            doc.trees.remove(key);
        }
        Ok(())
    })
}

/// Once `key`'s files are gone from `m`: its free trees and `mark` leave the
/// document (a tree someone holds would stay, though none should).
pub fn forget_copy(hc: &mut Hc, m: &Machine, key: &str, mark: &Lease) -> Result<()> {
    edit(hc, m, |doc| {
        let empty = match doc.trees.get_mut(key) {
            Some(slots) => {
                slots.retain(|slot, l| match l {
                    None => false,
                    Some(l) => slot != CLOSED || l.job != mark.job,
                });
                slots.is_empty()
            }
            None => false,
        };
        if empty {
            doc.trees.remove(key);
        }
        Ok(())
    })
}

/// Gives the tree back if `job` still holds it (it stays on disk, warm).
pub fn release(hc: &mut Hc, m: &Machine, key: &str, slot: u32, job: &str) -> Result<()> {
    swap(hc, m, key, slot, Some(job), Then::Free).map(|_| ())
}

/// `~/.herdr-pool`, `/data/pool`: every script starts by resolving it there.
const PRELUDE: &str = r#"set -e
case $1 in "~"|"~/"*) root="$HOME${1#"~"}" ;; *) root=$1 ;; esac
key=$2
mirror="$root/repos/$key.git"
# The mirror is made once, by one run at a time (two first runs at once are
# common), aside and then renamed, so whoever sees it sees it whole. `$1`
# makes a bare repository at the path it is given.
new_mirror() {
  [ ! -d "$mirror" ] || return 0
  mkdir -p "$root/repos" "$root/tmp"
  lock="$mirror.lock" n=0
  until mkdir "$lock" 2>/dev/null; do
    owner=$(cat "$lock/pid" 2>/dev/null || :)
    if { [ -n "$owner" ] && ! kill -0 "$owner" 2>/dev/null; } || [ -n "$(find "$lock" -maxdepth 0 -mmin +15 2>/dev/null)" ]; then
      rm -rf "$lock"; continue # its maker died
    fi
    n=$((n + 1))
    [ "$n" -lt 570 ] || { echo "another run is still making the mirror there ($lock)" >&2; exit 1; }
    sleep 1
  done
  echo $$ > "$lock/pid"
  trap 'rm -rf "$lock"' EXIT
  if [ ! -d "$mirror" ]; then
    rm -rf "$mirror.new"
    "$1" "$mirror.new"
    printf '.herdr-pool/\n' >> "$mirror.new/info/exclude"
    mv "$mirror.new" "$mirror"
  fi
  rm -rf "$lock"
  trap - EXIT
}
"#;

/// Far machines: make the mirror (seeded from origin when that works, so
/// bundles stay small), then say whether it already has the snapshot, else
/// which commits it has.
const PREPARE: &str = r#"origin=$3 snap=$4
from_origin() {
  git init -q --bare "$1"
  [ -n "$origin" ] || return 0
  git -C "$1" remote add origin "$origin"
  GIT_TERMINAL_PROMPT=0 GIT_SSH_COMMAND="ssh -o BatchMode=yes" \
    git -C "$1" fetch -q --no-tags origin '+refs/heads/*:refs/remotes/origin/*' >/dev/null 2>&1 ||
    echo "origin fetch failed there: sending the whole history" >&2
}
new_mirror from_origin
if git -C "$mirror" cat-file -e "$snap^{commit}" 2>/dev/null; then echo have; exit 0; fi
echo need
git -C "$mirror" for-each-ref --format='%(objectname)'
"#;

/// Appends one chunk of a bundle; the last one is fetched into the mirror.
const RECEIVE: &str = r#"job=$3 first=$4 last=$5
mkdir -p "$root/tmp"
f="$root/tmp/$job.bundle"
if [ "$first" = 1 ]; then : > "$f"; fi
cat >> "$f"
if [ "$last" != 1 ]; then exit 0; fi
git -C "$mirror" fetch -q --no-tags "$f" "+refs/herdr-pool/$job:refs/herdr-pool/$job"
# Jobs' refs come and go; this one keeps the newest snapshot known, so the
# next bundle can leave out what it holds. Only a hint: a run shipping at the
# same moment may hold its lock.
git -C "$mirror" update-ref refs/herdr-pool-last "refs/herdr-pool/$job" 2>/dev/null || :
rm -f "$f"
"#;

/// This machine: the mirror is a hardlinked clone of your repository, and
/// fetching from it needs no bundle.
const LOCAL_PREPARE: &str = r#"src=$3 job=$4
from_src() { git clone -q --bare --no-tags "$src" "$1"; }
new_mirror from_src
git -C "$mirror" fetch -q --no-tags "$src" "+refs/herdr-pool/$job:refs/herdr-pool/$job"
"#;

/// The snapshot into the slot's tree: a reused tree is reset to it, untracked
/// files go, ignored ones (dependencies, build caches) stay unless fresh.
const CHECKOUT: &str = r#"slot=$3 snap=$4 branch=$5 fresh=$6 job=$7
tree="$root/trees/$key/$slot"
git -C "$mirror" update-ref "refs/herdr-pool/$job" "$snap"
if [ -e "$tree/.git" ] && git -C "$tree" rev-parse --git-dir >/dev/null 2>&1; then
  cd "$tree"
  # Whatever the last job left half done goes too: a commit it was killed
  # in, a merge or a rebase (the next job's commits would land in them).
  g=$(git rev-parse --git-dir)
  rm -rf "$g/index.lock" "$g/rebase-merge" "$g/rebase-apply" "$g/sequencer" \
    "$g/MERGE_HEAD" "$g/MERGE_MSG" "$g/CHERRY_PICK_HEAD" "$g/REVERT_HEAD"
  git checkout -q --detach -f "$snap"
  if [ "$fresh" = 1 ]; then git clean -q -ffdx; else git clean -q -ffd; fi
else
  rm -rf "$tree"
  mkdir -p "$root/trees/$key"
  git -C "$mirror" worktree prune
  git -C "$mirror" worktree add -q --detach "$tree" "$snap"
  cd "$tree"
fi
if [ -n "$branch" ]; then git checkout -q --ignore-other-worktrees -B "$branch"; fi
pwd -P
"#;

/// Installs dependencies unless the files that decide them are unchanged
/// since the last install in this tree.
const SETUP: &str = r#"slot=$3 cmd=$4
shift 4
tree="$root/trees/$key/$slot"
mark="$root/trees/$key/.$slot.setup"
log="$root/trees/$key/.$slot.setup.log"
cd "$tree"
sum=$( { printf '%s\n' "$cmd"; for f in "$@"; do cat "$f" 2>/dev/null || :; done; } | { shasum 2>/dev/null || sha1sum; } | cut -c1-40)
if [ -f "$mark" ] && [ "$(cat "$mark")" = "$sum" ]; then echo warm; exit 0; fi
rm -f "$mark"
if sh -c "$cmd" > "$log" 2>&1; then printf '%s' "$sum" > "$mark"; echo ran; else tail -n 25 "$log" >&2; exit 1; fi
"#;

/// The tree's work as one commit on top of what the job committed:
/// `same`, `changed`, or ready to fetch (`path` here, `bundle` from afar).
const RESULT: &str = r#"slot=$3 job=$4 base=$5 mode=$6
cd "$root/trees/$key/$slot"
i=$(mktemp)
cp "$(git rev-parse --git-path index)" "$i" 2>/dev/null || :
GIT_INDEX_FILE="$i" git add -A
t=$(GIT_INDEX_FILE="$i" git write-tree)
rm -f "$i"
head=$(git rev-parse HEAD)
if [ "$t" = "$(git rev-parse "$head^{tree}")" ]; then res=$head
else
  # Dated like its parent, so the same work is always the same commit: a
  # pulled result is recognised as pulled.
  d=$(git log -1 --format=%cI "$head")
  res=$(GIT_AUTHOR_NAME=herdr-pool GIT_AUTHOR_EMAIL=herdr-pool@localhost GIT_COMMITTER_NAME=herdr-pool \
    GIT_COMMITTER_EMAIL=herdr-pool@localhost GIT_AUTHOR_DATE="$d" GIT_COMMITTER_DATE="$d" \
    git commit-tree "$t" -p "$head" -m "herdr-pool: uncommitted work of $job")
fi
if [ "$res" = "$base" ]; then echo "same $res"; exit 0; fi
if [ "$mode" = check ]; then echo "changed $res"; exit 0; fi
git update-ref "refs/herdr-pool/result/$job" "$res"
if [ "$mode" = path ]; then echo "path $res $mirror"; exit 0; fi
mkdir -p "$root/tmp"
b="$root/tmp/$job.result.bundle"
git bundle create -q "$b" "refs/herdr-pool/result/$job" "^$base"
echo "bundle $res $(wc -c < "$b" | tr -d ' ') $b"
"#;

const CHUNK: &str = r#"dd if="$1" bs=1048576 skip="$2" count="$3" 2>/dev/null"#;

const DIFF: &str = r#"slot=$3 base=$4 stat=$5
cd "$root/trees/$key/$slot"
i=$(mktemp)
cp "$(git rev-parse --git-path index)" "$i" 2>/dev/null || :
GIT_INDEX_FILE="$i" git add -A
if [ "$stat" = 1 ]; then GIT_INDEX_FILE="$i" git diff --cached --no-color --stat "$base"
else GIT_INDEX_FILE="$i" git diff --cached --no-color "$base"; fi
rm -f "$i"
"#;

/// After a job: its branch and refs go (no job: a free tree); with purge,
/// the tree too.
const DROP: &str = r#"slot=$3 job=$4 purge=$5
tree="$root/trees/$key/$slot"
if [ -e "$tree/.git" ]; then git -C "$tree" checkout -q --detach 2>/dev/null || :; fi
if [ -n "$job" ]; then
  git -C "$mirror" branch -q -D "pool/$job" 2>/dev/null || :
  git -C "$mirror" update-ref -d "refs/herdr-pool/$job" 2>/dev/null || :
  git -C "$mirror" update-ref -d "refs/herdr-pool/result/$job" 2>/dev/null || :
fi
if [ "$purge" = 1 ]; then
  git -C "$mirror" worktree remove --force "$tree" 2>/dev/null || rm -rf "$tree"
  rm -f "$root/trees/$key/.$slot.setup" "$root/trees/$key/.$slot.setup.log"
fi
"#;

/// Claude Code keeps an agent's session log in `<config>/projects/<folder>`,
/// the folder it started in with every character but letters and digits made
/// `-`, in each config folder there may be. These are the logs of folders
/// under `$3` of the pool's root, as given (`$1`) and resolved (`$2`): `$4`
/// exact takes only `<that><slot>`, any takes all. `$5` 1 deletes them, else
/// they are listed.
const CLAUDE_LOGS: &str = r#"claude_logs() {
  for base in "$1" "$2"; do
    [ -n "$base" ] || continue
    p=$(printf '%s' "$base/$3" | sed 's/[^A-Za-z0-9]/-/g')
    for c in ${CLAUDE_CONFIG_DIR:+"$CLAUDE_CONFIG_DIR"} "$HOME"/.claude*; do
      for d in "$c/projects/$p"*; do
        [ -d "$d" ] || continue
        rest=${d##*/}
        rest=${rest#"$p"}
        case $4:$rest in exact:|exact:*[!0-9]*) continue ;; esac
        if [ "$5" = 1 ]; then rm -rf "$d" && echo "deleted-log $d"; else echo "log $d"; fi
      done
    done
  done
}
"#;

/// Every repository's copy here, a line each: its key, the KB its mirror
/// and its trees take, how many trees, when it was last used (epoch s), and
/// where its history came from.
const LIST: &str = r#"for mirror in "$root"/repos/*.git; do
  [ -d "$mirror" ] || continue
  key=${mirror##*/}
  key=${key%.git}
  trees="$root/trees/$key"
  mkb=$(du -sk "$mirror" 2>/dev/null | awk '{print $1 + 0}')
  tkb=0 n=0
  if [ -d "$trees" ]; then
    tkb=$(du -sk "$trees" 2>/dev/null | awk '{print $1 + 0}')
    n=$(find "$trees" -mindepth 1 -maxdepth 1 -type d ! -name '.*' | wc -l | tr -d ' ')
  fi
  used=0
  for f in "$mirror" "$mirror/FETCH_HEAD" "$mirror/packed-refs" "$mirror/refs/herdr-pool" "$trees"; do
    [ -e "$f" ] || continue
    t=$(stat -c %Y "$f" 2>/dev/null || stat -f %m "$f" 2>/dev/null || echo 0)
    [ "$t" -le "$used" ] || used=$t
  done
  origin=$(git -C "$mirror" config --get remote.origin.url 2>/dev/null || :)
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$key" "$mkb" "$tkb" "$n" "$used" "$origin"
done
"#;

/// The repository's copy goes: its trees (dependencies, setup marks and
/// logs with them) and its mirror, a half-made one included.
const DELETE: &str = r#"logs=$3
case $key in ""|.*|*/*) echo "not a repository key: $key" >&2; exit 2 ;; esac
trees="$root/trees/$key"
kb=$(du -sk "$mirror" "$trees" 2>/dev/null | awk '{n += $1} END {print n + 0}')
real=$(cd "$root" 2>/dev/null && pwd -P || :)
rm -rf "$trees" "$mirror" "$mirror.new" "$mirror.lock"
echo "kb $kb"
claude_logs "$root" "$real" "trees/$key/" exact "$logs"
"#;

/// After every copy went: half-sent snapshots and the empty folders too.
const SWEEP: &str = r#"logs=$3
real=$(cd "$root" 2>/dev/null && pwd -P || :)
rm -rf "$root/tmp"
rmdir "$root/trees" "$root/repos" 2>/dev/null || :
claude_logs "$root" "$real" "trees/" any "$logs"
"#;

fn script(body: &str) -> String {
    format!("{PRELUDE}{body}")
}

/// One line per repository copied to `m` (LIST says what is in it).
pub fn copies(hc: &mut Hc, m: &Machine, env: &Env) -> Result<String> {
    run(hc, m, LIST, &[m.root(), ""], env, 300.0)
}

/// What deleting freed, and Claude Code's session logs found there.
#[derive(Default)]
pub struct Deleted {
    pub bytes: u64,
    /// Left there, or deleted with them.
    pub logs: Vec<String>,
    pub logs_deleted: Vec<String>,
}

fn deleted(text: &str) -> Deleted {
    let mut d = Deleted::default();
    for line in text.lines() {
        match line.split_once(' ') {
            Some(("kb", n)) => d.bytes += n.trim().parse::<u64>().unwrap_or(0) * 1024,
            Some(("log", p)) if !d.logs.iter().any(|l| l == p) => d.logs.push(p.into()),
            Some(("deleted-log", p)) => d.logs_deleted.push(p.into()),
            _ => {}
        }
    }
    d
}

/// Deletes `key`'s copy on `m`: mirror and trees. Claude Code's session logs
/// of agents that worked in those trees go too with `logs`, else are listed.
pub fn delete_copy(hc: &mut Hc, m: &Machine, key: &str, logs: bool, env: &Env) -> Result<Deleted> {
    let body = format!("{CLAUDE_LOGS}{DELETE}");
    run(hc, m, &body, &[m.root(), key, if logs { "1" } else { "0" }], env, 600.0).map(|t| deleted(&t))
}

/// After every copy on `m` went: what is left of the pool's folder there.
pub fn sweep(hc: &mut Hc, m: &Machine, logs: bool, env: &Env) -> Result<Deleted> {
    let body = format!("{CLAUDE_LOGS}{SWEEP}");
    run(hc, m, &body, &[m.root(), "", if logs { "1" } else { "0" }], env, 300.0).map(|t| deleted(&t))
}

fn run(hc: &mut Hc, m: &Machine, body: &str, args: &[&str], env: &Env, timeout: f64) -> Result<String> {
    let out = hc.sh(m.on.as_deref(), &script(body), args, env, timeout)?;
    match out.ok() {
        true => Ok(out.text()),
        false => Err(Error::new(out.why())),
    }
}

/// Gets the job's snapshot (already pinned in `repo`) into `m`'s mirror.
/// Returns how many bytes travelled (none: it was there already, or here).
pub fn ship(hc: &mut Hc, m: &Machine, repo: &Repo, job: &str, snap: &str, env: &Env) -> Result<u64> {
    let (root, key) = (m.root(), repo.key.as_str());
    if m.on.is_none() {
        let src = repo.root.to_string_lossy();
        return run(hc, m, LOCAL_PREPARE, &[root, key, &src, job], env, 600.0).map(|_| 0);
    }
    let origin = repo.origin.as_deref().unwrap_or("");
    let said = run(hc, m, PREPARE, &[root, key, origin, snap], env, 600.0)?;
    let mut lines = said.lines();
    if lines.next() == Some("have") {
        return Ok(0);
    }
    let known: Vec<String> = lines.map(String::from).filter(|l| l.len() >= 40).collect();
    let bundle = repo.bundle(job, &known)?;
    let chunks: Vec<&[u8]> = bundle.chunks(CHUNK_MB << 20).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let first = if i == 0 { "1" } else { "0" };
        let last = if i + 1 == chunks.len() { "1" } else { "0" };
        let mut argv = vec!["sh".to_string(), "-c".into(), script(RECEIVE), "herdr-pool".into()];
        argv.extend([root, key, job, first, last].map(String::from));
        let out = hc.exec(m.on.as_deref(), &argv, None, env, Some(chunk), 600.0)?;
        if !out.ok() {
            fail!("sending the snapshot to {}: {}", m.name, out.why());
        }
    }
    Ok(bundle.len() as u64)
}

/// Puts the snapshot in the tree; returns the tree's absolute path there.
#[allow(clippy::too_many_arguments)]
pub fn checkout(
    hc: &mut Hc,
    m: &Machine,
    key: &str,
    slot: u32,
    snap: &str,
    branch: Option<&str>,
    fresh: bool,
    job: &str,
    env: &Env,
) -> Result<String> {
    let slot = slot.to_string();
    let args = [m.root(), key, &slot, snap, branch.unwrap_or(""), if fresh { "1" } else { "0" }, job];
    let out = run(hc, m, CHECKOUT, &args, env, 300.0)?;
    out.lines().last().map(|l| l.trim().to_string()).filter(|p| p.starts_with('/')).ok_or_else(|| {
        Error::new(format!("{}: preparing the tree said no path", m.name))
    })
}

pub enum Setup {
    Warm,
    Ran,
}

pub fn setup(hc: &mut Hc, m: &Machine, key: &str, slot: u32, cmd: &str, files: &[String], env: &Env) -> Result<Setup> {
    let slot = slot.to_string();
    let mut args = vec![m.root(), key, &slot, cmd];
    args.extend(files.iter().map(String::as_str));
    let out = run(hc, m, SETUP, &args, env, 600.0).map_err(|e| e.context(&format!("setup ({cmd}) on {}", m.name)))?;
    Ok(if out.trim() == "warm" { Setup::Warm } else { Setup::Ran })
}

/// The dependency install a tree needs, from the lockfile at its root.
pub fn default_setup(files: &[String]) -> Option<(String, Vec<String>)> {
    let has = |f: &str| files.iter().any(|x| x == f);
    let pick = |cmd: &str, lock: &str| Some((cmd.to_string(), vec![lock.to_string(), "package.json".into()]));
    match () {
        _ if has("bun.lock") => pick("bun install --frozen-lockfile", "bun.lock"),
        _ if has("bun.lockb") => pick("bun install --frozen-lockfile", "bun.lockb"),
        _ if has("pnpm-lock.yaml") => pick("pnpm install --frozen-lockfile", "pnpm-lock.yaml"),
        _ if has("yarn.lock") => pick("yarn install", "yarn.lock"),
        _ if has("package-lock.json") => pick("npm ci --no-audit --no-fund --prefer-offline", "package-lock.json"),
        _ => None,
    }
}

pub enum Outcome {
    /// Nothing new since the base.
    Same,
    /// There is work (only asked whether).
    Changed(String),
    /// Work, fetched into `repo` as `dst`.
    Fetched(String),
}

/// Brings the tree's work back into `repo` as `dst` (a branch). `check`
/// only says whether there is any.
#[allow(clippy::too_many_arguments)]
pub fn bring_back(
    hc: &mut Hc,
    m: &Machine,
    repo: Option<&Repo>,
    key: &str,
    slot: u32,
    job: &str,
    base: &str,
    dst: &str,
    force: bool,
    env: &Env,
) -> Result<Outcome> {
    let mode = match (repo, m.on.is_none()) {
        (None, _) => "check",
        (Some(_), true) => "path",
        (Some(_), false) => "bundle",
    };
    let slot_s = slot.to_string();
    let said = run(hc, m, RESULT, &[m.root(), key, &slot_s, job, base, mode], env, 300.0)?;
    let line = said.lines().last().unwrap_or_default().to_string();
    // A path comes last and whole: it may hold spaces.
    let words: Vec<&str> = match line.split(' ').next() {
        Some("path") => line.splitn(3, ' ').collect(),
        _ => line.splitn(4, ' ').collect(),
    };
    let src = format!("refs/herdr-pool/result/{job}");
    match (words.as_slice(), repo) {
        (["same", _], _) => Ok(Outcome::Same),
        (["changed", res], _) => Ok(Outcome::Changed(res.to_string())),
        (["path", res, mirror], Some(repo)) => {
            repo.fetch(mirror, &src, dst, force)?;
            Ok(Outcome::Fetched(res.to_string()))
        }
        (["bundle", res, size, path], Some(repo)) => {
            let size: usize = size.parse().unwrap_or(0);
            let mut bytes = Vec::with_capacity(size);
            let mut skip = 0;
            while bytes.len() < size {
                let (s, c) = (skip.to_string(), CHUNK_MB.to_string());
                let out = hc.sh(m.on.as_deref(), CHUNK, &[path, &s, &c], env, 300.0)?;
                if !out.ok() || out.stdout.is_empty() {
                    fail!("reading the work from {}: {}", m.name, out.why());
                }
                bytes.extend_from_slice(&out.stdout);
                skip += CHUNK_MB;
            }
            let _ = hc.exec(m.on.as_deref(), &["rm".into(), "-f".into(), path.to_string()], None, env, None, 30.0);
            repo.fetch_bundle(&bytes, &src, dst, force)?;
            Ok(Outcome::Fetched(res.to_string()))
        }
        _ => Err(Error::new(format!("{}: unexpected answer: {line}", m.name))),
    }
}

/// The tree's changes since `base`, and whether there was more than hc
/// passes back in one go (8 MB).
pub fn diff(hc: &mut Hc, m: &Machine, key: &str, slot: u32, base: &str, stat: bool, env: &Env) -> Result<(String, bool)> {
    let slot = slot.to_string();
    let args = [m.root(), key, &slot, base, if stat { "1" } else { "0" }];
    let out = hc.sh(m.on.as_deref(), &script(DIFF), &args, env, 120.0)?;
    match out.ok() {
        true => Ok((out.text(), out.truncated)),
        false => Err(Error::new(out.why())),
    }
}

const CAT: &str = r#"slot=$3 file=$4 skip=$5 count=$6
cd "$root/trees/$key/$slot"
if [ ! -f "$file" ]; then echo "no file $file in the job's tree" >&2; exit 1; fi
dd if="$file" bs=1048576 skip="$skip" count="$count" 2>/dev/null
"#;

/// A file from the job's tree, as bytes: a few MB per call, so a build
/// output of any size comes whole.
pub fn cat(hc: &mut Hc, m: &Machine, key: &str, slot: u32, file: &str, env: &Env) -> Result<Vec<u8>> {
    let (slot, count) = (slot.to_string(), CHUNK_MB.to_string());
    let mut bytes = Vec::new();
    loop {
        let skip = (bytes.len() >> 20).to_string();
        let out = hc.sh(m.on.as_deref(), &script(CAT), &[m.root(), key, &slot, file, &skip, &count], env, 120.0)?;
        if !out.ok() {
            return Err(Error::new(out.why()));
        }
        let full = out.stdout.len() == CHUNK_MB << 20;
        bytes.extend_from_slice(&out.stdout);
        if !full {
            return Ok(bytes);
        }
    }
}

pub fn drop_tree(hc: &mut Hc, m: &Machine, key: &str, slot: u32, job: &str, purge: bool, env: &Env) -> Result<()> {
    let slot = slot.to_string();
    run(hc, m, DROP, &[m.root(), key, &slot, job, if purge { "1" } else { "0" }], env, 120.0).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    #[test]
    fn lockfiles_pick_their_installer() {
        let files = |f: &[&str]| f.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(default_setup(&files(&["package-lock.json", "src"])).unwrap().0.split(' ').next(), Some("npm"));
        assert_eq!(default_setup(&files(&["pnpm-lock.yaml", "package-lock.json"])).unwrap().0.split(' ').next(), Some("pnpm"));
        assert!(default_setup(&files(&["Cargo.lock"])).is_none());
    }

    /// The scripts themselves, run with plain sh against real repositories:
    /// mirror, two trees, setup that runs once, work packed and dropped.
    #[test]
    fn the_far_side_scripts_work_in_plain_sh() {
        let dir = std::env::temp_dir().join(format!("herdr-pool-sync-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sh = |body: &str, args: &[&str]| -> Command {
            let mut c = Command::new("sh");
            c.arg("-c")
                .arg(script(body))
                .arg("herdr-pool")
                .args(args)
                .env("HOME", &dir)
                .env("CLAUDE_CONFIG_DIR", dir.join("claude"))
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t");
            c
        };
        let run = |body: &str, args: &[&str]| -> String {
            let out = sh(body, args).output().unwrap();
            assert!(out.status.success(), "{body}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let src = dir.join("src");
        let ok = Command::new("sh")
            .arg("-c")
            .arg("git init -q -b main src && cd src && printf 'deps\\n' > .gitignore && echo 1 > lock && git add -A && git -c user.name=t -c user.email=t@t commit -qm one && git update-ref refs/herdr-pool/j1 HEAD")
            .current_dir(&dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let snap = crate::git::git(&src, &["rev-parse", "HEAD"]).unwrap();
        let src_s = src.to_string_lossy().into_owned();

        // First runs at once: one makes the mirror, the others wait for it.
        let piped = || std::process::Stdio::piped();
        let first: Vec<_> = (0..3)
            .map(|_| sh(LOCAL_PREPARE, &["~/.herdr-pool", "k", &src_s, "j1"]).stdout(piped()).stderr(piped()).spawn().unwrap())
            .collect();
        let repos = dir.join(".herdr-pool/repos");
        for child in first {
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        }
        let made: Vec<_> = std::fs::read_dir(&repos).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(made, vec![std::ffi::OsString::from("k.git")], "one mirror, no lock or half-made one left");
        // A lock whose maker died doesn't hold anyone up.
        let dead = Command::new("true").spawn().unwrap();
        let pid = dead.id();
        dead.wait_with_output().unwrap();
        std::fs::create_dir_all(repos.join("k2.git.lock")).unwrap();
        std::fs::write(repos.join("k2.git.lock/pid"), pid.to_string()).unwrap();
        run(LOCAL_PREPARE, &["~/.herdr-pool", "k2", &src_s, "j1"]);
        assert!(repos.join("k2.git/HEAD").exists() && !repos.join("k2.git.lock").exists());

        let tree = run(CHECKOUT, &["~/.herdr-pool", "k", "1", &snap, "pool/j1", "0", "j1"]);
        let tree = tree.trim();
        assert!(Path::new(tree).join("lock").exists(), "{tree}");
        let cat = |file: &str| sh(CAT, &["~/.herdr-pool", "k", "1", file, "0", "6"]).output().unwrap();
        assert_eq!(cat("lock").stdout, b"1\n");
        let missing = cat("nope");
        assert!(!missing.status.success() && String::from_utf8_lossy(&missing.stderr).contains("no file nope"));
        // A second job may hold another tree of the same mirror at once.
        run(CHECKOUT, &["~/.herdr-pool", "k", "2", &snap, "pool/j2", "0", "j2"]);

        assert_eq!(run(SETUP, &["~/.herdr-pool", "k", "1", "mkdir -p deps && echo x > deps/x", "lock"]).trim(), "ran");
        assert_eq!(run(SETUP, &["~/.herdr-pool", "k", "1", "mkdir -p deps && echo x > deps/x", "lock"]).trim(), "warm");

        std::fs::write(Path::new(tree).join("new"), "work").unwrap();
        assert!(run(RESULT, &["~/.herdr-pool", "k", "1", "j1", &snap, "check"]).starts_with("changed "));
        let said = run(RESULT, &["~/.herdr-pool", "k", "1", "j1", &snap, "bundle"]);
        let words: Vec<&str> = said.trim().split(' ').collect();
        assert_eq!(words[0], "bundle");
        assert!(Path::new(words[3]).exists());
        let stat = run(DIFF, &["~/.herdr-pool", "k", "1", &snap, "1"]);
        assert!(stat.contains("new"), "{stat}");

        // Reuse: a reset to the snapshot drops the work but keeps ignored deps,
        // and what the last job left half done (a killed commit, a merge).
        run(DROP, &["~/.herdr-pool", "k", "1", "j1", "0"]);
        let gitdir = crate::git::git(Path::new(tree), &["rev-parse", "--absolute-git-dir"]).unwrap();
        std::fs::write(Path::new(&gitdir).join("index.lock"), "").unwrap();
        std::fs::write(Path::new(&gitdir).join("MERGE_HEAD"), format!("{snap}\n")).unwrap();
        run(CHECKOUT, &["~/.herdr-pool", "k", "1", &snap, "", "0", "j3"]);
        assert!(!Path::new(&gitdir).join("MERGE_HEAD").exists() && !Path::new(&gitdir).join("index.lock").exists());
        assert!(!Path::new(tree).join("new").exists());
        assert!(Path::new(tree).join("deps/x").exists(), "warm: ignored files stay");
        assert_eq!(run(SETUP, &["~/.herdr-pool", "k", "1", "mkdir -p deps && echo x > deps/x", "lock"]).trim(), "warm");
        run(DROP, &["~/.herdr-pool", "k", "1", "j3", "0"]);
        run(DROP, &["~/.herdr-pool", "k", "1", "", "1"]); // a free tree: no job's refs to drop
        assert!(!Path::new(tree).exists(), "purged");

        // What repos lists and repos rm deletes. Tree 2 is still there, with
        // a Claude Code session log named after it; one only looks alike,
        // another is k2's.
        let listed = run(LIST, &["~/.herdr-pool", ""]);
        let rows: Vec<Vec<&str>> = listed.lines().map(|l| l.split('\t').collect()).collect();
        assert_eq!(rows.iter().map(|r| r[0]).collect::<Vec<_>>(), ["k", "k2"], "{listed}");
        let k = &rows[0];
        assert!(k[1].parse::<u64>().unwrap() > 0 && k[2].parse::<u64>().unwrap() > 0, "{listed}");
        assert_eq!(k[3], "1", "one tree left: {listed}");
        assert!(k[4].parse::<u64>().unwrap() > 1_600_000_000, "{listed}");
        assert_eq!(k[5], src_s, "a local mirror's origin is your repository");
        let real = std::fs::canonicalize(dir.join(".herdr-pool")).unwrap().to_string_lossy().into_owned();
        let named = |p: String| p.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect::<String>();
        let projects = dir.join("claude/projects");
        let log = projects.join(named(format!("{real}/trees/k/2")));
        let alike = projects.join(named(format!("{real}/trees/k/2x")));
        let other = projects.join(named(format!("{real}/trees/k2/1")));
        for d in [&log, &alike, &other] {
            std::fs::create_dir_all(d).unwrap();
        }
        let delete = format!("{CLAUDE_LOGS}{DELETE}");
        let d = deleted(&run(&delete, &["~/.herdr-pool", "k", "0"]));
        assert!(d.bytes > 0 && d.logs == [log.to_string_lossy()] && d.logs_deleted.is_empty(), "{:?}", d.logs);
        assert!(!repos.join("k.git").exists() && !dir.join(".herdr-pool/trees/k").exists(), "mirror and trees gone");
        assert!(repos.join("k2.git").exists(), "another repository's copy stays");
        assert!(log.exists(), "session logs stay unless asked");
        let d = deleted(&run(&delete, &["~/.herdr-pool", "k", "1"]));
        assert_eq!(d.logs_deleted, [log.to_string_lossy()]);
        assert!(!log.exists() && alike.exists() && other.exists(), "only that tree's logs");
        let climb = sh(&delete, &["~/.herdr-pool", "..", "1"]).output().unwrap();
        assert_eq!(climb.status.code(), Some(2), "a key can't reach outside its folder");
        assert!(dir.join(".herdr-pool/repos").exists());
        run(&delete, &["~/.herdr-pool", "k2", "0"]);
        let d = deleted(&run(&format!("{CLAUDE_LOGS}{SWEEP}"), &["~/.herdr-pool", "", "1"]));
        assert_eq!(d.logs_deleted.len(), 2, "after --all, every pool tree's logs: {:?}", d.logs_deleted);
        for gone in ["repos", "trees", "tmp"] {
            assert!(!dir.join(".herdr-pool").join(gone).exists(), "{gone}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
