// SPDX-License-Identifier: MPL-2.0
//! Rebuild the game crate with a per-reload metadata salt and copy the dylib (specs/20 §§ 1 and 3).

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use super::load::dylib_extension;

/// One cargo diagnostic extracted from `--message-format=json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildDiagnostic {
    /// `error` or `warning`.
    pub level: String,
    /// Compiler message.
    pub message: String,
    /// Source path, when cargo reported one.
    pub file: Option<String>,
    /// 1-based line, when cargo reported one.
    pub line: Option<u32>,
}

/// A failed game build. The old library keeps running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildFailure {
    /// Process status or spawn error.
    pub summary: String,
    /// Error diagnostics, when cargo emitted JSON.
    pub diagnostics: Vec<BuildDiagnostic>,
}

/// [`BuildFailure::summary`] of a build stopped through its [`BuildCancel`].
pub const BUILD_CANCELLED: &str = "the game build was cancelled";

impl BuildFailure {
    /// The build thread panicked (no cargo result).
    #[must_use]
    pub fn panicked() -> Self {
        Self {
            summary: "the game build thread panicked".to_string(),
            diagnostics: Vec::new(),
        }
    }

    /// The build was cancelled ([`BUILD_CANCELLED`]): cargo was stopped and nothing was copied.
    #[must_use]
    pub fn cancelled() -> Self {
        Self {
            summary: BUILD_CANCELLED.to_string(),
            diagnostics: Vec::new(),
        }
    }

    /// Whether this is [`BuildFailure::cancelled`].
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.summary == BUILD_CANCELLED
    }
}

/// The cancel token of one game build (`specs/19` § 7): the editor sets it, the build thread
/// stops cargo (killing its process) and publishes nothing. It is the editor's task token
/// ([`crate::tasks::CancelToken`], ADR-7381), so the toolbar's Cancel, `editor.cancel_task` and
/// `editor.cancel_job` set the same flag.
pub type BuildCancel = crate::tasks::CancelToken;

/// How often a running cargo is checked for exit or cancel.
const CANCEL_POLL: Duration = Duration::from_millis(25);

/// Runs `command` to completion with piped output, or kills it once `cancel` is set. The whole
/// process tree goes with it (ADR-7340): on Unix the child leads its own process group and the
/// group is killed; on Windows the child runs in a job object with `KILL_ON_JOB_CLOSE` and cancel
/// closes the job, so the compilers cargo started stop too.
///
/// # Errors
///
/// [`BuildFailure`] when the process cannot start, or [`BuildFailure::cancelled`] when the token was
/// set (the process is killed and reaped first).
pub fn run_cancellable(
    command: &mut Command,
    cancel: &BuildCancel,
) -> Result<std::process::Output, BuildFailure> {
    if cancel.is_cancelled() {
        return Err(BuildFailure::cancelled());
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The child leads its own process group, so a cancel kills cargo's compilers too (ADR-7340).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    #[cfg(windows)]
    let job = build_job::BuildJob::new();
    let mut child = command.spawn().map_err(|error| BuildFailure {
        summary: format!("cannot run cargo: {error}"),
        diagnostics: Vec::new(),
    })?;
    #[cfg(windows)]
    let job = job.and_then(|job| match job.assign(&child) {
        Ok(()) => Some(job),
        Err(error) => {
            tracing::warn!(target: "regia_editor", "cargo runs outside the kill job: {error}");
            None
        }
    });
    // Both pipes drain on their own threads so a chatty build never blocks on a full pipe.
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let status = loop {
        if cancel.is_cancelled() {
            #[cfg(unix)]
            kill_tree(&mut child);
            #[cfg(windows)]
            {
                // Closing the job's last handle ends every process in it (KILL_ON_JOB_CLOSE):
                // cargo and the rustc processes it started. `child.kill` is only the fallback
                // for a child the job could not take.
                drop(job);
                let _ = child.kill();
            }
            #[cfg(not(any(unix, windows)))]
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout.join();
            let _ = stderr.join();
            return Err(BuildFailure::cancelled());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(CANCEL_POLL),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(BuildFailure {
                    summary: format!("cannot wait for cargo: {error}"),
                    diagnostics: Vec::new(),
                });
            }
        }
    };
    Ok(std::process::Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

impl std::fmt::Display for BuildFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.summary)?;
        for diagnostic in self
            .diagnostics
            .iter()
            .filter(|item| item.level == "error")
            .take(8)
        {
            write!(formatter, "\n{}", diagnostic.message)?;
            if let Some(file) = &diagnostic.file {
                write!(formatter, " ({file}")?;
                if let Some(line) = diagnostic.line {
                    write!(formatter, ":{line}")?;
                }
                write!(formatter, ")")?;
            }
        }
        Ok(())
    }
}

