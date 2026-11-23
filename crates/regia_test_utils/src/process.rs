// SPDX-License-Identifier: MPL-2.0
//! Child processes for tests that run a real binary: killed when the test ends, bounded when
//! they stop making progress, and on an X server nothing else uses (ADR-1907).
//!
//! A bound here is a bound on a hang, not on speed: it only turns a wedged process into a failure
//! with a log, and belongs far above what the process needs on the slowest machine that runs it.

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often a bounded wait looks at the child.
const POLL: Duration = Duration::from_millis(100);

/// A child process killed and reaped when dropped, so a failed assertion leaves nothing running
/// beside the other tests.
pub struct Running(pub Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Waits for `child`, and kills it once `bound` has passed. The status tells the two apart: a
/// killed child has no exit code.
///
/// # Errors
/// Returns the IO error of polling, killing or reaping the child.
pub fn wait_within(child: &mut Child, bound: Duration) -> std::io::Result<ExitStatus> {
    let deadline = Instant::now() + bound;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            child.kill()?;
            return child.wait();
        }
        std::thread::sleep(POLL);
    }
}

/// [`Command::output`] with [`wait_within`]'s bound: a child still running at `bound` is killed,
/// and what it wrote until then comes back with its status. `command` must not leave a
/// grandchild holding the output pipes: the streams are read to their end.
///
/// # Errors
/// Returns the IO error of starting, polling, killing or reaping the child.
pub fn output_within(command: &mut Command, bound: Duration) -> std::io::Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().map(read_on_a_thread);
    let stderr = child.stderr.take().map(read_on_a_thread);
    let mut running = Running(child);
    let status = wait_within(&mut running.0, bound)?;
    Ok(Output {
        status,
        stdout: joined(stdout),
        stderr: joined(stderr),
    })
}

/// Reads `stream` to its end on a thread of its own, so a child that fills one pipe cannot stall
/// behind a reader waiting for the other.
fn read_on_a_thread(mut stream: impl Read + Send + 'static) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stream.read_to_end(&mut bytes);
        bytes
    })
}

/// What a reader of [`read_on_a_thread`] collected; empty when there was no stream.
fn joined(reader: Option<JoinHandle<Vec<u8>>>) -> Vec<u8> {
    reader.and_then(|reader| reader.join().ok()).unwrap_or_default()
}

/// An X server of this test's own. `Xvfb -displayfd` takes a free display number atomically and
/// answers on `stdout` once it accepts connections, so two runs cannot pick the same display, and
/// nothing waits on the ready signal `xvfb-run` loses when it arrives before its `wait` (it then
/// waits forever).
pub struct PrivateDisplay {
    _server: Running,
    display: String,
}

impl PrivateDisplay {
    /// `None` when `Xvfb` is not installed or did not come up.
    pub fn start() -> Option<Self> {
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "1280x720x24",
                "-nolisten",
                "tcp",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        let server = Running(child);
        let mut number = String::new();
        BufReader::new(stdout).read_line(&mut number).ok()?;
        let number = number.trim();
        (!number.is_empty()).then(|| Self {
            display: format!(":{number}"),
            _server: server,
        })
    }

    /// The server's `DISPLAY` value.
    pub fn display(&self) -> &str {
        &self.display
    }

    /// Points `command` at this server.
    pub fn apply(&self, command: &mut Command) {
        command
            .env("DISPLAY", &self.display)
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("XAUTHORITY");
    }
}

