// SPDX-License-Identifier: MPL-2.0
//! Out-of-process extension tools (`specs/57` § 2.7): `[[tool]]` entries run as child processes.
//!
//! The host passes inputs by argument and stdin, clears the environment (only `PATH` and the
//! platform's minimal system variables are kept, so no secrets are inherited), bounds the run
//! time and the output size, and kills the child on timeout or overflow. This is process
//! separation only: no OS-level isolation is applied or promised. Output is data; it never loads
//! code into the host.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use super::manifest::ToolEntry;

/// Default output bound (stdout + stderr) of a tool run.
pub const DEFAULT_MAX_OUTPUT: usize = 16 << 20;

/// Environment variables passed through to a tool.
const KEPT_ENV: [&str; 4] = ["PATH", "SYSTEMROOT", "TEMP", "TMP"];

/// A tool run request.
#[derive(Debug, Clone)]
pub struct ToolRun {
    /// The executable (absolute, or relative to the extension directory).
    pub command: PathBuf,
    /// Arguments.
    pub args: Vec<String>,
    /// Bytes written to stdin.
    pub stdin: Vec<u8>,
    /// Working directory.
    pub cwd: Option<PathBuf>,
    /// Kill after this long.
    pub timeout: Duration,
    /// Kill when stdout + stderr exceed this many bytes.
    pub max_output: usize,
}

impl ToolRun {
    /// A run of manifest tool `tool` of the extension at `dir`.
    #[must_use]
    pub fn for_entry(dir: &Path, tool: &ToolEntry, args: Vec<String>) -> Self {
        Self {
            command: dir.join(&tool.command),
            args,
            stdin: Vec::new(),
            cwd: Some(dir.to_path_buf()),
            timeout: Duration::from_secs(u64::from(tool.timeout_seconds.max(1))),
            max_output: DEFAULT_MAX_OUTPUT,
        }
    }
}

/// What a tool run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// Exit code (`None` when killed by a signal).
    pub status: Option<i32>,
    /// Captured stdout.
    pub stdout: Vec<u8>,
    /// Captured stderr.
    pub stderr: Vec<u8>,
}

/// Why a tool run failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    /// The process could not start (`load_failed`).
    #[error("load_failed: {0}")]
    Spawn(String),
    /// It ran past its timeout and was killed (`tool_timeout`).
    #[error("tool_timeout: killed after {0:?}")]
    Timeout(Duration),
    /// Its output passed the bound and it was killed (`tool_output_too_large`).
    #[error("tool_output_too_large: more than {0} bytes")]
    OutputTooLarge(usize),
}

impl ToolError {
    /// The lowercase protocol code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Spawn(_) => "load_failed",
            Self::Timeout(_) => "tool_timeout",
            Self::OutputTooLarge(_) => "tool_output_too_large",
        }
    }
}

enum Chunk {
    Out(Vec<u8>),
    Err(Vec<u8>),
}

fn pump(mut reader: impl Read + Send + 'static, tx: mpsc::Sender<Option<Chunk>>, out: bool) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    let chunk = if out { Chunk::Out(data) } else { Chunk::Err(data) };
                    if tx.send(Some(chunk)).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = tx.send(None);
    });
}

/// Kills the tool's process group. Through `kill(1)`, because this crate has no `unsafe` for the
/// system call; if that fails the direct child still dies in the caller.
#[cfg(unix)]
fn kill_group(child: &std::process::Child) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// No process groups off Unix (specs/57 § 2.7 promises process separation only).
#[cfg(not(unix))]
fn kill_group(_child: &std::process::Child) {}

/// Runs a tool to completion, timeout or output overflow.
///
/// # Errors
///
/// [`ToolError`]; the child is killed and reaped before an error returns.
pub fn run_tool(run: &ToolRun) -> Result<ToolOutput, ToolError> {
    let _span = tracing::info_span!("extension_tool", command = %run.command.display()).entered();
    let mut cmd = Command::new(&run.command);
    cmd.args(&run.args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in KEPT_ENV {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    if let Some(cwd) = &run.cwd {
        cmd.current_dir(cwd);
    }
    // Its own process group, so a timeout reaches what the tool started as well.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Spawn(format!("{}: {e}", run.command.display())))?;
    if let Some(mut stdin) = child.stdin.take() {
        let data = run.stdin.clone();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let (tx, rx) = mpsc::channel();
    if let Some(out) = child.stdout.take() {
        pump(out, tx.clone(), true);
    }
    if let Some(err) = child.stderr.take() {
        pump(err, tx.clone(), false);
    }
    drop(tx);
    let deadline = std::time::Instant::now() + run.timeout;
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let mut open = 2;
    let kill = |child: &mut std::process::Child| {
        kill_group(child);
        let _ = child.kill();
        let _ = child.wait();
    };
    while open > 0 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            kill(&mut child);
            return Err(ToolError::Timeout(run.timeout));
        }
        match rx.recv_timeout(left) {
            Ok(Some(Chunk::Out(d))) => stdout.extend(d),
            Ok(Some(Chunk::Err(d))) => stderr.extend(d),
            Ok(None) => open -= 1,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                kill(&mut child);
                return Err(ToolError::Timeout(run.timeout));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if stdout.len() + stderr.len() > run.max_output {
            kill(&mut child);
            return Err(ToolError::OutputTooLarge(run.max_output));
        }
    }
    // Both pipes closed: the child is exiting; wait for it within what is left of the timeout.
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(ToolOutput {
                    status: status.code(),
                    stdout,
                    stderr,
                });
            }
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) | Err(_) => {
                kill(&mut child);
                return Err(ToolError::Timeout(run.timeout));
            }
        }
    }
}
