// SPDX-License-Identifier: MPL-2.0
//! One editor process under test: a private Xvfb display (unless `--windowed`), the editor on the
//! throwaway project copy with every per-user folder redirected into the run folder, and the agent
//! connection. The editor's stdout and stderr go to `editor.log` in the run folder; the harness
//! reads it for panics and reads `logs.read` for error lines.

use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use regia_agent::client::{Client, ClientError};
use regia_agent::endpoint;
use serde_json::{Value, json};

/// How long the editor gets to advertise its endpoint and answer `agent.hello` (a debug editor on
/// lavapipe compiles its pipelines first).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(240);
/// How long one agent call may take before the step fails as a hang.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a killed editor gets to go.
const KILL_TIMEOUT: Duration = Duration::from_secs(10);

/// What a launch needs.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    /// The editor executable.
    pub exe: PathBuf,
    /// The throwaway project copy.
    pub project: PathBuf,
    /// The run's private folder for the per-user files, the agent endpoint and the log.
    pub work: PathBuf,
    /// The log file (appended to across restarts).
    pub log: PathBuf,
    /// Use the desktop's display instead of a private Xvfb.
    pub windowed: bool,
    /// Run the headless editor (`--headless`: no window, offscreen views).
    pub headless: bool,
    /// Pass `--normal-mode`: a restart after a crash. Two unclean exits put the editor in safe
    /// mode, which never autostarts the agent listener, so a restarted editor would never answer
    /// (the harness reports every crash as its own FAIL first).
    pub normal_mode: bool,
    /// Extra environment (the GPU choice).
    pub env: Vec<(String, String)>,
    /// Environment variables to remove.
    pub env_remove: Vec<String>,
}

/// The editor's command-line arguments for a launch.
#[must_use]
pub fn editor_args(spec: &LaunchSpec) -> Vec<std::ffi::OsString> {
    let mut args = vec!["--project".into(), spec.project.clone().into_os_string()];
    if spec.headless {
        args.push("--headless".into());
    }
    if spec.normal_mode {
        args.push("--normal-mode".into());
    }
    args
}

/// A running editor with its connection.
pub struct Session {
    child: Child,
    xvfb: Option<Child>,
    client: Client,
    log: PathBuf,
    log_offset: u64,
    log_seq: u64,
    /// What `agent.hello` answered.
    pub hello: Value,
    /// The private Xvfb display (`:n`), `None` with `--windowed`.
    pub display: Option<String>,
}

/// The outcome of one agent call.
#[derive(Debug)]
pub enum CallError {
    /// The editor answered with a JSON-RPC error: `(code, message, data)`.
    Rpc(i64, String, Value),
    /// No answer within [`CALL_TIMEOUT`].
    TimedOut,
    /// The connection broke (the editor may have died).
    Gone(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rpc(code, message, data) => write!(f, "{message} ({code}) {data}"),
            Self::TimedOut => write!(f, "no answer within {} s", CALL_TIMEOUT.as_secs()),
            Self::Gone(e) => write!(f, "connection lost: {e}"),
        }
    }
}

