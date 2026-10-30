//! Machines you could add: hosts in your ssh config and your devices on
//! Tailscale, and what each one is (OS, cores, memory, hc or not), as one ssh
//! login finds out. Only hosts ssh already knows (their key is in
//! known_hosts) are tried by themselves: logging in somewhere you never have
//! (a colleague's laptop on the tailnet, a work server) is yours to choose.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Reach {
    /// Not tried: ssh has never been there.
    #[default]
    Unchecked,
    Ready,
    /// ssh got in no further than asking who you are (a password, a key it
    /// doesn't have, a code).
    Login(String),
    /// Off, asleep, or not answering.
    Offline(String),
    /// Not something the pool runs on (Windows, a phone).
    Unsupported(String),
}

/// What one login found out.
#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub reach: Reach,
    pub os: String,
    pub arch: String,
    /// `hostname -s` there.
    pub name: String,
    pub cpus: u32,
    pub mem_gb: f64,
    pub model: String,
    /// The machine's own id (IOPlatformUUID, /etc/machine-id): the same
    /// machine found twice is one.
    pub id: String,
    pub hc: Option<String>,
    pub git: bool,
    pub email: String,
}

impl Probe {
    /// `macOS · M2 Max · 12 cores · 32 GB`
    pub fn summary(&self) -> String {
        let mut parts = vec![match self.os.as_str() {
            "Darwin" => "macOS".to_string(),
            "" => "?".into(),
            os => os.into(),
        }];
        let model = self.model.trim_start_matches("Apple ").trim();
        if !model.is_empty() {
            parts.push(shorten_model(model));
        } else if !self.arch.is_empty() {
            parts.push(self.arch.clone());
        }
        if self.cpus > 0 {
            parts.push(format!("{} cores", self.cpus));
        }
        if self.mem_gb > 0.0 {
            parts.push(format!("{:.0} GB", self.mem_gb));
        }
        parts.join(&format!(" {} ", crate::ui::sep()))
    }
}

/// `Intel(R) Core(TM) i7-8700K CPU @ 3.70GHz` -> `Core i7-8700K`.
fn shorten_model(model: &str) -> String {
    let m = model.replace("(R)", "").replace("(TM)", "").replace(" CPU", "");
    let m = m.split(" @ ").next().unwrap_or(&m).trim().to_string();
    let m = m.strip_prefix("Intel ").unwrap_or(&m).to_string();
    // `AMD Ryzen 9 3900X 12-Core Processor`: the cores are said elsewhere.
    m.split_whitespace().filter(|w| *w != "Processor" && !w.ends_with("-Core")).collect::<Vec<_>>().join(" ")
}

#[derive(Clone, Debug)]
pub struct Candidate {
    /// What ssh and hc dial: an ssh config alias or a host name, maybe with
    /// a user.
    pub target: String,
    /// The host it reaches, lower case, to tell machines apart.
    pub host: String,
    pub from_ssh_config: bool,
    pub on_tailnet: bool,
    /// Tailscale's word for its OS, when it is on the tailnet.
    pub os: Option<String>,
    pub online: bool,
    /// ssh has its host key: it has been there.
    pub known: bool,
    pub found: Probe,
}

impl Candidate {
    /// A machine name for hc: its own name if it told us, else the target's
    /// host, made into something hc accepts.
    pub fn suggested_name(&self) -> String {
        let raw = if self.found.name.is_empty() {
            let host = self.target.rsplit('@').next().unwrap_or(&self.target);
            host.split('.').next().unwrap_or(host).to_string()
        } else {
            self.found.name.clone()
        };
        machine_name(&raw)
    }
}

/// A name hc takes for a machine: 1-32 of a-z 0-9 - _ ., starting with a
/// letter or digit, not shaped like a terminal id (`t7`).
pub fn machine_name(raw: &str) -> String {
    let mut s: String = raw
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let mut s = s.trim_matches(|c: char| !c.is_ascii_alphanumeric()).to_string();
    s.truncate(32);
    let s = s.trim_end_matches(|c: char| !c.is_ascii_alphanumeric()).to_string();
    let id_shaped =
        s.strip_prefix('t').is_some_and(|r| r.starts_with(|c: char| ('1'..='9').contains(&c)) && r.chars().all(|c| c.is_ascii_digit()));
    match (s.is_empty(), id_shaped) {
        (true, _) => "machine".into(),
        (_, true) => format!("{s}-mac"),
        _ => s,
    }
}