/// Where the game crate lives and where artifacts go.
#[derive(Debug, Clone)]
pub struct GameBuild {
    /// Manifest cargo should build (`Cargo.toml` of the workspace or of a standalone copy).
    pub manifest_path: PathBuf,
    /// Package name (`-p`).
    pub package: String,
    /// Cargo target directory. The editor and the game dylib must share it so they link one
    /// copy of each engine rlib.
    pub target_dir: PathBuf,
    /// Destination directory for `.regia/hot/game-<N>-<pid>-<serial>.<ext>`.
    pub hot_dir: PathBuf,
}

/// Result of a successful salted build.
#[derive(Debug, Clone)]
pub struct BuiltGame {
    /// The unique copy under [`GameBuild::hot_dir`].
    pub copied: PathBuf,
    /// Wall time of the cargo invocation.
    pub elapsed: Duration,
    /// Warnings cargo reported.
    pub warnings: Vec<BuildDiagnostic>,
}

/// Builds `game` as a dylib with `-C metadata=hotgen{generation}` and copies it to
/// `.regia/hot/game-{generation}-{pid}-{serial}.<ext>`.
///
/// Also passes `-C prefer-dynamic` so the dylib uses the process's Rust `std`.
///
/// # Errors
///
/// [`BuildFailure`] when cargo cannot be started, exits non-zero, or emits no dylib. The caller
/// must not touch the running world.
pub fn build_salted_dylib(
    build: &GameBuild,
    generation: u32,
    abi_hash: u64,
) -> Result<BuiltGame, BuildFailure> {
    build_salted_dylib_cancellable(build, generation, abi_hash, &BuildCancel::default())
}

/// [`build_salted_dylib`] that stops when `cancel` is set: cargo is killed, and a build that
/// finished before the cancel was seen publishes no copy (the hot dir gets nothing partial).
///
/// # Errors
///
/// As [`build_salted_dylib`], plus [`BuildFailure::cancelled`].
pub fn build_salted_dylib_cancellable(
    build: &GameBuild,
    generation: u32,
    abi_hash: u64,
    cancel: &BuildCancel,
) -> Result<BuiltGame, BuildFailure> {
    let started = std::time::Instant::now();
    let mut command = Command::new("cargo");
    command
        .args(rustc_args(build, generation))
        .env("REGIA_ABI_HASH", abi_hash.to_string())
        .env("CARGO_TERM_COLOR", "never");
    let output = run_cancellable(&mut command, cancel)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let diagnostics = parse_cargo_json(&stdout);
    if !output.status.success() {
        let mut summary = format!("game build failed ({})", output.status);
        if diagnostics.iter().all(|item| item.level != "error") && !stderr.trim().is_empty() {
            summary.push_str(": ");
            summary.push_str(stderr.trim());
        }
        return Err(BuildFailure { summary, diagnostics });
    }
    let Some(artifact) = dylib_artifact(&stdout, &build.package) else {
        return Err(BuildFailure {
            summary: "cargo produced no game dylib".to_string(),
            diagnostics,
        });
    };
    if cancel.is_cancelled() {
        return Err(BuildFailure::cancelled());
    }
    let copied = copy_game_dylib(&artifact, &build.hot_dir, generation).map_err(|error| BuildFailure {
        summary: error,
        diagnostics: Vec::new(),
    })?;
    let warnings = diagnostics
        .into_iter()
        .filter(|item| item.level == "warning")
        .collect();
    Ok(BuiltGame {
        copied,
        elapsed: started.elapsed(),
        warnings,
    })
}