impl CallError {
    /// The typed error code an editor error carries (`data.code` or `data.reason`), if any.
    #[must_use]
    pub fn data_code(&self) -> Option<String> {
        match self {
            Self::Rpc(_, _, data) => data
                .get("code")
                .or_else(|| data.get("reason"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            _ => None,
        }
    }
}

impl Session {
    /// Starts Xvfb (unless windowed) and the editor, then waits for its own endpoint.
    ///
    /// # Errors
    ///
    /// Xvfb or the editor could not start, or the editor never answered `agent.hello`.
    pub fn start(spec: &LaunchSpec) -> Result<Self> {
        let xvfb = if spec.windowed || spec.headless {
            None
        } else {
            Some(start_xvfb()?)
        };
        let agent_dir = spec.work.join("agent");
        std::fs::create_dir_all(&agent_dir).with_context(|| format!("creating {}", agent_dir.display()))?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&spec.log)
            .with_context(|| format!("opening {}", spec.log.display()))?;
        let log_offset = log.metadata().map(|m| m.len()).unwrap_or(0);
        let mut cmd = Command::new(&spec.exe);
        cmd.args(editor_args(spec))
            .current_dir(&spec.project)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        let home = spec.work.join("home");
        for (key, dir) in [
            ("HOME", home.clone()),
            ("XDG_CONFIG_HOME", home.join(".config")),
            ("XDG_DATA_HOME", home.join(".local/share")),
            ("XDG_CACHE_HOME", home.join(".cache")),
            ("XDG_STATE_HOME", home.join(".local/state")),
            ("REGIA_EDITOR_CONFIG_DIR", spec.work.join("editor-config")),
            ("REGIA_AGENT_DIR", agent_dir.clone()),
            ("TMPDIR", spec.work.join("tmp")),
        ] {
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            cmd.env(key, dir);
        }
        cmd.env("RUST_BACKTRACE", "1")
            .env("REGIA_PROJECT_DIR", &spec.project);
        for key in &spec.env_remove {
            cmd.env_remove(key);
        }
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }
        let (mut xvfb, display) = match xvfb {
            Some((child, display)) => (Some(child), Some(display)),
            None => (None, None),
        };
        if let Some(display) = &display {
            cmd.env("DISPLAY", display).env_remove("WAYLAND_DISPLAY");
        }
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                if let Some(x) = xvfb.as_mut() {
                    let _ = x.kill();
                    let _ = x.wait();
                }
                return Err(err).with_context(|| format!("spawning {}", spec.exe.display()));
            }
        };
        match wait_connect(&agent_dir, &mut child) {
            Ok(Some((client, hello))) => Ok(Self {
                child,
                xvfb,
                client,
                log: spec.log.clone(),
                log_offset,
                log_seq: 0,
                hello,
                display,
            }),
            other => {
                let status = child.try_wait().ok().flatten();
                terminate(&mut child);
                if let Some(x) = xvfb.as_mut() {
                    let _ = x.kill();
                    let _ = x.wait();
                }
                let panic = first_panic(&spec.log, log_offset)
                    .or_else(|| last_editor_error(&spec.log, log_offset))
                    .unwrap_or_default();
                match other {
                    Err(err) => Err(err),
                    _ => bail!(
                        "the editor did not answer agent.hello within {} s (exit {status:?}) {panic}; log: {}",
                        CONNECT_TIMEOUT.as_secs(),
                        spec.log.display()
                    ),
                }
            }
        }
    }

    /// Calls a method.
    ///
    /// # Errors
    ///
    /// The editor's error, a timeout or a lost connection.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        match self.client.call(method, params) {
            Ok(v) => Ok(v),
            Err(ClientError::Rpc(e)) => Err(CallError::Rpc(
                e.code,
                e.message.clone(),
                e.data.clone().unwrap_or(Value::Null),
            )),
            Err(ClientError::TimedOut) => Err(CallError::TimedOut),
            Err(other) => Err(CallError::Gone(other.to_string())),
        }
    }

    /// Whether the editor process still runs.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// The editor's process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// New panic lines in `editor.log` since the last call.
    pub fn new_panics(&mut self) -> Vec<String> {
        let (lines, end) = read_from(&self.log, self.log_offset);
        self.log_offset = end;
        lines
            .into_iter()
            .filter(|l| l.contains("panicked at") || l.contains("panicked with"))
            .collect()
    }

    /// Marks every log record so far as seen.
    pub fn skip_logs(&mut self) {
        if let Ok(reply) = self.call("logs.read", json!({ "since": self.log_seq, "limit": 1000 }))
            && let Some(next) = reply.get("next").and_then(Value::as_u64)
        {
            self.log_seq = next;
        }
        let (_, end) = read_from(&self.log, self.log_offset);
        self.log_offset = end;
    }

    /// Log records since the last call at warn level and above: `(level, line)`.
    pub fn new_log_records(&mut self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for _ in 0..8 {
            let Ok(reply) = self.call(
                "logs.read",
                json!({ "since": self.log_seq, "level": "warn", "limit": 500 }),
            ) else {
                break;
            };
            let records = reply
                .get("records")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for r in &records {
                let level = r
                    .get("level")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let target = r.get("target").and_then(Value::as_str).unwrap_or_default();
                let message = r.get("message").and_then(Value::as_str).unwrap_or_default();
                out.push((level, format!("{target}: {message}")));
            }
            let next = reply.get("next").and_then(Value::as_u64).unwrap_or(self.log_seq);
            let advanced = next > self.log_seq;
            self.log_seq = next;
            if records.len() < 500 || !advanced {
                break;
            }
        }
        out
    }

    /// Asks the editor to end (`agent.shutdown`), then kills it if it lingers; stops Xvfb.
    pub fn stop(mut self) {
        let _ = self.client.call("agent.shutdown", json!({ "save": false }));
        let deadline = Instant::now() + KILL_TIMEOUT;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        terminate(&mut self.child);
        if let Some(x) = self.xvfb.as_mut() {
            let _ = x.kill();
            let _ = x.wait();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // `stop` consumed the normal path; a dropped session (an error mid-run) must not leave
        // an editor or an Xvfb behind. Both are this process's own children.
        if matches!(self.child.try_wait(), Ok(None)) {
            terminate(&mut self.child);
        }
        if let Some(x) = self.xvfb.as_mut()
            && matches!(x.try_wait(), Ok(None))
        {
            let _ = x.kill();
            let _ = x.wait();
        }
    }
}