/// Why hc would refuse `name`, if it would.
pub fn bad_machine_name(name: &str) -> Option<String> {
    if name.is_empty() || name.len() > 32 {
        return Some("1 to 32 characters".into());
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit()) {
        return Some("starts with a letter or digit".into());
    }
    if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c)) {
        return Some("a-z, 0-9, - _ . only".into());
    }
    (machine_name(name) != name).then(|| "too much like a terminal id (t7)".into())
}

/// Host aliases in your ssh config and the files it includes; patterns
/// (`*`, `?`, `!`) left out.
pub fn ssh_aliases(home: &Path) -> Vec<String> {
    let mut out = Vec::new();
    read_ssh_config(&home.join(".ssh/config"), home, &mut out, 0);
    out
}

fn read_ssh_config(file: &Path, home: &Path, out: &mut Vec<String>, depth: usize) {
    let Ok(text) = std::fs::read_to_string(file) else { return };
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let end = line.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(line.len());
        let rest = line[end..].trim_start_matches(|c: char| c.is_whitespace() || c == '=');
        match line[..end].to_ascii_lowercase().as_str() {
            "host" => {
                for word in words(rest) {
                    if !word.contains(['*', '?', '!']) && !out.contains(&word) {
                        out.push(word);
                    }
                }
            }
            "include" if depth < 8 => {
                for pattern in words(rest) {
                    for f in expand(&pattern, home) {
                        read_ssh_config(&f, home, out, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Words as ssh splits them: by spaces, double quotes holding spaces.
fn words(s: &str) -> Vec<String> {
    let (mut out, mut word, mut quoted) = (Vec::new(), String::new(), false);
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// An Include pattern's files: `~` is home, relative is under ~/.ssh, and
/// the last part may hold wildcards.
fn expand(pattern: &str, home: &Path) -> Vec<PathBuf> {
    let p = match pattern.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if pattern.starts_with('/') => PathBuf::from(pattern),
        None => home.join(".ssh").join(pattern),
    };
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !name.contains(['*', '?']) {
        return vec![p];
    }
    let Some(dir) = p.parent() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> =
        entries.flatten().filter(|e| wildcard(&name, &e.file_name().to_string_lossy())).map(|e| e.path()).collect();
    files.sort();
    files
}

fn wildcard(pattern: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pattern.chars().collect(), s.chars().collect());
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while j < s.len() {
        if i < p.len() && (p[i] == '?' || p[i] == s[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(st) = star {
            i = st + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|c| *c == '*')
}

/// Where ssh really goes for `alias` (`ssh -G` applies the whole config).
fn resolve(alias: &str) -> Option<(String, u16)> {
    let out = Command::new("ssh").args(["-G", alias]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_ssh_g(&String::from_utf8_lossy(&out.stdout))
}

fn parse_ssh_g(text: &str) -> Option<(String, u16)> {
    let field = |k: &str| text.lines().find_map(|l| l.strip_prefix(k).map(str::trim).map(String::from));
    Some((field("hostname ")?.to_ascii_lowercase(), field("port ").and_then(|p| p.parse().ok()).unwrap_or(22)))
}

pub struct Peer {
    /// The name ssh can dial: its MagicDNS name, else its address.
    pub target: String,
    /// Every name and address it answers to, lower case.
    pub names: Vec<String>,
    pub os: String,
    pub online: bool,
}

fn tailscale() -> Option<PathBuf> {
    let app = PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
    crate::hc::on_path("tailscale").or_else(|| app.is_file().then_some(app))
}

/// Your other devices on Tailscale, from `tailscale status --json`.
fn tailnet() -> Vec<Peer> {
    let Some(bin) = tailscale() else { return Vec::new() };
    let Some(out) = output_within(Command::new(bin).args(["status", "--json"]), Duration::from_secs(4)) else {
        return Vec::new();
    };
    serde_json::from_slice::<Value>(&out).map(|v| parse_tailnet(&v)).unwrap_or_default()
}

fn parse_tailnet(v: &Value) -> Vec<Peer> {
    let magic = v["CurrentTailnet"]["MagicDNSEnabled"].as_bool().unwrap_or(false);
    let mut peers: Vec<Peer> = v["Peer"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
        .filter(|p| p["ShareeNode"] != true)
        .map(|p| {
            let dns = p["DNSName"].as_str().unwrap_or_default().trim_end_matches('.').to_ascii_lowercase();
            let short = dns.split('.').next().unwrap_or_default().to_string();
            let ips: Vec<String> = p["TailscaleIPs"].as_array().into_iter().flatten().filter_map(|i| i.as_str()).map(String::from).collect();
            let target = match (magic && !short.is_empty(), ips.first()) {
                (true, _) | (false, None) => short.clone(),
                (false, Some(ip)) => ip.clone(),
            };
            let host = p["HostName"].as_str().unwrap_or_default().to_ascii_lowercase();
            let mut names = vec![short, dns, host];
            names.extend(ips);
            names.retain(|n| !n.is_empty());
            Peer { target, names, os: p["OS"].as_str().unwrap_or_default().into(), online: p["Online"] == true }
        })
        .filter(|p| !p.target.is_empty())
        .collect();
    peers.sort_by(|a, b| a.target.cmp(&b.target));
    peers
}

/// Whether ssh has a key for `host` (on `port`): hashed known_hosts too.
fn known(host: &str, port: u16) -> bool {
    let key = if port == 22 { host.to_string() } else { format!("[{host}]:{port}") };
    Command::new("ssh-keygen")
        .args(["-F", &key])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Everything there is to offer, tried where ssh has been before. `skip`
/// says a target or host is already in the pool (or is this machine).
pub fn gather(home: &Path, skip: &(dyn Fn(&str, &str) -> bool + Sync)) -> Vec<Candidate> {
    let aliases = ssh_aliases(home);
    let (resolved, peers) = std::thread::scope(|s| {
        let peers = s.spawn(tailnet);
        let resolved: Vec<(String, Option<(String, u16)>)> =
            aliases.iter().cloned().zip(par_map(&aliases, |a| resolve(a))).map(|(a, r)| (a, r.flatten())).collect();
        (resolved, peers.join().unwrap_or_default())
    });
    let mut found: Vec<Candidate> = Vec::new();
    let mut ports = Vec::new();
    for (alias, at) in resolved {
        let (host, port) = at.unwrap_or_else(|| (alias.to_ascii_lowercase(), 22));
        let peer = peers.iter().find(|p| p.names.contains(&host) || p.names.contains(&alias.to_ascii_lowercase()));
        found.push(Candidate {
            target: alias,
            host,
            from_ssh_config: true,
            on_tailnet: peer.is_some(),
            os: peer.map(|p| p.os.clone()),
            online: peer.is_none_or(|p| p.online),
            known: false,
            found: Probe::default(),
        });
        ports.push(port);
    }
    for p in &peers {
        if found.iter().any(|c| p.names.contains(&c.host)) {
            continue;
        }
        found.push(Candidate {
            target: p.target.clone(),
            host: p.target.clone(),
            from_ssh_config: false,
            on_tailnet: true,
            os: Some(p.os.clone()),
            online: p.online,
            known: false,
            found: Probe::default(),
        });
        ports.push(22);
    }
    let mut keep: Vec<(Candidate, u16)> = found.into_iter().zip(ports).filter(|(c, _)| !skip(&c.target, &c.host)).collect();
    let seen: Vec<bool> =
        par_map(&keep, |(c, port)| known(&c.host, *port) || known(&c.target, *port)).into_iter().map(|k| k.unwrap_or(false)).collect();
    for ((c, _), seen) in keep.iter_mut().zip(seen) {
        c.known = seen;
    }
    let mut cands: Vec<Candidate> = keep.into_iter().map(|(c, _)| c).collect();
    for c in &mut cands {
        c.found.reach = match c.os.as_deref() {
            Some(os) if !matches!(os, "macOS" | "linux") => Reach::Unsupported(os.into()),
            _ if !c.online => Reach::Offline("offline on Tailscale".into()),
            _ => Reach::Unchecked,
        };
    }
    cands
}

/// Tries every candidate ssh knows, several at once.
pub fn probe_known(cands: &mut [Candidate]) {
    let todo: Vec<(usize, String)> =
        cands.iter().enumerate().filter(|(_, c)| c.known && c.found.reach == Reach::Unchecked).map(|(i, c)| (i, c.target.clone())).collect();
    let found = par_map(&todo, |(_, target)| probe(target));
    for ((i, _), p) in todo.iter().zip(found) {
        if let Some(p) = p {
            cands[*i].found = p;
        }
    }
}

/// `f` over `items` in threads, at most AT_ONCE at a time: each one runs
/// ssh or ssh-keygen, and a long ssh config would run out of processes.
/// None where `f` panicked.
fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<Option<R>> {
    const AT_ONCE: usize = 16;
    let mut out = Vec::with_capacity(items.len());
    for chunk in items.chunks(AT_ONCE) {
        std::thread::scope(|s| {
            let handles: Vec<_> = chunk.iter().map(|x| s.spawn(|| f(x))).collect();
            out.extend(handles.into_iter().map(|h| h.join().ok()));
        });
    }
    out
}

/// What a login finds out: one line per fact.
const SCRIPT: &str = r#"PATH="$PATH:/usr/sbin:/sbin"
os=$(uname -s 2>/dev/null)
echo "os=$os"
echo "arch=$(uname -m 2>/dev/null)"
echo "name=$(hostname -s 2>/dev/null || hostname 2>/dev/null)"
case $os in
Darwin)
  echo "cpus=$(sysctl -n hw.ncpu 2>/dev/null)"
  echo "mem=$(sysctl -n hw.memsize 2>/dev/null)"
  echo "model=$(sysctl -n machdep.cpu.brand_string 2>/dev/null)"
  echo "id=$(ioreg -rd1 -c IOPlatformExpertDevice 2>/dev/null | awk -F'"' '/IOPlatformUUID/{print $4}')" ;;
Linux)
  echo "cpus=$(nproc 2>/dev/null)"
  echo "mem=$(awk '/^MemTotal/{printf "%d", $2*1024}' /proc/meminfo 2>/dev/null)"
  echo "model=$(awk -F': *' '/^model name/{print $2; exit}' /proc/cpuinfo 2>/dev/null)"
  echo "id=$(cat /etc/machine-id 2>/dev/null)" ;;
esac
p=$(command -v herdr-core 2>/dev/null) || p=$HOME/.local/bin/herdr-core
[ -x "$p" ] && echo "hc=$("$p" --version 2>/dev/null | head -n 1)"
command -v git >/dev/null 2>&1 && echo "git=1"
echo "email=$(git config --global user.email 2>/dev/null)"
"#;

/// This machine, the way `probe` sees another.
pub fn here() -> Probe {
    let out = Command::new("sh").args(["-c", SCRIPT]).stdin(Stdio::null()).stderr(Stdio::null()).output();
    let mut p = parse_probe(&out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default());
    p.reach = Reach::Ready;
    p
}

/// One login to `target`, no questions (BatchMode): what is there, or why
/// it couldn't get in.
pub fn probe(target: &str) -> Probe {
    let mut cmd = Command::new("ssh");
    // StrictHostKeyChecking=yes: a key ssh doesn't know is never taken on
    // (an `accept-new` in your config would take it without asking).
    cmd.args(["-T", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", "ConnectTimeout=6"])
        .args(["-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=2", target, "sh -s"]);
    asking(cmd)
}

/// The same questions through a `--via` command, which takes a shell line
/// as its last argument the way ssh does (sh reads the command as you wrote it).
pub fn probe_via(via: &str) -> Probe {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", &format!("{via} 'sh -s'")]);
    asking(cmd)
}

/// Runs the probe script through `cmd`, which runs `sh -s` there.
fn asking(mut cmd: Command) -> Probe {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Ok(mut child) = cmd.spawn() else {
        return Probe { reach: Reach::Offline("can't run ssh".into()), ..Probe::default() };
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(SCRIPT.as_bytes());
    }
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let read = |r: Option<std::process::ChildStdout>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut r) = r {
                use std::io::Read;
                let _ = r.read_to_string(&mut s);
            }
            s
        })
    };
    let out = read(stdout);
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut r) = stderr {
            use std::io::Read;
            let _ = r.read_to_string(&mut s);
        }
        s
    });
    let deadline = Instant::now() + Duration::from_secs(25);
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Probe { reach: Reach::Offline("no answer".into()), ..Probe::default() };
            }
        }
    };
    classify(code, &out.join().unwrap_or_default(), &err.join().unwrap_or_default())
}

fn classify(code: Option<i32>, out: &str, err: &str) -> Probe {
    if code == Some(0) || out.contains("os=") {
        let mut p = parse_probe(out);
        p.reach = match p.os.as_str() {
            "Darwin" | "Linux" => Reach::Ready,
            "" => Reach::Unsupported("no sh there".into()),
            os => Reach::Unsupported(os.into()),
        };
        return p;
    }
    let e = err.to_ascii_lowercase();
    let first = err.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("Warning:")).unwrap_or("ssh failed");
    let reach = match () {
        _ if e.contains("identification has changed") => Reach::Offline("its host key CHANGED: ssh refuses (check it)".into()),
        _ if e.contains("permission denied") || e.contains("too many authentication failures") => Reach::Login(first.into()),
        _ if e.contains("host key verification failed") => Reach::Unchecked,
        _ if e.contains("could not resolve") || e.contains("not known") => Reach::Offline("unknown host".into()),
        _ if e.contains("timed out") => Reach::Offline("no answer".into()),
        _ if e.contains("refused") => Reach::Offline("ssh is off there (connection refused)".into()),
        _ if e.contains("no route") || e.contains("unreachable") => Reach::Offline("unreachable".into()),
        _ if e.contains("not recognized") => Reach::Unsupported("Windows".into()),
        _ => Reach::Offline(first.into()),
    };
    Probe { reach, ..Probe::default() }
}