/// On Windows, one build's process tree: a job object with `KILL_ON_JOB_CLOSE` (the same pattern
/// as `regia_platform::child_bind`, ADR-2682). Unix gives the child its own process group and
/// kills the group; Windows has no process groups, so the job object is what ends cargo *and*
/// the rustc processes it started when a build is cancelled.
#[cfg(windows)]
mod build_job {
    // Win32 job-object calls (docs/04-conventions.md § 5, ADR-7340); every block carries a
    // SAFETY note.
    #![allow(unsafe_code)]

    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
    };

    /// The job handle as an integer, so the guard is `Send` and `Sync` (a kernel object
    /// reference, usable from any thread).
    pub struct BuildJob {
        handle: Option<isize>,
    }

    impl BuildJob {
        /// A job whose closing ends everything in it, or `None` when the OS refused.
        #[must_use]
        pub fn new() -> Option<Self> {
            // SAFETY: plain Win32 calls with valid arguments: no security attributes and no
            // name; the limit structure is a zeroed POD with one flag, passed with its own size.
            let job = unsafe {
                let job: HANDLE = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    None
                } else {
                    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                    let ok = SetInformationJobObject(
                        job,
                        JobObjectExtendedLimitInformation,
                        (&raw const info).cast(),
                        u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                            .unwrap_or(u32::MAX),
                    );
                    if ok == 0 {
                        CloseHandle(job);
                        None
                    } else {
                        Some(job as isize)
                    }
                }
            };
            job.map(|handle| Self { handle: Some(handle) })
        }

        /// Puts `child` in the job: when the job closes, `child` ends with everything it started.
        ///
        /// # Errors
        ///
        /// `AssignProcessToJobObject` failed (the caller falls back to killing the child alone).
        pub fn assign(&self, child: &Child) -> std::io::Result<()> {
            let Some(handle) = self.handle else {
                return Ok(());
            };
            // SAFETY: `handle` is the live job handle this guard owns; the child's handle is
            // valid while `child` is borrowed.
            let ok = unsafe { AssignProcessToJobObject(handle as HANDLE, child.as_raw_handle() as HANDLE) };
            if ok == 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
    }

    impl Drop for BuildJob {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                // SAFETY: the guard owns the handle and closes it exactly once; on a cancel this
                // is what kills the whole tree (KILL_ON_JOB_CLOSE), after the owner stopped it.
                unsafe {
                    CloseHandle(handle as HANDLE);
                }
            }
        }
    }
}

/// Kills the child and its process group (`kill -KILL -- -<pgid>`; the child leads it).
#[cfg(unix)]
fn kill_tree(child: &mut std::process::Child) {
    let group = format!("-{}", child.id());
    let _ = Command::new("kill")
        .args(["-KILL", "--", &group])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
}
/// Copies `built` to `hot_dir/game-{generation}-{pid}-{serial}.<ext>`.
///
/// The dynamic loader caches by path, so the name is unique per reload (specs/20 § 3). The
/// process id and a process-wide serial make it unique across hosts and processes sharing one
/// hot dir too: every loaded library stays mapped for the life of its process, and a second
/// editor (or the scripted host) reaching the same generation used to truncate that file
/// (audit 11-03 dylib H2). The copy is written under a temp name and published with a
/// no-replace link, so an existing file is never overwritten.
pub fn copy_game_dylib(built: &Path, hot_dir: &Path, generation: u32) -> Result<PathBuf, String> {
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    std::fs::create_dir_all(hot_dir).map_err(|error| error.to_string())?;
    let pid = std::process::id();
    loop {
        let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dest = hot_dir.join(format!("game-{generation}-{pid}-{serial}.{}", dylib_extension()));
        if dest.exists() {
            continue;
        }
        let tmp = hot_dir.join(format!(".game-{generation}-{pid}-{serial}.tmp"));
        std::fs::copy(built, &tmp)
            .map_err(|error| format!("cannot copy {} to {}: {error}", built.display(), tmp.display()))?;
        match std::fs::hard_link(&tmp, &dest) {
            Ok(()) => {
                let _ = std::fs::remove_file(&tmp);
                return Ok(dest);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(&tmp);
            }
            Err(_) => {
                // No hard links on this file system: the unique name is the guard.
                if let Err(error) = std::fs::rename(&tmp, &dest) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(format!("cannot publish {}: {error}", dest.display()));
                }
                return Ok(dest);
            }
        }
    }
}

/// Hash of the engine dylib in `target_dir/<profile>/deps` (specs/20 § 1).
///
/// A game build that resolves different features relinks that dylib under the running editor.
/// The host refuses the load ("engine changed — restart the editor"). When this directory has
/// no `regia_dylib` artifact, the hash falls back to the engine rlibs so a unit test can still
/// see a changed file.
#[must_use]
pub fn engine_fingerprint(target_dir: &Path) -> u64 {
    regia_framework::extension::stamp::engine_fingerprint(target_dir, profile_dir())
}

/// Cargo profile flag for the game rebuild, matching the profile `engine_fingerprint` watches.
///
/// A release host maps `target/release/libregia_dylib`; the game must link that same engine
/// copy, so it builds in the same profile instead of cargo's default dev profile.
/// The arguments [`build_salted_dylib`] passes to `cargo` (`specs/19` § 5, `specs/20` § 1):
/// `rustc --manifest-path <m> -p <package> --lib --crate-type dylib [--release] --features hot-reload
/// --message-format=json --target-dir <t> -- -C prefer-dynamic -C metadata=hotgen<generation>`.
///
/// `--release` is present only in a release editor build, so the dylib links the editor's own rlibs.
pub(crate) fn rustc_args(build: &GameBuild, generation: u32) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec![
        "rustc".into(),
        "--manifest-path".into(),
        build.manifest_path.clone().into_os_string(),
        "-p".into(),
        build.package.clone().into(),
        "--lib".into(),
        "--crate-type".into(),
        "dylib".into(),
    ];
    args.extend(profile_args().iter().map(std::ffi::OsString::from));
    args.extend([
        "--features".into(),
        "hot-reload".into(),
        "--message-format=json".into(),
        "--target-dir".into(),
        build.target_dir.clone().into_os_string(),
        "--".into(),
        "-C".into(),
        "prefer-dynamic".into(),
        "-C".into(),
        format!("metadata=hotgen{generation}").into(),
    ]);
    args
}

