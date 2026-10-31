//! The repository on this machine: snapshots of what you see (uncommitted and
//! untracked files included) taken without touching your index or work tree,
//! bundles of just the objects another machine lacks, and work brought back.

use crate::error::{Error, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct Repo {
    pub root: PathBuf,
    /// Folder-safe and stable across machines: `acme-web`.
    pub key: String,
    /// Names a config section may use: `Acme/web`, the folder name.
    pub names: Vec<String>,
    pub origin: Option<String>,
}

pub struct Snap {
    pub sha: String,
    /// It holds changes that are not committed.
    pub dirty: bool,
}

/// Snapshots and results are internal commits; they must not depend on (or
/// borrow) the user's identity.
const IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "herdr-pool"),
    ("GIT_AUTHOR_EMAIL", "herdr-pool@localhost"),
    ("GIT_COMMITTER_NAME", "herdr-pool"),
    ("GIT_COMMITTER_EMAIL", "herdr-pool@localhost"),
];

pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    git_env(dir, args, &[], None)
}

fn git_env(dir: &Path, args: &[&str], env: &[(&str, &str)], input: Option<&[u8]>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args).envs(env.iter().copied());
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() });
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| Error::new(format!("cannot run git: {e}")))?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(input)?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        fail!("git {}: {}", args.first().unwrap_or(&""), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

impl Repo {
    /// The repository `dir` is in, if any.
    pub fn find(dir: &Path) -> Option<Repo> {
        let root = PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"]).ok()?);
        let origin = git(&root, &["remote", "get-url", "origin"]).ok().filter(|o| !o.is_empty());
        let folder = root.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
        let mut names = Vec::new();
        let key = match origin.as_deref().and_then(origin_path) {
            Some(path) => {
                names.push(path.clone());
                slug(&path.replace('/', "-"))
            }
            None => {
                // No origin: the folder name, kept apart from same-named
                // folders by the repository's first commit.
                let first = git(&root, &["rev-list", "--max-parents=0", "HEAD"]).unwrap_or_default();
                let first = first.lines().last().unwrap_or("0000000").chars().take(7).collect::<String>();
                format!("{}-{first}", slug(&folder))
            }
        };
        names.push(folder);
        Some(Repo { root, key, names, origin })
    }

    /// A commit of the work tree as it is now, untracked files included and
    /// ignored ones left out, built in a scratch index so yours is untouched.
    /// With `clean`, just HEAD.
    pub fn snapshot(&self, clean: bool) -> Result<Snap> {
        let head = git(&self.root, &["rev-parse", "--verify", "HEAD^{commit}"])
            .map_err(|_| Error::new("the repository has no commits yet: commit once, then retry"))?;
        if clean {
            return Ok(Snap { sha: head, dirty: false });
        }
        let index = PathBuf::from(git(&self.root, &["rev-parse", "--path-format=absolute", "--git-path", "index"])?);
        let scratch = std::env::temp_dir().join(format!("herdr-pool-index-{}-{}", std::process::id(), crate::state::now_ms()));
        // Starting from a copy of the index keeps `add` fast: it only hashes what changed.
        let _ = std::fs::copy(&index, &scratch);
        let scratch_s = scratch.to_string_lossy().into_owned();
        let env = [("GIT_INDEX_FILE", scratch_s.as_str())];
        let tree = git_env(&self.root, &["add", "-A"], &env, None)
            .and_then(|_| git_env(&self.root, &["write-tree"], &env, None));
        let _ = std::fs::remove_file(&scratch);
        let tree = tree?;
        if tree == git(&self.root, &["rev-parse", "HEAD^{tree}"])? {
            return Ok(Snap { sha: head, dirty: false });
        }
        // Dated like HEAD: the same work tree is the same snapshot, which the
        // other side may already have.
        let date = git(&self.root, &["log", "-1", "--format=%cI", "HEAD"])?;
        let mut env = IDENTITY.to_vec();
        env.extend([("GIT_AUTHOR_DATE", date.as_str()), ("GIT_COMMITTER_DATE", date.as_str())]);
        let sha = git_env(&self.root, &["commit-tree", &tree, "-p", &head, "-m", "herdr-pool: snapshot"], &env, None)?;
        Ok(Snap { sha, dirty: true })
    }

    /// Keeps `sha` alive (and findable as the job's base) while the job lives.
    pub fn pin(&self, job: &str, sha: &str) -> Result<()> {
        git(&self.root, &["update-ref", &format!("refs/herdr-pool/{job}"), sha]).map(|_| ())
    }

    pub fn unpin(&self, job: &str) {
        let _ = git(&self.root, &["update-ref", "-d", &format!("refs/herdr-pool/{job}")]);
    }

    pub fn pinned(&self, job: &str) -> Option<String> {
        git(&self.root, &["rev-parse", "--verify", "--quiet", &format!("refs/herdr-pool/{job}")]).ok()
    }

    /// Which of `shas` exist here.
    pub fn have(&self, shas: &[String]) -> Result<Vec<String>> {
        if shas.is_empty() {
            return Ok(Vec::new());
        }
        let input = shas.join("\n") + "\n";
        let out = git_env(&self.root, &["cat-file", "--batch-check=%(objectname) %(objecttype)"], &[], Some(input.as_bytes()))?;
        Ok(out.lines().filter(|l| !l.ends_with(" missing")).filter_map(|l| l.split(' ').next()).map(String::from).collect())
    }

    /// A bundle of the job's pinned snapshot minus everything reachable from
    /// `known` (commits the other side has): usually a few kilobytes.
    pub fn bundle(&self, job: &str, known: &[String]) -> Result<Vec<u8>> {
        let file = temp_bundle(job);
        let mut revs = format!("refs/herdr-pool/{job}\n");
        for sha in self.have(known)? {
            revs += &format!("^{sha}\n");
        }
        let file_s = file.to_string_lossy().into_owned();
        let made = git_env(&self.root, &["bundle", "create", "-q", &file_s, "--stdin"], &[], Some(revs.as_bytes()));
        let bytes = made.and_then(|_| std::fs::read(&file).map_err(Error::from));
        let _ = std::fs::remove_file(&file);
        bytes
    }

    /// Fetch `src` from a bundle (or another repository's path) into `dst`.
    /// Without `force`, an existing `dst` only moves forward.
    pub fn fetch(&self, from: &str, src: &str, dst: &str, force: bool) -> Result<()> {
        let spec = format!("{}{src}:{dst}", if force { "+" } else { "" });
        git(&self.root, &["fetch", "-q", "--no-tags", from, &spec]).map(|_| ())
    }

    pub fn fetch_bundle(&self, bytes: &[u8], src: &str, dst: &str, force: bool) -> Result<()> {
        let file = temp_bundle("back");
        std::fs::write(&file, bytes)?;
        let fetched = self.fetch(&file.to_string_lossy(), src, dst, force);
        let _ = std::fs::remove_file(&file);
        fetched
    }

    /// `base..result` in a line: commits, files, lines.
    pub fn summary(&self, base: &str, result: &str) -> String {
        let commits = git(&self.root, &["rev-list", "--count", &format!("{base}..{result}")]).unwrap_or_default();
        let stat = git(&self.root, &["diff", "--shortstat", base, result]).unwrap_or_default();
        format!("{} commit(s); {}", commits.trim(), stat.trim())
    }

    /// The job's changes (`base..result`) onto your work tree. A clean apply
    /// leaves your index alone; on a conflict, a three-way apply writes
    /// conflict markers (and stages what merged cleanly).
    pub fn apply(&self, base: &str, result: &str) -> Result<String> {
        let diff = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["diff", "--binary", "--no-color", base, result])
            .output()?;
        if diff.stdout.is_empty() {
            return Ok("nothing to apply".into());
        }
        if git_env(&self.root, &["apply", "--whitespace=nowarn"], &[], Some(&diff.stdout)).is_ok() {
            return Ok("applied to your work tree".into());
        }
        match git_env(&self.root, &["apply", "--3way", "--whitespace=nowarn"], &[], Some(&diff.stdout)) {
            Ok(_) => Ok("applied with a three-way merge (changes staged)".into()),
            Err(e) => Err(e.context("the changes conflict with your work tree; resolve the markers, or merge the branch instead")),
        }
    }
}

