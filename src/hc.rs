//! A line to herdr-core. `hc connect` carries its JSON-RPC over stdin/stdout,
//! finds the core's socket and starts a core when none runs, so this never
//! needs to know socket paths. Plain mode answers one call at a time, so work
//! that runs side by side opens one line per thread.

use crate::error::{Error, Result};
use base64::Engine as _;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub type Env = BTreeMap<String, String>;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

pub const INSTALL_HINT: &str = "install herdr-core: curl -fsSL https://releases.herdr.dev/core/install.sh | sh";

pub struct Hc {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

/// hc's own error: a stable kind (`not_found`, `conflict`, `unreachable`, ...)
/// with a message and a hint.
#[derive(Debug, Clone)]
pub struct HcError {
    pub kind: String,
    pub message: String,
    pub hint: String,
}

impl From<HcError> for Error {
    fn from(e: HcError) -> Error {
        match e.hint.is_empty() {
            true => Error::new(e.message),
            false => Error::new(format!("{} ({})", e.message, e.hint)),
        }
    }
}

/// The hc program: `HERDR_POOL_HC`, else `herdr-core` (the installer always
/// puts it there; `hc` may be another tool's), else `hc`.
pub fn binary() -> PathBuf {
    if let Some(p) = std::env::var_os("HERDR_POOL_HC").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(p) = on_path("herdr-core") {
        return p;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = Path::new(&home).join(".local/bin/herdr-core");
        if p.is_file() {
            return p;
        }
    }
    on_path("hc").unwrap_or_else(|| PathBuf::from("hc"))
}

pub fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// What `machine.exec` saw: exit code and exactly what the program wrote.
pub struct ExecOut {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    /// It wrote more than hc keeps (8 MB): stdout is only the start.
    pub truncated: bool,
}

impl ExecOut {
    pub fn ok(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Why it failed, in a line or a few: the end of stderr, else of stdout.
    pub fn why(&self) -> String {
        if self.timed_out {
            return "timed out".into();
        }
        let err = String::from_utf8_lossy(&self.stderr);
        let text = if err.trim().is_empty() { String::from_utf8_lossy(&self.stdout) } else { err };
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let tail = lines[lines.len().saturating_sub(6)..].join("\n");
        match (tail.is_empty(), self.code) {
            (true, Some(c)) => format!("exit {c}"),
            (true, None) => "killed by a signal".into(),
            (false, _) => tail,
        }
    }
}

impl Hc {
    pub fn connect() -> Result<Hc> {
        use std::os::unix::process::CommandExt;
        let bin = binary();
        let mut child = Command::new(&bin)
            .arg("connect")
            .process_group(0) // the terminal's Ctrl-C is ours to handle (see signal.rs)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::new(format!("cannot run {} connect: {e}; {INSTALL_HINT}", bin.display())))?;
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("piped"));
        Ok(Hc { child, stdin, stdout, next: 0 })
    }

    pub fn call(&mut self, method: &str, params: Value) -> std::result::Result<Value, HcError> {
        self.next += 1;
        let id = self.next;
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let sent = match self.stdin.as_mut() {
            Some(stdin) => writeln!(stdin, "{message}").and_then(|()| stdin.flush()),
            None => Err(std::io::Error::other("closed")),
        };
        if sent.is_err() {
            return Err(self.gone());
        }
        let mut line = String::new();
        loop {
            line.clear();
            match self.stdout.read_line(&mut line) {
                Ok(0) | Err(_) => return Err(self.gone()),
                Ok(_) => {}
            }
            let Ok(reply) = serde_json::from_str::<Value>(&line) else { continue };
            if reply["id"].as_u64() != Some(id) {
                continue; // a notification
            }
            if let Some(e) = reply.get("error") {
                return Err(HcError {
                    kind: e["data"]["kind"].as_str().unwrap_or("error").into(),
                    message: e["message"].as_str().unwrap_or("hc failed").into(),
                    hint: e["data"]["hint"].as_str().unwrap_or_default().into(),
                });
            }
            return Ok(reply["result"].clone());
        }
    }

    /// The line closed: say what hc said on its way out.
    fn gone(&mut self) -> HcError {
        self.stdin = None;
        let mut err = String::new();
        if let Some(mut stderr) = self.child.stderr.take() {
            let _ = stderr.read_to_string(&mut err);
        }
        let err = err.trim();
        HcError {
            kind: "disconnected".into(),
            message: match err.is_empty() {
                true => "hc connect closed the line".into(),
                false => format!("hc: {err}"),
            },
            hint: String::new(),
        }
    }

    /// `machine.exec`: argv as given (no shell), on `on` or here.
    pub fn exec(
        &mut self,
        on: Option<&str>,
        argv: &[String],
        cwd: Option<&str>,
        env: &Env,
        input: Option<&[u8]>,
        timeout: f64,
    ) -> std::result::Result<ExecOut, HcError> {
        let mut p = json!({"argv": argv, "env": env, "timeout": timeout.min(600.0)});
        if let Some(on) = on {
            p["on"] = on.into();
        }
        if let Some(cwd) = cwd {
            p["cwd"] = cwd.into();
        }
        if let Some(input) = input {
            p["input"] = B64.encode(input).into();
        }
        let r = self.call("machine.exec", p)?;
        let decode = |v: &Value| B64.decode(v.as_str().unwrap_or_default()).unwrap_or_default();
        Ok(ExecOut {
            code: r["code"].as_i64().map(|c| c as i32),
            stdout: decode(&r["stdout"]),
            stderr: decode(&r["stderr"]),
            timed_out: r["timed_out"].as_bool().unwrap_or(false),
            truncated: r["truncated"].as_bool().unwrap_or(false),
        })
    }

    /// A POSIX sh script with positional arguments: no quoting to get wrong.
    pub fn sh(
        &mut self,
        on: Option<&str>,
        script: &str,
        args: &[&str],
        env: &Env,
        timeout: f64,
    ) -> std::result::Result<ExecOut, HcError> {
        let mut argv = vec!["sh".to_string(), "-c".into(), script.into(), "herdr-pool".into()];
        argv.extend(args.iter().map(|a| a.to_string()));
        self.exec(on, &argv, None, env, None, timeout)
    }

    pub fn write_file(&mut self, on: Option<&str>, path: &str, data: &[u8]) -> std::result::Result<(), HcError> {
        let mut p = json!({"path": path, "data": B64.encode(data)});
        if let Some(on) = on {
            p["on"] = on.into();
        }
        self.call("file.write", p).map(|_| ())
    }

    pub fn doc_get(&mut self, on: Option<&str>, name: &str) -> std::result::Result<(Value, u64), HcError> {
        let mut p = json!({"name": name});
        if let Some(on) = on {
            p["on"] = on.into();
        }
        let r = self.call("doc.get", p)?;
        Ok((r["value"].clone(), r["version"].as_u64().unwrap_or(0)))
    }

    pub fn doc_set(&mut self, on: Option<&str>, name: &str, value: &Value, version: u64) -> std::result::Result<u64, HcError> {
        let mut p = json!({"name": name, "value": value, "version": version});
        if let Some(on) = on {
            p["on"] = on.into();
        }
        Ok(self.call("doc.set", p)?["version"].as_u64().unwrap_or(0))
    }
}

impl Drop for Hc {
    fn drop(&mut self) {
        self.stdin = None; // EOF ends `hc connect`; the core keeps running
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// This terminal's address on this core, when we run inside an hc terminal of
/// this session (as `hc` itself decides it).
pub fn caller() -> Option<String> {
    std::env::var("HC_TERMINAL_ID").ok().filter(|t| !t.is_empty())
}