fn profile_args() -> &'static [&'static str] {
    if cfg!(debug_assertions) {
        &[]
    } else {
        &["--release"]
    }
}

fn profile_dir() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

/// The game crate's dylib, not a dependency's.
///
/// `cargo rustc` emits one `compiler-artifact` per crate. `regia_dylib` is also a dylib, and its
/// line often comes first. Matching the target name keeps the copy named after the game.
///
/// The target name and the file name are the crate name: cargo turns a package's `-` into `_`
/// (`my-game` builds target `my_game`, `libmy_game.so`), so the match uses the crate name.
fn dylib_artifact(stdout: &str, package: &str) -> Option<PathBuf> {
    let extension = dylib_extension();
    let package = package.replace('-', "_");
    let package = package.as_str();
    let name_key = format!("\"name\":\"{package}\"");
    for line in stdout.lines() {
        if !line.contains("\"compiler-artifact\"") || !line.contains(&name_key) {
            continue;
        }
        for filename in json_string_array(line, "filenames") {
            if !filename.ends_with(&format!(".{extension}")) {
                continue;
            }
            let path = PathBuf::from(&filename);
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if file_name.contains(package) {
                return Some(path);
            }
        }
    }
    None
}

pub(crate) fn parse_cargo_json(stdout: &str) -> Vec<BuildDiagnostic> {
    let mut diagnostics = Vec::new();
    for line in stdout.lines() {
        if !line.contains("\"compiler-message\"") {
            continue;
        }
        // rustc's JSON object is `message`, `code`, `level`, `spans`, `children`, `rendered`.
        // Read the parent object, so a child diagnostic cannot replace the parent's text.
        // One parse per line: the level decides whether the diagnostic is kept.
        if let Some(diagnostic) = diagnostic_from(line) {
            diagnostics.push(diagnostic);
        }
    }
    diagnostics
}

fn diagnostic_from(line: &str) -> Option<BuildDiagnostic> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let message = value.get("message")?;
    let level = message.get("level").and_then(serde_json::Value::as_str)?;
    if level != "error" && level != "warning" {
        return None;
    }
    let text = message
        .get("message")
        .and_then(serde_json::Value::as_str)?
        .to_string();
    let spans = message.get("spans").and_then(serde_json::Value::as_array);
    let span = spans.and_then(|spans| {
        spans
            .iter()
            .find(|span| span.get("is_primary").and_then(serde_json::Value::as_bool) == Some(true))
            .or_else(|| spans.first())
    });
    let file = span
        .and_then(|span| span.get("file_name"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let line_no = span
        .and_then(|span| span.get("line_start"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    Some(BuildDiagnostic {
        level: level.to_string(),
        message: text,
        file,
        line: line_no,
    })
}

fn json_string_array(line: &str, key: &str) -> Vec<String> {
    let pattern = format!("\"{key}\":[");
    let Some(start) = line.find(&pattern) else {
        return Vec::new();
    };
    let mut rest = &line[start + pattern.len()..];
    let mut values = Vec::new();
    while let Some(quote) = rest.find('"') {
        rest = &rest[quote + 1..];
        let Some(value) = unescape(rest) else {
            break;
        };
        // Advance past the consumed characters. `unescape` stops at the closing quote; find it.
        let Some(end) = find_string_end(rest) else {
            break;
        };
        values.push(value);
        rest = &rest[end + 1..];
        if rest.starts_with(']') {
            break;
        }
    }
    values
}

fn find_string_end(text: &str) -> Option<usize> {
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '"' {
            return Some(index);
        }
    }
    None
}

fn unescape(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '"' {
            return Some(out);
        }
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('/') => out.push('/'),
                Some('u') => out.push(unescape_unicode(&mut chars)?),
                Some(other) => out.push(other),
                None => return None,
            }
        } else {
            out.push(ch);
        }
    }
    None
}