/// A private Xvfb: probe display numbers until one stays up. A direct child (not `xvfb-run`, a
/// shell script that would orphan Xvfb on a kill).
fn start_xvfb() -> Result<(Child, String)> {
    for n in 140u32..200 {
        if Path::new(&format!("/tmp/.X11-unix/X{n}")).exists() {
            continue;
        }
        let mut child = Command::new("Xvfb")
            .args([
                format!(":{n}"),
                "-screen".into(),
                "0".into(),
                "1920x1080x24".into(),
                "-nolisten".into(),
                "tcp".into(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context(
                "the headless editor smoke needs Xvfb (package xorg-server-xvfb / xvfb), or pass --windowed",
            )?;
        std::thread::sleep(Duration::from_millis(400));
        if child.try_wait()?.is_none() {
            return Ok((child, format!(":{n}")));
        }
    }
    bail!("no free X display between :140 and :199 for Xvfb")
}

fn wait_connect(agent_dir: &Path, child: &mut Child) -> Result<Option<(Client, Value)>> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok((advertised, token)) = endpoint::read(agent_dir)
            && advertised.pid == child.id()
            && let Ok(mut client) = Client::connect(&advertised.url, &token)
        {
            client.set_read_timeout(Some(CALL_TIMEOUT))?;
            if let Ok(hello) = client.call("agent.hello", json!({ "client": "xtask-editor-smoke" })) {
                return Ok(Some((client, hello)));
            }
        }
        if let Ok(Some(_)) = child.try_wait() {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(None)
}

fn terminate(child: &mut Child) {
    if child.kill().is_ok() {
        let deadline = Instant::now() + KILL_TIMEOUT;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let _ = child.wait();
}

/// Lines of `path` from byte `from`, and the end offset.
fn read_from(path: &Path, from: u64) -> (Vec<String>, u64) {
    let Ok(mut file) = File::open(path) else {
        return (Vec::new(), from);
    };
    if file.seek(SeekFrom::Start(from)).is_err() {
        return (Vec::new(), from);
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return (Vec::new(), from);
    }
    let end = from + bytes.len() as u64;
    (
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(str::to_owned)
            .collect(),
        end,
    )
}

/// The last line the editor printed about itself (`regia-editor: ...`, how it reports a failed
/// start such as a GPU that cannot present to the window) from byte `from`.
#[must_use]
pub fn last_editor_error(path: &Path, from: u64) -> Option<String> {
    read_from(path, from)
        .0
        .into_iter()
        .rev()
        .find(|l| l.starts_with("regia-editor:"))
}

/// The first panic line of `path` from byte `from`.
#[must_use]
pub fn first_panic(path: &Path, from: u64) -> Option<String> {
    read_from(path, from)
        .0
        .into_iter()
        .find(|l| l.contains("panicked at") || l.contains("panicked with"))
}

/// Decodes standard base64 (with or without padding); `None` on a bad character.
#[must_use]
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(u32::from(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        }))
    }
    let clean: Vec<u8> = text
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let mut acc = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            acc |= val(*c)? << (18 - 6 * i);
        }
        let bytes = acc.to_be_bytes();
        let n = match chunk.len() {
            4 => 3,
            3 => 2,
            2 => 1,
            _ => return None,
        };
        out.extend_from_slice(&bytes[1..=n]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(headless: bool, normal_mode: bool) -> LaunchSpec {
        LaunchSpec {
            exe: PathBuf::from("regia-editor"),
            project: PathBuf::from("work/project"),
            work: PathBuf::from("work"),
            log: PathBuf::from("editor.log"),
            windowed: false,
            headless,
            normal_mode,
            env: vec![],
            env_remove: vec![],
        }
    }

    #[test]
    fn a_restart_launches_in_normal_mode_and_a_first_start_does_not() {
        let strs = |s: &LaunchSpec| {
            editor_args(s)
                .into_iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(strs(&spec(false, false)), ["--project", "work/project"]);
        assert_eq!(
            strs(&spec(true, false)),
            ["--project", "work/project", "--headless"]
        );
        // Two unclean exits would otherwise start safe mode, which opens no agent listener.
        assert_eq!(
            strs(&spec(false, true)),
            ["--project", "work/project", "--normal-mode"]
        );
    }

    #[test]
    fn base64_decodes_padded_and_unpadded_text() {
        assert_eq!(base64_decode("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
        assert_eq!(base64_decode("aGVsbG8").as_deref(), Some(&b"hello"[..]));
        assert_eq!(base64_decode("aGk=").as_deref(), Some(&b"hi"[..]));
        assert_eq!(base64_decode("").as_deref(), Some(&b""[..]));
        assert!(base64_decode("a$==").is_none());
    }

    #[test]
    fn panic_lines_are_found_after_the_offset() {
        let dir = regia_test_utils::unique_temp_dir("editor-smoke-log");
        std::fs::create_dir_all(&dir).expect("dir");
        let log = dir.join("editor.log");
        std::fs::write(&log, "thread 'main' panicked at a.rs:1\nok\n").expect("write");
        assert!(first_panic(&log, 0).is_some());
        let len = std::fs::metadata(&log).expect("meta").len();
        assert!(
            first_panic(&log, len).is_none(),
            "an old panic is not reported again"
        );
        std::fs::write(
            &log,
            "x\nregia-editor: GPU selection failed: cannot present to this window\n",
        )
        .expect("write");
        assert_eq!(
            last_editor_error(&log, 0).as_deref(),
            Some("regia-editor: GPU selection failed: cannot present to this window")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