/// A temp file for one bundle. Its own even when threads bundle the same job
/// at once, as a test run does shipping to every machine together: git
/// refuses a bundle whose `.lock` another one holds.
fn temp_bundle(what: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("herdr-pool-{what}-{}-{n}.bundle", std::process::id()))
}

/// `git@github.com:Acme/web.git`, `https://github.com/Acme/web` ->
/// `Acme/web`.
pub fn origin_path(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/').map(|(_, p)| p)?,
        None => url.split_once(':').map(|(_, p)| p).unwrap_or(url),
    };
    let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
    match parts.len() {
        0 => None,
        1 => Some(parts[0].into()),
        n => Some(format!("{}/{}", parts[n - 2], parts[n - 1])),
    }
}

/// Lowercase, `a-z 0-9 - _ .` only, at most 48 characters.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().flat_map(char::to_lowercase) {
        let c = if c.is_ascii_alphanumeric() || c == '_' || c == '.' { c } else { '-' };
        if !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    let out = out.trim_matches(|c| c == '-' || c == '.').chars().take(48).collect::<String>();
    if out.is_empty() {
        "repo".into()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_become_org_and_repo() {
        assert_eq!(origin_path("git@github.com:Acme/web.git").as_deref(), Some("Acme/web"));
        assert_eq!(origin_path("https://github.com/acme/tools").as_deref(), Some("acme/tools"));
        assert_eq!(origin_path("ssh://git@host:2222/a/b/c.git").as_deref(), Some("b/c"));
        assert_eq!(slug("Acme-Web"), "acme-web");
        assert_eq!(slug("a//b  c"), "a-b-c");
    }

    /// The whole loop on real repositories: snapshot a dirty tree without
    /// touching it, ship it to a bare mirror, work there, bring it back.
    #[test]
    fn a_snapshot_travels_and_the_work_comes_back() {
        let dir = std::env::temp_dir().join(format!("herdr-pool-git-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sh = |cwd: &Path, script: &str| {
            let ok = Command::new("sh")
                .args(["-c", script])
                .current_dir(cwd)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .envs(IDENTITY)
                .status()
                .unwrap()
                .success();
            assert!(ok, "{script}");
        };
        sh(&dir, "git init -q -b main user && cd user && echo a > a && printf 'deps\\n' > .gitignore && git add -A && git commit -qm one");
        let user = dir.join("user");
        sh(&user, "echo more >> a && echo new > untracked && mkdir deps && echo x > deps/x");
        let repo = Repo::find(&user).unwrap();
        let before = git(&user, &["status", "--porcelain"]).unwrap();

        let snap = repo.snapshot(false).unwrap();
        assert!(snap.dirty);
        assert_eq!(git(&user, &["status", "--porcelain"]).unwrap(), before, "the user's tree and index are untouched");
        assert_eq!(repo.snapshot(false).unwrap().sha, snap.sha, "the same work is the same snapshot");
        let files = git(&user, &["ls-tree", "-r", "--name-only", &snap.sha]).unwrap();
        assert_eq!(files, ".gitignore\na\nuntracked", "untracked in, ignored out");

        repo.pin("job", &snap.sha).unwrap();
        sh(&dir, "git init -q --bare mirror.git");
        let bundle = repo.bundle("job", &[]).unwrap();
        // A test run ships one job to every machine at once.
        std::thread::scope(|s| {
            let each: Vec<_> = (0..6).map(|_| s.spawn(|| repo.bundle("job", &[]))).collect();
            for h in each {
                assert!(h.join().unwrap().is_ok(), "bundles of one job at once");
            }
        });
        std::fs::write(dir.join("b.bundle"), &bundle).unwrap();
        sh(&dir, "git -C mirror.git fetch -q ../b.bundle refs/herdr-pool/job:refs/herdr-pool/job");
        sh(&dir, &format!("git -C mirror.git worktree add -q --detach ../tree {}", snap.sha));
        sh(&dir.join("tree"), "echo agent >> a && git commit -qam work && echo late > late");

        // What the far side does for `pull`, then fetching it back here.
        sh(
            &dir.join("tree"),
            &format!(
                "i=$(mktemp) && cp \"$(git rev-parse --git-path index)\" $i && GIT_INDEX_FILE=$i git add -A && t=$(GIT_INDEX_FILE=$i git write-tree) && r=$(git commit-tree $t -p HEAD -m r) && git update-ref refs/herdr-pool/result/job $r && git bundle create -q ../back.bundle refs/herdr-pool/result/job ^{}",
                snap.sha
            ),
        );
        let back = std::fs::read(dir.join("back.bundle")).unwrap();
        repo.fetch_bundle(&back, "refs/herdr-pool/result/job", "refs/heads/pool/job", false).unwrap();
        let result = git(&user, &["rev-parse", "pool/job"]).unwrap();
        assert!(repo.summary(&snap.sha, &result).starts_with("2 commit(s)"));

        // A second job only ships what is new.
        sh(&user, "git add -A && git commit -qm two");
        let snap2 = repo.snapshot(false).unwrap();
        assert!(!snap2.dirty);
        repo.pin("job2", &snap2.sha).unwrap();
        let known = vec![snap.sha.clone(), "0123456789012345678901234567890123456789".into()];
        let small = repo.bundle("job2", &known).unwrap();
        assert!(small.len() < bundle.len() + 200, "incremental: {} vs {}", small.len(), bundle.len());
        std::fs::write(dir.join("c.bundle"), &small).unwrap();
        sh(&dir, "git -C mirror.git fetch -q ../c.bundle refs/herdr-pool/job2:refs/herdr-pool/job2");

        // Applying the job's work onto the user's (now committed) tree.
        sh(&user, "git reset -q --hard HEAD");
        let applied = repo.apply(&snap.sha, &result).unwrap();
        assert_eq!(applied, "applied to your work tree");
        assert!(std::fs::read_to_string(user.join("late")).unwrap().contains("late"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