/// One JSON `\uXXXX`, or a surrogate pair (`\uD800\uDC00`) decoded to one scalar.
fn unescape_unicode(chars: &mut std::str::Chars<'_>) -> Option<char> {
    let unit = hex4(chars)?;
    if (0xD800..=0xDBFF).contains(&unit) {
        let next = [chars.next(), chars.next()];
        if next != [Some('\\'), Some('u')] {
            return None;
        }
        let low = hex4(chars)?;
        if !(0xDC00..=0xDFFF).contains(&low) {
            return None;
        }
        let code = 0x10000 + (((unit - 0xD800) << 10) | (low - 0xDC00));
        return char::from_u32(code);
    }
    if (0xDC00..=0xDFFF).contains(&unit) {
        return None;
    }
    char::from_u32(unit)
}

fn hex4(chars: &mut std::str::Chars<'_>) -> Option<u32> {
    let hex: String = chars.by_ref().take(4).collect();
    if hex.len() != 4 {
        return None;
    }
    u32::from_str_radix(&hex, 16).ok()
}

#[cfg(test)]
mod tests {
    // The Windows tree-kill test reads process exit codes through Win32
    // (docs/04-conventions.md § 5 allows a local test opt-out); the block carries a SAFETY note.
    #![allow(unsafe_code)]

    use super::*;