fn parse_probe(text: &str) -> Probe {
    let mut p = Probe::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k {
            "os" => p.os = v.into(),
            "arch" => p.arch = v.into(),
            "name" => p.name = v.into(),
            "cpus" => p.cpus = v.parse().unwrap_or(0),
            "mem" => p.mem_gb = v.parse::<f64>().map(|b| b / 1_073_741_824.0).unwrap_or(0.0),
            "model" => p.model = v.into(),
            "id" => p.id = v.into(),
            "hc" => p.hc = v.split_whitespace().last().map(String::from),
            "git" => p.git = true,
            "email" => p.email = v.into(),
            _ => {}
        }
    }
    p
}

/// A command's stdout, if it finishes in time and well.
fn output_within(cmd: &mut Command, limit: Duration) -> Option<Vec<u8>> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        use std::io::Read;
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(s)) if s.success() => return reader.join().ok(),
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_are_shortened() {
        assert_eq!(shorten_model("AMD Ryzen 9 3900X 12-Core Processor"), "AMD Ryzen 9 3900X");
        assert_eq!(shorten_model("Intel(R) Core(TM) i7-8700K CPU @ 3.70GHz"), "Core i7-8700K");
        assert_eq!(shorten_model("Apple M2 Max"), "Apple M2 Max");
    }

    #[test]
    fn ssh_config_hosts_follow_includes_and_skip_patterns() {
        let dir = std::env::temp_dir().join(format!("hp-ssh-{}", std::process::id()));
        let ssh = dir.join(".ssh");
        std::fs::create_dir_all(ssh.join("conf.d")).unwrap();
        std::fs::write(
            ssh.join("config"),
            "# mine\nInclude conf.d/*.conf\nHost studio mini.local\n  HostName studio.lan\nHost *.corp !bastion\nHost=\"quoted\"\nmatch host x\nHost studio\n",
        )
        .unwrap();
        std::fs::write(ssh.join("conf.d/a.conf"), "Host from-include\n").unwrap();
        std::fs::write(ssh.join("conf.d/b.txt"), "Host not-matched\n").unwrap();
        assert_eq!(ssh_aliases(&dir), vec!["from-include", "studio", "mini.local", "quoted"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn wildcards_match_like_ssh() {
        assert!(wildcard("*.conf", "a.conf"));
        assert!(!wildcard("*.conf", "a.txt"));
        assert!(wildcard("h?st*", "host-1"));
        assert!(wildcard("*", ""));
    }

    #[test]
    fn ssh_g_gives_host_and_port() {
        assert_eq!(parse_ssh_g("user me\nhostname Studio.Local\nport 2222\n"), Some(("studio.local".into(), 2222)));
        assert_eq!(parse_ssh_g("port 22\n"), None);
    }

    #[test]
    fn tailnet_peers_keep_names_and_drop_shared_ones() {
        let v: Value = serde_json::from_str(
            r#"{"CurrentTailnet":{"MagicDNSEnabled":true},"Peer":{
              "a":{"HostName":"Home-MBP","DNSName":"mbp-home.tail1.ts.net.","OS":"macOS","Online":true,"TailscaleIPs":["100.101.1.1"]},
              "b":{"HostName":"phone","DNSName":"phone.tail1.ts.net.","OS":"android","Online":true,"TailscaleIPs":["100.2.2.2"]},
              "c":{"HostName":"theirs","DNSName":"theirs.tail2.ts.net.","OS":"linux","Online":true,"ShareeNode":true}}}"#,
        )
        .unwrap();
        let peers = parse_tailnet(&v);
        assert_eq!(peers.len(), 2, "a shared-in device isn't yours to add");
        assert_eq!(peers[0].target, "mbp-home");
        assert!(peers[0].names.contains(&"100.101.1.1".to_string()) && peers[0].names.contains(&"home-mbp".to_string()));
        assert_eq!(peers[1].os, "android");
    }

    #[test]
    fn a_probe_reads_its_facts() {
        let p = classify(
            Some(0),
            "os=Darwin\narch=arm64\nname=Home-MBP\ncpus=12\nmem=34359738368\nmodel=Apple M2 Max\nid=ABC\nhc=hc 0.1.14\ngit=1\nemail=\n",
            "",
        );
        assert_eq!(p.reach, Reach::Ready);
        assert_eq!(p.summary(), "macOS · M2 Max · 12 cores · 32 GB");
        assert_eq!(p.hc.as_deref(), Some("0.1.14"));
        assert!(p.git && p.email.is_empty());
        let linux = parse_probe("os=Linux\narch=x86_64\ncpus=8\nmodel=Intel(R) Core(TM) i7-8700K CPU @ 3.70GHz\n");
        assert_eq!(linux.summary(), "Linux · Core i7-8700K · 8 cores");
    }

    #[test]
    fn ssh_failures_say_what_to_do() {
        let login = classify(Some(255), "", "me@x: Permission denied (publickey,password).\n");
        assert!(matches!(login.reach, Reach::Login(_)));
        assert_eq!(classify(Some(255), "", "ssh: connect to host x port 22: Connection refused\n").reach, Reach::Offline("ssh is off there (connection refused)".into()));
        assert_eq!(classify(Some(255), "", "ssh: Could not resolve hostname x: nodename nor servname provided, or not known\n").reach, Reach::Offline("unknown host".into()));
        assert_eq!(classify(Some(255), "", "Host key verification failed.\n").reach, Reach::Unchecked);
        assert!(matches!(classify(Some(255), "", "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@\n").reach, Reach::Offline(_)));
    }

    #[test]
    fn machine_names_are_ones_hc_takes() {
        assert_eq!(machine_name("Home-MBP"), "home-mbp");
        assert_eq!(machine_name("MacBook Pro de Alex"), "macbook-pro-de-alex");
        assert_eq!(machine_name("t7"), "t7-mac");
        assert_eq!(machine_name("--"), "machine");
        assert_eq!(bad_machine_name("studio"), None);
        assert!(bad_machine_name("Studio").is_some());
        assert!(bad_machine_name("t3").is_some());
        assert!(bad_machine_name("").is_some());
    }
}