/// Writes the executable script `path` (mode 0755) through a short-lived `/bin/sh`, so this
/// process never holds a write handle on the file.
///
/// A test that writes a stand-in tool with `std::fs::write` and then runs it races every other
/// test thread that starts a process: a child forked while the write handle is open keeps a copy
/// of it until that child calls `exec`, and running the script meanwhile fails with `ETXTBSY`
/// ("Text file busy", OS error 26). Closing the file first does not help (the copy is in the
/// other child). Here the only writer is the `sh` child, which has exited when this returns, so
/// no process can still hold the script open for writing and the run cannot fail that way
/// (integfix-misc-reds, ADR-5959).
///
/// # Errors
/// Returns the IO error of starting `sh` or feeding it the text, or an error when `sh` fails.
#[cfg(unix)]
pub fn write_executable_script(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut child = Command::new("/bin/sh")
        .args(["-c", "cat > \"$0\" && chmod 755 \"$0\""])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()?;
    let fed = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("sh has no stdin"))
        .and_then(|mut stdin| stdin.write_all(contents.as_bytes()));
    let status = child.wait()?;
    fed?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "writing the script {} failed: {status}",
            path.display()
        )))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;

    use super::*;

    /// Longer than any test here runs: a `sleep` for this long only ends by being killed.
    const STUCK_SECONDS: &str = "600";

    #[test]
    fn output_within_returns_both_streams_and_the_exit_code() {
        let output = output_within(
            Command::new("sh").args(["-c", "echo out; echo err >&2; exit 3"]),
            Duration::from_secs(600),
        )
        .expect("run sh");
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stdout, b"out\n");
        assert_eq!(output.stderr, b"err\n");
    }

    #[test]
    fn output_within_kills_a_process_past_the_bound() {
        let output =
            output_within(Command::new("sleep").arg(STUCK_SECONDS), Duration::ZERO).expect("run sleep");
        assert_eq!(output.status.code(), None);
        assert_eq!(output.status.signal(), Some(9));
    }

    #[test]
    fn running_ends_the_process_when_dropped() {
        let child = Command::new("sleep")
            .arg(STUCK_SECONDS)
            .spawn()
            .expect("run sleep");
        let pid = child.id();
        drop(Running(child));
        // Reaped as well as killed: a zombie would still answer here.
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }

    /// The fixed-number race `xvfb-run` needs a retry loop for cannot happen here.
    #[test]
    fn two_private_displays_never_share_a_number() {
        let Some(first) = PrivateDisplay::start() else {
            eprintln!("skip: no Xvfb");
            return;
        };
        let second = PrivateDisplay::start().expect("a second server, as the first one came up");
        assert_ne!(first.display(), second.display());
    }
    /// The cause of the `ETXTBSY` flakes: while any other process holds a write handle on a
    /// script (here a child given the file as its stdout, standing in for a sibling test's
    /// child forked mid-write), running the script fails with "Text file busy".
    #[cfg(target_os = "linux")]
    #[test]
    fn a_script_open_for_writing_elsewhere_cannot_run() {
        let dir = crate::unique_temp_dir("etxtbsy-cause");
        let script = dir.join("tool");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").expect("write");
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        let handle = std::fs::OpenOptions::new()
            .append(true)
            .open(&script)
            .expect("open for writing");
        let holder = Running(
            Command::new("sleep")
                .arg(STUCK_SECONDS)
                .stdout(Stdio::from(handle))
                .spawn()
                .expect("run sleep"),
        );
        let error = Command::new(&script).status().expect_err("busy");
        assert_eq!(error.raw_os_error(), Some(26), "{error}");
        drop(holder);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The fix: a script written by `write_executable_script` runs at once, every time, while
    /// other threads keep starting processes (each of which forks this process).
    #[test]
    fn a_script_from_the_helper_runs_while_other_threads_spawn() {
        let dir = crate::unique_temp_dir("etxtbsy-helper");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let spawners: Vec<_> = (0..4)
            .map(|_| {
                let stop = std::sync::Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let _ = Command::new("true").status();
                    }
                })
            })
            .collect();
        for i in 0..200_i32 {
            let script = dir.join(format!("tool-{i}"));
            write_executable_script(&script, &format!("#!/bin/sh\nexit {}\n", i % 7)).expect("write");
            let status = Command::new(&script).status().expect("runs at once");
            assert_eq!(status.code(), Some(i % 7));
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for spawner in spawners {
            spawner.join().expect("spawner");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