    /// `specs/19` § 7: a cancelled build stops its process (one that would never exit on its own)
    /// and reports [`BUILD_CANCELLED`].
    #[cfg(unix)]
    #[test]
    fn a_cancelled_build_kills_its_process_and_reports_cancelled() {
        let cancel = BuildCancel::default();
        let setter = cancel.clone();
        let canceller = std::thread::spawn(move || setter.cancel());
        let mut command = Command::new("sleep");
        command.arg("100000");
        let failure = run_cancellable(&mut command, &cancel).expect_err("cancelled");
        assert!(failure.is_cancelled(), "{failure}");
        canceller.join().expect("canceller");

        // A token set before the start never spawns anything.
        let mut never = Command::new("definitely-not-a-program-regia");
        assert!(
            run_cancellable(&mut never, &cancel)
                .expect_err("cancelled")
                .is_cancelled()
        );

        // Without a cancel the output comes back as `output()` would give it.
        let mut echo = Command::new("echo");
        echo.arg("built");
        let output = run_cancellable(&mut echo, &BuildCancel::default()).expect("echo");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "built");
    }

    /// `specs/19` § 5 and `specs/20` § 1: the exact cargo command line of a hot-reload build.
    #[test]
    fn the_hot_reload_build_runs_the_spec_cargo_rustc_command() {
        let build = GameBuild {
            manifest_path: PathBuf::from("proj/Cargo.toml"),
            package: "game".into(),
            target_dir: PathBuf::from("proj/target"),
            hot_dir: PathBuf::from("proj/.regia/hot"),
        };
        let args: Vec<String> = rustc_args(&build, 7)
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let mut expected = vec![
            "rustc",
            "--manifest-path",
            "proj/Cargo.toml",
            "-p",
            "game",
            "--lib",
            "--crate-type",
            "dylib",
        ];
        if !cfg!(debug_assertions) {
            expected.push("--release");
        }
        expected.extend([
            "--features",
            "hot-reload",
            "--message-format=json",
            "--target-dir",
            "proj/target",
            "--",
            "-C",
            "prefer-dynamic",
            "-C",
            "metadata=hotgen7",
        ]);
        assert_eq!(args, expected);
    }

    #[test]
    fn cargo_json_extracts_the_dylib_and_the_error() {
        let ext = dylib_extension();
        let line = format!(
            r#"{{"reason":"compiler-artifact","target":{{"name":"hot_fixture"}},"filenames":["/tmp/libhot_fixture.rlib","/tmp/libhot_fixture.{ext}"]}}"#
        );
        let engine = format!(
            r#"{{"reason":"compiler-artifact","target":{{"name":"regia_dylib"}},"filenames":["/tmp/libregia_dylib.{ext}"]}}"#
        );
        let artifact = dylib_artifact(&format!("{engine}\n{line}"), "hot_fixture").expect("dylib");
        assert!(
            artifact.ends_with(format!("libhot_fixture.{ext}")),
            "{artifact:?}"
        );

        let message = r#"{"reason":"compiler-message","message":{"level":"error","message":"cannot find type `Mover`","spans":[{"file_name":"src/lib.rs","line_start":12}]}}"#;
        let diagnostics = parse_cargo_json(message);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].message, "cannot find type `Mover`");
        assert_eq!(diagnostics[0].file.as_deref(), Some("src/lib.rs"));
        assert_eq!(diagnostics[0].line, Some(12));

        // A warning line parses too (one parse, the level keeps it), and a non-error,
        // non-warning level is skipped.
        let warning = r#"{"reason":"compiler-message","message":{"level":"warning","message":"unused variable","spans":[{"file_name":"src/lib.rs","line_start":3}]}}"#;
        let help =
            r#"{"reason":"compiler-message","message":{"level":"help","message":"a child","spans":[]}}"#;
        let diagnostics = parse_cargo_json(&format!("{warning}\n{help}"));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].level, "warning");
        assert_eq!(diagnostics[0].message, "unused variable");
        assert_eq!(diagnostics[0].line, Some(3));
    }

    /// Review rc1-rev-editor-k finding 3: a hyphenated package (`my-game`, the usual project name)
    /// builds target `my_game` and `libmy_game.<ext>`; the artifact must still be found, and a
    /// dependency's dylib must still not be taken for it.
    #[test]
    fn a_hyphenated_package_finds_its_underscored_dylib() {
        let ext = dylib_extension();
        let engine = format!(
            r#"{{"reason":"compiler-artifact","package_id":"path+file:///w/regia_dylib#0.5.0","target":{{"name":"regia_dylib"}},"filenames":["/t/debug/deps/libregia_dylib.{ext}"]}}"#
        );
        let game = format!(
            r#"{{"reason":"compiler-artifact","package_id":"path+file:///w/my-game#0.1.0","target":{{"name":"my_game"}},"filenames":["/t/debug/libmy_game.rlib","/t/debug/libmy_game.{ext}"]}}"#
        );
        let artifact = dylib_artifact(&format!("{engine}\n{game}"), "my-game").expect("dylib");
        assert!(artifact.ends_with(format!("libmy_game.{ext}")), "{artifact:?}");
        assert_eq!(dylib_artifact(&engine, "my-game"), None);
    }

    /// rustc's JSON diagnostic lists the parent message, then `level`, then `spans`, then
    /// `children`. The parent text is what the editor shows, including `\u` escapes.
    #[test]
    fn cargo_json_reads_the_parent_diagnostic_and_unicode_escapes() {
        let ext = dylib_extension();
        let message = r#"{"reason":"compiler-message","message":{"message":"cannot find type `Mov\u00e9` in this scope","code":null,"level":"error","spans":[{"file_name":"src/mover.rs","line_start":4,"is_primary":true}],"children":[{"message":"consider importing this struct","level":"help","spans":[{"file_name":"src/child.rs","line_start":9}]}],"rendered":"error"}}"#;
        let diagnostics = parse_cargo_json(message);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].level, "error");
        assert_eq!(diagnostics[0].message, "cannot find type `Mové` in this scope");
        assert_eq!(diagnostics[0].file.as_deref(), Some("src/mover.rs"));
        assert_eq!(diagnostics[0].line, Some(4));

        let artifact_line = format!(
            r#"{{"reason":"compiler-artifact","target":{{"name":"hot_fixture"}},"filenames":["/tmp/libhot_fixture\u00e9.{ext}"]}}"#
        );
        let artifact = dylib_artifact(&artifact_line, "hot_fixture").expect("dylib");
        assert!(
            artifact.to_string_lossy().contains('é'),
            "filename was not unescaped: {artifact:?}"
        );
    }

    #[test]
    fn fingerprint_changes_when_an_engine_rlib_changes() {
        let dir = regia_test_utils::unique_temp_dir("hot_fp");
        let deps = dir.join(profile_dir()).join("deps");
        std::fs::create_dir_all(&deps).expect("deps");
        let rlib = deps.join("libregia_ecs-aaaa.rlib");
        std::fs::write(&rlib, b"one").expect("write");
        let first = engine_fingerprint(&dir);
        std::fs::write(&rlib, b"two").expect("rewrite");
        // mtime resolution can be coarse; length changed, so the hash must change.
        let second = engine_fingerprint(&dir);
        assert_ne!(first, second);
        assert_ne!(first, 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fingerprint_ignores_mtime_only_changes() {
        let dir = regia_test_utils::unique_temp_dir("hot_fp_mtime");
        let deps = dir.join(profile_dir()).join("deps");
        std::fs::create_dir_all(&deps).expect("deps");
        let dylib = deps.join(format!("libregia_dylib-bbbb.{}", dylib_extension()));
        std::fs::write(&dylib, b"same bytes").expect("write");
        let first = engine_fingerprint(&dir);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&dylib)
            .expect("open");
        file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000))
            .expect("set mtime");
        drop(file);
        assert_eq!(first, engine_fingerprint(&dir));
        std::fs::write(&dylib, b"same bytez").expect("rewrite");
        assert_ne!(first, engine_fingerprint(&dir), "same length, different bytes");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn game_rebuild_profile_matches_the_fingerprint_profile() {
        let release = profile_dir() == "release";
        assert_eq!(profile_args().contains(&"--release"), release);
    }

    #[test]
    fn fingerprint_prefers_the_engine_dylib_over_rlibs() {
        let dir = regia_test_utils::unique_temp_dir("hot_fp_dylib");
        let deps = dir.join(profile_dir()).join("deps");
        std::fs::create_dir_all(&deps).expect("deps");
        let rlib = deps.join("libregia_ecs-aaaa.rlib");
        std::fs::write(&rlib, b"one").expect("write");
        let dylib = deps.join(format!("libregia_dylib-bbbb.{}", dylib_extension()));
        std::fs::write(&dylib, b"dylib-a").expect("write");
        let first = engine_fingerprint(&dir);
        std::fs::write(&rlib, b"two-two").expect("rewrite rlib");
        let second = engine_fingerprint(&dir);
        assert_eq!(first, second, "an rlib change must not move a dylib fingerprint");
        std::fs::write(&dylib, b"dylib-bb").expect("rewrite dylib");
        let third = engine_fingerprint(&dir);
        assert_ne!(first, third);
        assert_ne!(first, 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Audit 11-03 dylib H2 / editor L3: two hosts (two editors, or the editor and the scripted
    /// host) on one hot dir both reach generation 1; the second copy truncated the library the
    /// first process had mapped.
    #[test]
    fn two_hosts_on_one_hot_dir_never_share_a_copy() {
        let dir = regia_test_utils::unique_temp_dir("hot_copy_two_hosts");
        let ext = dylib_extension();
        let first_source = dir.join(format!("liba.{ext}"));
        let second_source = dir.join(format!("libb.{ext}"));
        std::fs::write(&first_source, b"generation one of host A").expect("write a");
        std::fs::write(&second_source, b"host B").expect("write b");
        let hot = dir.join("hot");
        let first = copy_game_dylib(&first_source, &hot, 1).expect("copy a");
        let second = copy_game_dylib(&second_source, &hot, 1).expect("copy b");
        assert_ne!(first, second, "both hosts got the same copy name");
        assert_eq!(
            std::fs::read(&first).expect("read a"),
            b"generation one of host A",
            "the mapped copy was overwritten"
        );
        assert_eq!(std::fs::read(&second).expect("read b"), b"host B");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn copy_uses_the_spec_name() {
        let dir = regia_test_utils::unique_temp_dir("hot_copy");
        std::fs::create_dir_all(&dir).expect("dir");
        let source = dir.join(format!("libgame.{}", dylib_extension()));
        std::fs::write(&source, b"dylib").expect("write");
        let hot = dir.join("hot");
        let copied = copy_game_dylib(&source, &hot, 4).expect("copy");
        // `game-<N>-<pid>-<serial>.<ext>` (ADR-2396: process-unique copy names).
        let name = copied
            .file_name()
            .and_then(|name| name.to_str())
            .expect("utf-8 name")
            .to_string();
        let prefix = format!("game-4-{}-", std::process::id());
        assert!(name.starts_with(&prefix), "{name}");
        assert!(name.ends_with(&format!(".{}", dylib_extension())), "{name}");
        assert_eq!(std::fs::read(&copied).expect("read"), b"dylib");
        assert!(
            std::fs::read_dir(&hot)
                .expect("hot dir")
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().ends_with(".tmp")),
            "no temp file left"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `specs/19` § 7 (ADR-5615 Status: "the game build has no cancel"): a cancelled build kills
    /// its child and the child's process group (cargo's compilers), and returns no output.
    #[cfg(target_os = "linux")]
    #[test]
    fn cancel_build_kills_the_running_child() {
        let dir = regia_test_utils::unique_temp_dir("build-cancel-kill");
        std::fs::create_dir_all(&dir).expect("dir");
        let cancel = BuildCancel::new();
        let watcher = cancel.clone();
        let mut command = Command::new("sh");
        // A child that would run for a minute, with a grandchild in its group; both write their
        // pids (atomically, by rename) before waiting.
        command.current_dir(&dir).args([
            "-c",
            "sleep 60 & echo $! > g.tmp && mv g.tmp g; echo $$ > p.tmp && mv p.tmp p; wait",
        ]);
        let pids = dir.clone();
        let canceller = std::thread::spawn(move || {
            // Cancel once both processes run (a condition, not a sleep).
            while !(pids.join("p").is_file() && pids.join("g").is_file()) {
                std::thread::yield_now();
            }
            watcher.cancel();
        });
        let result = run_cancellable(&mut command, &cancel);
        let _ = canceller.join();
        assert!(
            result.is_err_and(|failure| failure.is_cancelled()),
            "a cancelled run reports cancelled"
        );
        let alive = |name: &str| {
            let pid = std::fs::read_to_string(dir.join(name)).expect("pid");
            // Gone, or a zombie waiting for its reaper: either way it no longer runs.
            std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()))
                .is_ok_and(|stat| !stat.split_whitespace().nth(2).is_some_and(|state| state == "Z"))
        };
        assert!(!alive("p"), "the child was killed");
        // The group kill is asynchronous for the orphaned grandchild: wait for that condition.
        let mut spins = 0_u64;
        while alive("g") && spins < 50_000_000 {
            std::thread::yield_now();
            spins += 1;
        }
        assert!(!alive("g"), "the child's process group was killed");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The Windows half of "a cancelled build kills its tree" (the known issue of
    /// v0.1.28-v06-autosave-recovery): the child (this test binary re-run) starts a grandchild,
    /// and cancel ends both — the child runs in a job object with `KILL_ON_JOB_CLOSE`, so closing
    /// the job kills cargo and the rustc processes it started. Run on a Windows host:
    /// `cargo test -p regia_editor --lib hot_reload::build_game::tests::cancel_build_kills_the_whole_tree_on_windows`.
    #[cfg(windows)]
    #[test]
    fn cancel_build_kills_the_whole_tree_on_windows() {
        let dir = regia_test_utils::unique_temp_dir("build-cancel-tree");
        let cancel = BuildCancel::new();
        let watcher = cancel.clone();
        let mut command = Command::new(std::env::current_exe().expect("the test binary"));
        command
            .args([
                "--exact",
                "build_cancel_tree_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("REGIA_BUILD_CANCEL_TREE", &dir);
        let watch_dir = dir.clone();
        let canceller = std::thread::spawn(move || {
            // Cancel once both processes run (a condition, not a sleep).
            while !(watch_dir.join("p").is_file() && watch_dir.join("g").is_file()) {
                std::thread::yield_now();
            }
            watcher.cancel();
        });
        let result = run_cancellable(&mut command, &cancel);
        let _ = canceller.join();
        assert!(
            result.is_err_and(|failure| failure.is_cancelled()),
            "a cancelled run reports cancelled"
        );
        let child_pid = std::fs::read_to_string(dir.join("p")).expect("child pid");
        let grandchild_pid = std::fs::read_to_string(dir.join("g")).expect("grandchild pid");
        assert!(!pid_running(child_pid.trim()), "the child was killed");
        // The job kill is asynchronous for the rest of the tree: wait for that condition.
        let mut spins = 0_u64;
        while pid_running(grandchild_pid.trim()) && spins < 50_000_000 {
            std::thread::yield_now();
            spins += 1;
        }
        assert!(
            !pid_running(grandchild_pid.trim()),
            "the grandchild was killed with the tree"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Whether `pid` still runs: `OpenProcess` fails for a pid that does not exist, and a handle
    /// whose exit code is not `STILL_ACTIVE` belongs to a process that ended (pid reuse shows
    /// the new process, which is fine: it is not ours).
    #[cfg(windows)]
    fn pid_running(pid_text: &str) -> bool {
        let Ok(pid) = pid_text.trim().parse::<u32>() else {
            return false;
        };
        // SAFETY: `OpenProcess` with `PROCESS_QUERY_LIMITED_INFORMATION` on a pid read from our
        // own pid file; the handle is closed on every path.
        unsafe {
            use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
            use windows_sys::Win32::System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            };
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code = 0_u32;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            ok != 0 && code == STILL_ACTIVE as u32
        }
    }

    /// The child of [`cancel_build_kills_the_whole_tree_on_windows`]: starts a grandchild and
    /// blocks, like cargo waiting on its rustc. Ends with the job.
    #[cfg(windows)]
    #[test]
    fn build_cancel_tree_child() {
        let Some(dir) = std::env::var_os("REGIA_BUILD_CANCEL_TREE").map(PathBuf::from) else {
            return;
        };
        let mut grandchild = Command::new(std::env::current_exe().expect("the test binary"))
            .args([
                "--exact",
                "build_cancel_tree_grandchild",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("REGIA_BUILD_CANCEL_TREE", &dir)
            .spawn()
            .expect("grandchild");
        crate::atomic_write::write_atomic(&dir.join("g"), grandchild.id().to_string().as_bytes())
            .expect("grandchild pid");
        crate::atomic_write::write_atomic(&dir.join("p"), std::process::id().to_string().as_bytes())
            .expect("own pid");
        // What cargo does: wait on its compilers.
        let _ = grandchild.wait();
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }

    /// The grandchild: a stand-in for a rustc process. Ends with the job.
    #[cfg(windows)]
    #[test]
    fn build_cancel_tree_grandchild() {
        if std::env::var_os("REGIA_BUILD_CANCEL_TREE").is_none() {
            return;
        }
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
}
