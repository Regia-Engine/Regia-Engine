// SPDX-License-Identifier: MPL-2.0
//! Launch standalone and Launch on Android device (`docs/specs/19-editor.md` § 4, ADR-8980,
//! `book/agents/editor-launch.md`).
//!
//! Play-in-editor runs the game inside the editor. A *launch* runs it the way a player does, as its
//! own process, built from the open project:
//!
//! - **Standalone**: the project is cooked for the host platform (incrementally, with the same
//!   [`regia_asset_pipeline::cook_project`] call `regia run` makes), the launcher crate is built
//!   with `cargo build --message-format=json-render-diagnostics` (the binary is the executable
//!   cargo reports), and the binary runs in the project directory with the open level as its start
//!   level (`REGIA_START_LEVEL`, a play copy of the level as it is in the editor, unsaved edits
//!   included; removed when the launch ends).
//! - **Android**: `adb devices -l` lists the attached devices (one usable device is picked;
//!   several need a serial), `regia package --platform android --profile dev --format apk --json`
//!   builds the debug APK (for the device's ABI when the manifest lists it), then
//!   `adb install -r`, `adb shell am start -n <id>/.RegiaActivity`, `adb shell pidof <id>`, and
//!   `adb logcat --pid <pid>` streams the app's log.
//!
//! Every line a step or the game writes goes to the Output Log under the target [`LOG_TARGET`],
//! prefixed `[launch <id>]`, and to the launch's tail (`editor.launch_status`). The steps run on a
//! worker thread; the editor frame never waits on a build. **Stop** kills every child of the launch
//! with its whole process tree (on Unix each child leads its own process group and the group is
//! killed; on Windows the children run in one job object with `KILL_ON_JOB_CLOSE`, the pattern of
//! the game build's cancel, ADR-7340; a Windows child starts suspended until it is in the job,
//! ADR-7548) and reaps them before it returns; once the launch has started the Android app, Stop
//! also sends `adb shell am force-stop <id>` (the id passed the package-name grammar first). The
//! children are bound to the editor's lifetime as well (`regia_platform::child_bind`): the editor
//! ending ends them.
//!
//! `adb` is found the way `regia doctor` finds it: `$ANDROID_HOME`, `$ANDROID_SDK_ROOT` or the
//! Android Studio default SDK's `platform-tools/adb`, else `adb` on `PATH`. [`LaunchTools`] can
//! override every program and the `PATH` the lookups use (tests put fakes first there).

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead as _, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// The Output Log target of every launch line (each prefixed `[launch <id>]`).
pub const LOG_TARGET: &str = "regia_editor::launch";
/// Lines kept per launch for `editor.launch_status`.
pub const TAIL_LINES: usize = 400;
/// Lines of the failed step's output an error carries (`log_tail`).
pub const ERROR_TAIL_LINES: usize = 40;
/// The Android activity every Regia Engine APK declares (`regia_cli::android::ACTIVITY_CLASS`).
pub const ANDROID_ACTIVITY: &str = "RegiaActivity";

/// How often a running child is checked for exit or Stop.
const POLL: Duration = Duration::from_millis(25);
/// How long a short adb call (`devices`, `force-stop`) may take before it is killed.
const SHORT_CALL: Duration = Duration::from_secs(20);
/// How long the app may take to show up in `pidof` after `am start`.
const PID_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a finished child's pipes may stay open (a daemon it started holds them).
const PIPE_GRACE: Duration = Duration::from_secs(5);
/// The longest `editor.launch { wait: true }` holds the editor frame (its handler runs on the editor
/// thread); a launch that has neither run nor failed by then answers `pending: true` (ADR-7548).
pub const WAIT_BOUND_MS: u64 = 1000;

/// The protocol's error codes (lowercase ascii, listed on `book/agents/editor-launch.md`).
pub mod codes {
    /// No project is open.
    pub const NO_PROJECT: &str = "launch_no_project";
    /// `target` is not `standalone` or `android`.
    pub const BAD_TARGET: &str = "launch_bad_target";
    /// A launch is building or running; stop it first.
    pub const BUSY: &str = "launch_busy";
    /// `adb` was not found (no SDK platform-tools, nothing on `PATH`).
    pub const NO_ADB: &str = "launch_no_adb";
    /// No usable device is attached, or the named serial is not one.
    pub const NO_DEVICE: &str = "launch_no_device";
    /// Several devices are attached and none was named.
    pub const DEVICE_AMBIGUOUS: &str = "launch_device_ambiguous";
    /// `Regia.toml` has no `[project] id` (the Android application id).
    pub const NO_APP_ID: &str = "launch_no_app_id";
    /// `[project] id` is not an Android package name (dot-separated segments of ASCII letters,
    /// digits and `_`, each starting with a letter; at least two; no Java keyword). It reaches the
    /// device's shell, so nothing else is accepted (ADR-7548).
    pub const BAD_APP_ID: &str = "launch_bad_app_id";
    /// The cook, the cargo build or the APK packaging failed (the error carries the log tail).
    pub const BUILD_FAILED: &str = "launch_build_failed";
    /// `adb install` failed.
    pub const INSTALL_FAILED: &str = "launch_install_failed";
    /// The game or the app did not start (`am start` refused, no pid, the binary did not run).
    pub const START_FAILED: &str = "launch_start_failed";
    /// A program of the launch could not be run.
    pub const SPAWN_FAILED: &str = "launch_spawn_failed";
    /// Retired (ADR-7548): never answered any more. `editor.launch { wait: true }` waits at most
    /// a short bound and then answers `pending: true` instead of this error.
    pub const TIMEOUT: &str = "launch_timeout";
    /// An extension's before-launch hook aborted the launch (ADR-7791); the message names it.
    pub const HOOK_ABORTED: &str = "launch_hook_aborted";
    /// A saved profile is invalid or cannot be executed by this launcher.
    pub const PROFILE_INVALID: &str = "launch_profile_invalid";
    /// A package-off Android profile: there is no artifact-reuse policy to start (ADR-8472).
    pub const PROFILE_UNSUPPORTED: &str = "launch_profile_unsupported";

    /// Every code, for the book page and its test.
    pub const ALL: &[&str] = &[
        NO_PROJECT,
        BAD_TARGET,
        BUSY,
        NO_ADB,
        NO_DEVICE,
        DEVICE_AMBIGUOUS,
        NO_APP_ID,
        BAD_APP_ID,
        BUILD_FAILED,
        INSTALL_FAILED,
        START_FAILED,
        SPAWN_FAILED,
        TIMEOUT,
        HOOK_ABORTED,
        PROFILE_INVALID,
        PROFILE_UNSUPPORTED,
    ];
}

/// Where a launch runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LaunchTarget {
    /// A desktop dev build on this machine.
    #[default]
    Standalone,
    /// The debug APK on an attached Android device.
    Android,
}

impl LaunchTarget {
    /// `standalone` or `android`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standalone => "standalone",
            Self::Android => "android",
        }
    }

    /// The target called `name`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "standalone" => Some(Self::Standalone),
            "android" => Some(Self::Android),
            _ => None,
        }
    }
}

/// Where a launch is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchState {
    /// Cooking, building or packaging.
    Building,
    /// `adb install`.
    Installing,
    /// Starting the game or the app.
    Starting,
    /// The game runs (standalone) or logcat streams the app (Android).
    Running,
    /// Stopped by the editor (Stop, `editor.launch_stop`, the editor closing).
    Stopped,
    /// The game (or logcat) ended on its own.
    Exited,
    /// A step failed; see the launch's error.
    Failed,
}

impl LaunchState {
    /// `building`, `installing`, `starting`, `running`, `stopped`, `exited` or `failed`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Building => "building",
            Self::Installing => "installing",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Exited => "exited",
            Self::Failed => "failed",
        }
    }

    /// Whether the launch is still going (building, installing, starting or running).
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Building | Self::Installing | Self::Starting | Self::Running
        )
    }
}

/// A refused or failed launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchError {
    /// One of [`codes`].
    pub code: &'static str,
    /// Human text.
    pub message: String,
    /// The last lines of the failed step's output (empty for a refusal).
    pub log_tail: Vec<String>,
}

impl LaunchError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            log_tail: Vec::new(),
        }
    }

    /// The protocol error: `data.code`, `data.hint` and `data.log_tail`.
    #[must_use]
    pub fn rpc(&self) -> RpcError {
        let hint = match self.code {
            codes::NO_PROJECT => "open a project first",
            codes::BAD_TARGET => "target is \"standalone\" or \"android\"",
            codes::BUSY => "editor.launch_stop first",
            codes::NO_ADB => "install the SDK platform-tools and set ANDROID_HOME (`regia doctor` checks it)",
            codes::NO_DEVICE => "attach and authorize a device (`editor.launch_devices` lists them)",
            codes::DEVICE_AMBIGUOUS => "pass `device` with a serial from editor.launch_devices",
            codes::NO_APP_ID => "set [project] id in Regia.toml",
            codes::BAD_APP_ID => "set [project] id in Regia.toml to a package name like com.example.mygame",
            codes::BUILD_FAILED => "fix the build errors in log_tail and launch again",
            codes::INSTALL_FAILED => "see log_tail; uninstall an APK signed with another key first",
            _ => "see book/agents/editor-launch.md",
        };
        let mut error = RpcError::new(INVALID_PARAMS, self.code, self.message.clone(), hint);
        if let Some(Value::Object(data)) = error.data.as_mut() {
            data.insert("log_tail".into(), json!(self.log_tail));
        }
        error
    }

    fn to_json(&self) -> Value {
        json!({ "code": self.code, "message": self.message, "log_tail": self.log_tail })
    }
}

/// One line of `adb devices -l`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdbDevice {
    /// The serial (`-s`).
    pub serial: String,
    /// `device` (usable), `unauthorized`, `offline`, ...
    pub state: String,
    /// The `model:` field, when adb printed one.
    pub model: Option<String>,
}

impl AdbDevice {
    /// Whether adb can install to it (state `device`).
    #[must_use]
    pub fn usable(&self) -> bool {
        self.state == "device"
    }
}

/// Parses `adb devices -l`: one device per line after the `List of devices attached` header.
#[must_use]
pub fn parse_adb_devices(text: &str) -> Vec<AdbDevice> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("List of devices") && !line.starts_with('*'))
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let serial = fields.next()?.to_owned();
            let state = fields.next()?.to_owned();
            let model = fields
                .filter_map(|f| f.strip_prefix("model:"))
                .map(str::to_owned)
                .next();
            Some(AdbDevice { serial, state, model })
        })
        .collect()
}

/// The device a launch uses: `wanted` when it is listed and usable, else the only usable one.
///
/// # Errors
///
/// [`codes::NO_DEVICE`] or [`codes::DEVICE_AMBIGUOUS`].
pub fn choose_device(devices: &[AdbDevice], wanted: Option<&str>) -> Result<String, LaunchError> {
    let usable: Vec<&AdbDevice> = devices.iter().filter(|d| d.usable()).collect();
    if let Some(serial) = wanted {
        return match devices.iter().find(|d| d.serial == serial) {
            Some(d) if d.usable() => Ok(d.serial.clone()),
            Some(d) => Err(LaunchError::new(
                codes::NO_DEVICE,
                format!("device {serial} is {}, not ready", d.state),
            )),
            None => Err(LaunchError::new(
                codes::NO_DEVICE,
                format!("device {serial} is not attached"),
            )),
        };
    }
    match usable.as_slice() {
        [] => Err(LaunchError::new(
            codes::NO_DEVICE,
            "no Android device is attached and authorized",
        )),
        [one] => Ok(one.serial.clone()),
        several => Err(LaunchError::new(
            codes::DEVICE_AMBIGUOUS,
            format!(
                "{} devices are attached ({}); name one",
                several.len(),
                several
                    .iter()
                    .map(|d| d.serial.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

/// Whether cargo's `package_id` names `package`: `<source>#<name>@<version>`,
/// `<source>/<name>#<version>` (the name equals the directory name) or the older
/// `<name> <version> (<source>)`.
fn package_id_names(id: &str, package: &str) -> bool {
    // The old spelling is the only one with a space (its source may hold a `#` of its own).
    if let Some((name, _)) = id.split_once(' ') {
        return name == package;
    }
    match id.rsplit_once('#') {
        Some((_, fragment)) if fragment.contains('@') => {
            fragment.split_once('@').is_some_and(|(name, _)| name == package)
        }
        Some((source, _)) => {
            let path = source.split_once('?').map_or(source, |(path, _)| path);
            path.trim_end_matches('/').rsplit('/').next() == Some(package)
        }
        None => false,
    }
}

/// The executable `cargo build --message-format=json*` built for `package`: the last
/// `compiler-artifact` with an `executable` whose package id or target names `package`. `None`
/// when no artifact belongs to `package`; another package's binary is never taken for it
/// (ADR-7548).
#[must_use]
pub fn parse_cargo_executable(stdout: &str, package: &str) -> Option<PathBuf> {
    let mut named = None;
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if message["reason"] != "compiler-artifact" {
            continue;
        }
        let Some(exe) = message["executable"].as_str() else {
            continue;
        };
        let names_it = message["target"]["name"] == package
            || message["package_id"]
                .as_str()
                .is_some_and(|id| package_id_names(id, package));
        if names_it {
            named = Some(PathBuf::from(exe));
        }
    }
    named
}

/// The `artifact` of `regia package --json`'s result line (the last JSON object with `ok: true`).
#[must_use]
pub fn parse_package_artifact(stdout: &str) -> Option<PathBuf> {
    stdout.lines().rev().find_map(|line| {
        let value: Value = serde_json::from_str(line.trim()).ok()?;
        (value["ok"] == true)
            .then(|| value["artifact"].as_str().map(PathBuf::from))
            .flatten()
    })
}

/// The first pid of `adb shell pidof` output.
#[must_use]
pub fn parse_pidof(output: &str) -> Option<u32> {
    output.split_whitespace().next()?.parse().ok()
}

/// Where the launcher looks for the Android SDK.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SdkSource {
    /// `$ANDROID_HOME`, `$ANDROID_SDK_ROOT`, then the Android Studio default (what `regia doctor`
    /// checks).
    #[default]
    Environment,
    /// This SDK directory.
    Dir(PathBuf),
    /// No SDK: `adb` only from the `adb` field of [`LaunchTools`] or `PATH`.
    None,
}

/// The programs a launch runs. Every field defaults to the normal lookup.
#[derive(Debug, Clone, Default)]
pub struct LaunchTools {
    /// The `regia` CLI that packages the APK (default: [`crate::bake_runner::cli_program`]).
    pub cli: Option<PathBuf>,
    /// `cargo` (default: `cargo` on the launch `PATH`).
    pub cargo: Option<PathBuf>,
    /// `adb` (default: the SDK's `platform-tools`, else `PATH`).
    pub adb: Option<PathBuf>,
    /// Where the SDK is looked for.
    pub sdk: SdkSource,
    /// The `PATH` lookups search and every child gets (default: the editor's).
    pub path_env: Option<OsString>,
    /// Build the standalone game with `--release`.
    pub release: bool,
}

fn env_path(name: &str) -> Option<PathBuf> {
    regia_core::brand::env_var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

impl LaunchTools {
    fn path_var(&self) -> Option<OsString> {
        self.path_env.clone().or_else(|| std::env::var_os("PATH"))
    }

    /// `name` (with the platform's executable suffix) in the first `PATH` directory that has it.
    #[must_use]
    pub fn find_on_path(&self, name: &str) -> Option<PathBuf> {
        let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
        let path = self.path_var()?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(&file))
            .find(|candidate| candidate.is_file())
    }

    /// The Android SDK directory, when one exists.
    #[must_use]
    pub fn sdk_dir(&self) -> Option<PathBuf> {
        let sdk = match &self.sdk {
            SdkSource::None => return None,
            SdkSource::Dir(dir) => Some(dir.clone()),
            SdkSource::Environment => env_path("ANDROID_HOME")
                .or_else(|| env_path("ANDROID_SDK_ROOT"))
                .or_else(|| {
                    let home = env_path("HOME").or_else(|| env_path("USERPROFILE"))?;
                    Some(if cfg!(windows) {
                        home.join("AppData").join("Local").join("Android").join("Sdk")
                    } else if cfg!(target_os = "macos") {
                        home.join("Library").join("Android").join("sdk")
                    } else {
                        home.join("Android").join("Sdk")
                    })
                }),
        };
        sdk.filter(|dir| dir.is_dir())
    }

    /// `adb`: the override, the SDK's `platform-tools/adb`, else `adb` on `PATH`.
    #[must_use]
    pub fn adb_program(&self) -> Option<PathBuf> {
        if let Some(adb) = &self.adb {
            return Some(adb.clone());
        }
        let exe = format!("adb{}", std::env::consts::EXE_SUFFIX);
        self.sdk_dir()
            .map(|sdk| sdk.join("platform-tools").join(&exe))
            .filter(|p| p.is_file())
            .or_else(|| self.find_on_path("adb"))
    }

    fn cargo_program(&self) -> PathBuf {
        self.cargo
            .clone()
            .or_else(|| self.find_on_path("cargo"))
            .unwrap_or_else(|| PathBuf::from("cargo"))
    }

    fn cli_program(&self) -> PathBuf {
        self.cli.clone().unwrap_or_else(crate::bake_runner::cli_program)
    }

    /// A command for `program` with the launch `PATH`.
    fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut cmd = Command::new(program);
        if let Some(path) = &self.path_env {
            cmd.env("PATH", path);
        }
        cmd
    }

    fn adb(&self, adb: &Path, device: &str) -> Command {
        let mut cmd = self.command(adb);
        cmd.args(["-s", device]);
        cmd
    }
}

// ---------------------------------------------------------------------------
// Children
// ---------------------------------------------------------------------------

/// Prepares `cmd` to run as a launch child: no stdin, piped output, its own process group (Unix),
/// bound to the editor's lifetime.
fn prepare_child(cmd: &mut Command) {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    regia_platform::child_bind::bind_to_parent(cmd);
}

/// Kills `child` with its process tree: on Unix its process group (`killpg`, no program run; it
/// leads the group, and it is not reaped yet, so the id is still its own), then the child itself. On
/// Windows the launch's job object is closed first by the caller; `kill` is the fallback for a
/// child the job could not take.
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        let _ = regia_platform::child_bind::kill_process_group(child.id());
    }
    let _ = child.kill();
}

/// What a pipe reader does with each line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sink {
    /// To the Output Log and the tail.
    Log,
    /// Kept for the step only (cargo's JSON).
    Collect,
    /// Both.
    LogAndCollect,
}

/// Reads `stream` line by line on its own thread; collected lines arrive on the returned channel,
/// which disconnects when the stream ends (so [`drain`] also waits for a logged-only stream).
fn read_lines(
    stream: impl Read + Send + 'static,
    sink: Sink,
    log: Option<LineLog>,
) -> Option<Receiver<String>> {
    let (tx, rx) = channel();
    let collect = matches!(sink, Sink::Collect | Sink::LogAndCollect);
    let spawned = std::thread::Builder::new()
        .name("launch-pipe".into())
        .spawn(move || {
            let mut reader = std::io::BufReader::new(stream);
            let mut bytes = Vec::new();
            loop {
                bytes.clear();
                match reader.read_until(b'\n', &mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = String::from_utf8_lossy(&bytes);
                let line = line.trim_end_matches(['\n', '\r']).to_owned();
                if sink != Sink::Collect
                    && let Some(log) = &log
                {
                    log.line(&line);
                }
                if collect {
                    // A receiver that stopped waiting must not stop the reads: the child would
                    // block on a full pipe.
                    let _ = tx.send(line);
                }
            }
        });
    match spawned {
        Ok(_) => Some(rx),
        Err(e) => {
            tracing::warn!(target: LOG_TARGET, "launch: cannot read a child's output: {e}");
            None
        }
    }
}

/// Takes every collected line until the writer side closes, waiting at most [`PIPE_GRACE`] for a
/// pipe a daemon the child started still holds.
fn drain(rx: Option<Receiver<String>>) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(rx) = rx else {
        return lines;
    };
    let deadline = Instant::now() + PIPE_GRACE;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => lines.push(line),
            Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Timeout) => break,
        }
    }
    lines
}

/// Runs a short command to completion (killed after `timeout`), collecting both outputs.
fn run_short(mut cmd: Command, timeout: Duration) -> std::io::Result<(Option<ExitStatus>, String, String)> {
    prepare_child(&mut cmd);
    let mut child = cmd.spawn()?;
    let out = child
        .stdout
        .take()
        .and_then(|s| read_lines(s, Sink::Collect, None));
    let err = child
        .stderr
        .take()
        .and_then(|s| read_lines(s, Sink::Collect, None));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            _ => {
                kill_tree(&mut child);
                let _ = child.wait();
                break None;
            }
        }
    };
    Ok((status, drain(out).join("\n"), drain(err).join("\n")))
}

// ---------------------------------------------------------------------------
// The shared launch state
// ---------------------------------------------------------------------------

/// A running child of the launch.
struct Tracked {
    token: u64,
    role: &'static str,
    child: Child,
}

/// What the worker and the editor share.
struct Inner {
    state: LaunchState,
    step: String,
    steps: Vec<String>,
    error: Option<LaunchError>,
    tail: VecDeque<String>,
    step_tail: VecDeque<String>,
    lines: u64,
    pid: Option<u32>,
    exit_code: Option<i32>,
    artifact: Option<PathBuf>,
    children: Vec<Tracked>,
    next_token: u64,
    stop: bool,
    /// Android: `am start` was (or is about to be) sent, so the app on the device is this launch's
    /// and Stop force-stops it. Before that an app with the id is an earlier install's (ADR-7548).
    app_started: bool,
    /// Windows: the job object every child joins (closing it ends their trees).
    group: Option<regia_platform::child_bind::ChildGroup>,
    /// Android: the host port `adb forward` gave the device game's agent port (ADR-8472); removed
    /// when the launch ends.
    forward: Option<u16>,
}

struct Shared {
    inner: Mutex<Inner>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Logs lines of one launch (cheap to clone into reader threads).
#[derive(Clone)]
struct LineLog {
    id: u32,
    shared: Arc<Shared>,
}

impl LineLog {
    fn line(&self, line: &str) {
        tracing::info!(target: LOG_TARGET, "[launch {}] {line}", self.id);
        let mut inner = self.shared.lock();
        inner.tail.push_back(line.to_owned());
        while inner.tail.len() > TAIL_LINES {
            inner.tail.pop_front();
        }
        inner.step_tail.push_back(line.to_owned());
        while inner.step_tail.len() > ERROR_TAIL_LINES {
            inner.step_tail.pop_front();
        }
        inner.lines += 1;
        drop(inner);
        self.shared.changed.notify_all();
    }
}

/// A point-in-time copy of a launch, for waits and panels.
#[derive(Debug, Clone)]
pub struct LaunchStatus {
    /// The launch id (the `[launch <id>]` log prefix).
    pub id: u32,
    /// Where it runs.
    pub target: LaunchTarget,
    /// The Android serial.
    pub device: Option<String>,
    /// Where it is.
    pub state: LaunchState,
    /// The step running or last run.
    pub step: String,
    /// Every step started, in order.
    pub steps: Vec<String>,
    /// The game's pid (standalone) or the app's pid on the device (Android).
    pub pid: Option<u32>,
    /// The exit code of a game that exited on its own.
    pub exit_code: Option<i32>,
    /// The built binary or APK.
    pub artifact: Option<PathBuf>,
    /// Why it failed.
    pub error: Option<LaunchError>,
    /// The newest [`TAIL_LINES`] lines.
    pub tail: Vec<String>,
    /// Lines written so far.
    pub lines: u64,
    /// Android: the host URL of the device game's agent server (`ws://127.0.0.1:<forwarded
    /// port>`) once the launch's transport forward is open (ADR-8472, reported per ADR-8594); its
    /// endpoint and token are in the launch's agent directory ([`agent_dir_in`]). `None` for a
    /// standalone launch (the game writes its own endpoint there) and before the forward opens.
    pub agent: Option<String>,
}

impl LaunchStatus {
    /// Whether a kept line contains `needle`.
    #[must_use]
    pub fn tail_contains(&self, needle: &str) -> bool {
        self.tail.iter().any(|line| line.contains(needle))
    }
}

/// One launch: its fixed facts and the shared state.
struct Run {
    id: u32,
    target: LaunchTarget,
    device: Option<String>,
    app_id: Option<AndroidAppId>,
    adb: Option<PathBuf>,
    level_copy: Option<PathBuf>,
    shared: Arc<Shared>,
    /// Standalone: the game's own agent directory (`REGIA_AGENT_DIR`), removed when the launch ends.
    agent_dir: Option<PathBuf>,
    /// Standalone: the asset pusher (`assets.push` into the game, ADR-7836).
    push: Option<PushLink>,
}

impl Run {
    fn status(&self) -> LaunchStatus {
        let inner = self.shared.lock();
        self.snapshot(&inner)
    }

    fn snapshot(&self, inner: &Inner) -> LaunchStatus {
        LaunchStatus {
            id: self.id,
            target: self.target,
            device: self.device.clone(),
            state: inner.state,
            step: inner.step.clone(),
            steps: inner.steps.clone(),
            pid: inner.pid,
            exit_code: inner.exit_code,
            artifact: inner.artifact.clone(),
            error: inner.error.clone(),
            tail: inner.tail.iter().cloned().collect(),
            lines: inner.lines,
            agent: inner
                .forward
                .map(|host_port| format!("ws://127.0.0.1:{host_port}")),
        }
    }
}

// ---------------------------------------------------------------------------
// The worker
// ---------------------------------------------------------------------------

/// The step functions' failure: a launch error, or Stop (nothing more to do).
enum Halt {
    Failed(LaunchError),
    Stopped,
}

type StepResult<T> = Result<T, Halt>;

/// A finished step child.
struct StepOutput {
    status: ExitStatus,
    stdout: Vec<String>,
}

/// A child the worker started: its token, pid and pipe readers.
struct Spawned {
    token: u64,
    pid: u32,
    /// stdout: the collected lines (or only the end of the stream).
    out: Option<Receiver<String>>,
    /// stderr: the end of the stream (its lines are logged).
    err: Option<Receiver<String>>,
}

/// The worker's view of one launch.
struct Worker {
    profile: Option<crate::launch_profiles::LaunchProfile>,
    log: LineLog,
    tools: LaunchTools,
    project: PathBuf,
    level: Option<PathBuf>,
    device: Option<String>,
    app_id: Option<AndroidAppId>,
    adb: Option<PathBuf>,
    /// Standalone: where the game advertises its agent endpoint (`REGIA_AGENT_DIR`). Android: where
    /// the editor writes the device game's forwarded endpoint (ADR-8472).
    agent_dir: Option<PathBuf>,
    /// Android: the device agent port and one-time token the launch hands the game (ADR-8472).
    transport: Option<regia_framework::dev_launch::DevAgent>,
}

impl Worker {
    fn shared(&self) -> &Shared {
        &self.log.shared
    }

    fn note(&self, text: impl AsRef<str>) {
        self.log.line(text.as_ref());
    }

    /// Enters `state` at `step` (unless Stop came first).
    fn enter(&self, state: LaunchState, step: &str) -> StepResult<()> {
        let mut inner = self.shared().lock();
        if inner.stop {
            return Err(Halt::Stopped);
        }
        inner.state = state;
        inner.step = step.to_owned();
        inner.steps.push(step.to_owned());
        inner.step_tail.clear();
        drop(inner);
        self.shared().changed.notify_all();
        Ok(())
    }

    fn fail(&self, code: &'static str, message: impl Into<String>) -> Halt {
        let log_tail = self.shared().lock().step_tail.iter().cloned().collect();
        Halt::Failed(LaunchError {
            code,
            message: message.into(),
            log_tail,
        })
    }

    /// Spawns a child of the launch (refused once Stop came); returns its token and pid.
    fn spawn(&self, role: &'static str, mut cmd: Command, sink: Sink) -> StepResult<Spawned> {
        prepare_child(&mut cmd);
        let mut inner = self.shared().lock();
        if inner.stop {
            return Err(Halt::Stopped);
        }
        // Windows: the child starts suspended and runs only once it is in the job, so nothing it
        // starts escapes Stop (ADR-7548). Elsewhere spawn, then add.
        let spawned = match &inner.group {
            Some(group) => group.spawn(&mut cmd),
            None => cmd
                .spawn()
                .map(|child| regia_platform::child_bind::Spawned { child, outside: None }),
        };
        let mut child = match spawned {
            Ok(regia_platform::child_bind::Spawned { child, outside }) => {
                if let Some(e) = outside {
                    tracing::warn!(target: LOG_TARGET, "launch: {role} runs outside the kill job: {e}");
                }
                child
            }
            Err(e) => {
                drop(inner);
                return Err(Halt::Failed(LaunchError::new(
                    codes::SPAWN_FAILED,
                    format!("cannot run {} ({role}): {e}", cmd.get_program().to_string_lossy()),
                )));
            }
        };
        let pid = child.id();
        let out = child
            .stdout
            .take()
            .and_then(|s| read_lines(s, sink, Some(self.log.clone())));
        // stderr is always logged (cargo's rendered diagnostics, adb's progress).
        let err = child
            .stderr
            .take()
            .and_then(|s| read_lines(s, Sink::Log, Some(self.log.clone())));
        let token = inner.next_token;
        inner.next_token += 1;
        inner.children.push(Tracked { token, role, child });
        Ok(Spawned { token, pid, out, err })
    }

    /// Waits for the child `token` to exit; `None` when Stop took it.
    fn wait(&self, token: u64) -> Option<ExitStatus> {
        let mut inner = self.shared().lock();
        loop {
            let pos = inner.children.iter().position(|t| t.token == token)?;
            match inner.children[pos].child.try_wait() {
                Ok(Some(status)) => {
                    inner.children.remove(pos);
                    return Some(status);
                }
                Ok(None) => {}
                Err(e) => {
                    let mut tracked = inner.children.remove(pos);
                    tracing::warn!(target: LOG_TARGET, "launch: cannot wait for {}: {e}", tracked.role);
                    kill_tree(&mut tracked.child);
                    let _ = tracked.child.wait();
                    return None;
                }
            }
            inner = match self.shared().changed.wait_timeout(inner, POLL) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// Runs a step child to completion.
    fn run(&self, role: &'static str, cmd: Command, sink: Sink) -> StepResult<StepOutput> {
        let child = self.spawn(role, cmd, sink)?;
        let Some(status) = self.wait(child.token) else {
            return Err(Halt::Stopped);
        };
        let stdout = drain(child.out);
        // Every stderr line is in the step's tail before a failure reads it.
        drain(child.err);
        Ok(StepOutput { status, stdout })
    }

    fn cook(&self, platform: &str, incremental: bool) -> StepResult<()> {
        self.enter(LaunchState::Building, "cook")?;
        let report = {
            let _span = tracing::info_span!(target: LOG_TARGET, "launch_cook").entered();
            regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
                project_root: self.project.clone(),
                platform: platform.into(),
                incremental,
            })
        };
        match report {
            Err(e) => return Err(self.fail(codes::BUILD_FAILED, format!("the cook failed: {e}"))),
            Ok(report) if !report.errors.is_empty() => {
                for (path, error) in &report.errors {
                    self.note(format!("cook error in {path}: {error}"));
                }
                return Err(self.fail(
                    codes::BUILD_FAILED,
                    format!("the cook failed with {} errors", report.errors.len()),
                ));
            }
            Ok(report) => self.note(format!(
                "cooked {} assets ({} unchanged) in {} ms",
                report.cooked, report.skipped, report.elapsed_ms
            )),
        }

        Ok(())
    }

    fn run_standalone(&self) -> StepResult<()> {
        if self.profile.as_ref().is_some_and(|p| p.package) {
            return self.run_packaged_standalone();
        }
        if self.profile.as_ref().is_none_or(|p| p.cook != "skip") {
            self.cook(
                regia_asset_pipeline::host_platform(),
                self.profile.as_ref().is_none_or(|p| p.cook != "full"),
            )?;
        }

        self.enter(LaunchState::Building, "cargo build")?;
        let package = launcher_package(&self.project).map_err(|e| self.fail(codes::BUILD_FAILED, e))?;
        let manifest = if self.project.join("Cargo.toml").is_file() {
            self.project.join("Cargo.toml")
        } else {
            self.project.join("launcher").join("Cargo.toml")
        };
        let mut cmd = self.tools.command(self.tools.cargo_program());
        cmd.arg("build")
            .arg("--manifest-path")
            .arg(&manifest)
            .arg("-p")
            .arg(&package)
            .arg("--message-format=json-render-diagnostics")
            .env("CARGO_TERM_COLOR", "never")
            .current_dir(&self.project);
        if let Some(profile) = &self.profile {
            let args = profile_cargo_args(&self.project, &profile.build_profile)
                .map_err(|e| self.fail(codes::PROFILE_INVALID, e))?;
            cmd.args(args);
        } else if self.tools.release {
            cmd.arg("--release");
        }
        let built = self.run("cargo", cmd, Sink::Collect)?;
        if !built.status.success() {
            return Err(self.fail(
                codes::BUILD_FAILED,
                format!("cargo build failed ({})", built.status),
            ));
        }
        let Some(exe) = parse_cargo_executable(&built.stdout.join("\n"), &package) else {
            return Err(self.fail(
                codes::BUILD_FAILED,
                format!("cargo built no executable for {package}"),
            ));
        };
        self.shared().lock().artifact = Some(exe.clone());

        self.run_game(&exe, &self.project)
    }

    fn run_game(&self, exe: &Path, cwd: &Path) -> StepResult<()> {
        self.enter(LaunchState::Starting, "run")?;
        let mut cmd = self.tools.command(exe);
        cmd.current_dir(cwd).env("REGIA_ASSET_WATCH", "0");
        if let Some(profile) = &self.profile {
            cmd.args(&profile.extra_arguments);
        }
        // The game's own agent directory: the editor's remote Outliner and Inspector reach it there
        // (ADR-7833), apart from the editor's endpoint.
        cmd.env("REGIA_AGENT_DIR", agent_dir_in(&self.project, self.log.id));
        // The game advertises its agent endpoint in the launch's own directory, never over the
        // editor's (`<project>/.regia/agent`); the asset pusher connects there (ADR-7836).
        if let Some(dir) = &self.agent_dir {
            cmd.env("REGIA_AGENT_DIR", dir);
        }
        if let Some(level) = &self.level {
            cmd.env("REGIA_START_LEVEL", level);
        } else {
            cmd.env_remove("REGIA_START_LEVEL");
        }
        let Spawned { token, pid, .. } = match self.spawn("game", cmd, Sink::Log) {
            Err(Halt::Failed(e)) => {
                return Err(Halt::Failed(LaunchError {
                    code: codes::START_FAILED,
                    ..e
                }));
            }
            other => other?,
        };
        self.note(format!("{} started, pid {pid}", exe.display()));
        {
            let mut inner = self.shared().lock();
            inner.pid = Some(pid);
        }
        self.enter_running("game")?;
        self.finish(token, "the game")
    }

    fn enter_running(&self, step: &str) -> StepResult<()> {
        let mut inner = self.shared().lock();
        if inner.stop {
            return Err(Halt::Stopped);
        }
        inner.state = LaunchState::Running;
        inner.step = step.to_owned();
        drop(inner);
        self.shared().changed.notify_all();
        Ok(())
    }

    /// Waits for the long-running child and records how it ended.
    fn finish(&self, token: u64, what: &str) -> StepResult<()> {
        let Some(status) = self.wait(token) else {
            return Err(Halt::Stopped);
        };
        self.note(format!("{what} exited with {status}"));
        let mut inner = self.shared().lock();
        if !inner.stop {
            inner.state = LaunchState::Exited;
            inner.exit_code = status.code();
        }
        drop(inner);
        self.shared().changed.notify_all();
        Ok(())
    }

    fn run_packaged_standalone(&self) -> StepResult<()> {
        let profile = self
            .profile
            .as_ref()
            .ok_or_else(|| self.fail(codes::PROFILE_INVALID, "No profile"))?;
        if profile.cook == "full" {
            self.cook(regia_asset_pipeline::host_platform(), false)?;
        }
        self.enter(LaunchState::Building, "package")?;
        let mut cmd = self.tools.command(self.tools.cli_program());
        cmd.args([
            "package",
            "--platform",
            regia_asset_pipeline::host_platform(),
            "--profile",
            &profile.build_profile,
            "--format",
            "dir",
            "--json",
        ])
        .arg("--project")
        .arg(&self.project)
        .current_dir(&self.project);
        let packaged = self.run("package", cmd, Sink::Collect)?;
        let artifact = parse_package_artifact(&packaged.stdout.join("\n"))
            .filter(|_| packaged.status.success())
            .ok_or_else(|| self.fail(codes::BUILD_FAILED, "Desktop packaging failed"))?;
        let binary =
            packaged_binary(&self.project, &artifact).map_err(|e| self.fail(codes::BUILD_FAILED, e))?;
        self.shared().lock().artifact = Some(artifact.clone());
        self.run_game(&binary, &artifact)
    }

    fn run_android(&self) -> StepResult<()> {
        let (Some(adb), Some(device), Some(app_id)) = (&self.adb, &self.device, &self.app_id) else {
            return Err(Halt::Failed(LaunchError::new(
                codes::NO_DEVICE,
                "no device was chosen",
            )));
        };
        if self.profile.as_ref().is_some_and(|p| p.cook == "full") {
            self.cook("android", false)?;
        }
        self.enter(LaunchState::Building, "package")?;
        // Build only the device's ABI when the manifest lists it (as `regia install-android` does).
        let abi = run_short(
            {
                let mut cmd = self.tools.adb(adb, device);
                cmd.args(["shell", "getprop", "ro.product.cpu.abi"]);
                cmd
            },
            SHORT_CALL,
        )
        .ok()
        .map(|(_, out, _)| out.trim().to_owned())
        .filter(|abi| manifest_abis(&self.project).iter().any(|listed| listed == abi));
        let mut cmd = self.tools.command(self.tools.cli_program());
        cmd.args([
            "package",
            "--platform",
            "android",
            "--profile",
            self.profile.as_ref().map_or("dev", |p| p.build_profile.as_str()),
            "--format",
            "apk",
            "--json",
        ])
        .arg("--project")
        .arg(&self.project)
        .current_dir(&self.project);
        if let Some(abi) = &abi {
            cmd.args(["--abi", abi]);
        }
        let packaged = self.run("package", cmd, Sink::Collect)?;
        let artifact = parse_package_artifact(&packaged.stdout.join("\n"));
        let apk = match artifact {
            Some(apk) if packaged.status.success() => apk,
            _ => {
                for line in &packaged.stdout {
                    self.note(line);
                }
                return Err(self.fail(
                    codes::BUILD_FAILED,
                    format!("regia package --platform android failed ({})", packaged.status),
                ));
            }
        };
        self.shared().lock().artifact = Some(apk.clone());

        self.enter(LaunchState::Installing, "install")?;
        let mut cmd = self.tools.adb(adb, device);
        cmd.arg("install").arg("-r").arg(&apk);
        let installed = self.run("adb install", cmd, Sink::LogAndCollect)?;
        if !installed.status.success() || installed.stdout.iter().any(|l| l.contains("Failure")) {
            return Err(self.fail(
                codes::INSTALL_FAILED,
                format!("adb install failed ({})", installed.status),
            ));
        }

        self.enter(LaunchState::Starting, "start")?;
        let component = format!("{app_id}/.{ANDROID_ACTIVITY}");
        // The profile's start level and arguments and the agent transport ride on one intent
        // extra (ADR-8472); a launch without any sends the plain `am start`.
        let options = regia_framework::dev_launch::DevLaunch {
            start_level: self.profile.as_ref().and_then(|p| p.start_level.clone()),
            args: self
                .profile
                .as_ref()
                .map(|p| p.extra_arguments.clone())
                .unwrap_or_default(),
            agent: self.transport.clone(),
        };
        let am_start = crate::device_transport::am_start_args(&component, &options)
            .map_err(|e| self.fail(codes::PROFILE_INVALID, format!("launch options: {e}")))?;
        self.shared().lock().app_started = true;
        let mut cmd = self.tools.adb(adb, device);
        cmd.args(&am_start);
        let started = self.run("am start", cmd, Sink::LogAndCollect)?;
        if !started.status.success() || started.stdout.iter().any(|l| l.contains("Error:")) {
            return Err(self.fail(codes::START_FAILED, format!("am start -n {component} failed")));
        }
        let deadline = Instant::now() + PID_TIMEOUT;
        let pid = loop {
            let mut cmd = self.tools.adb(adb, device);
            cmd.args(["shell", "pidof", app_id.as_str()]);
            let out = self.run("pidof", cmd, Sink::Collect)?;
            if let Some(pid) = parse_pidof(&out.stdout.join(" ")) {
                break pid;
            }
            if Instant::now() >= deadline {
                return Err(self.fail(codes::START_FAILED, format!("{app_id} did not start on {device}")));
            }
            let inner = self.shared().lock();
            if inner.stop {
                return Err(Halt::Stopped);
            }
            drop(self.shared().changed.wait_timeout(inner, POLL * 8));
        };
        self.shared().lock().pid = Some(pid);
        self.note(format!("{app_id} running on {device} as pid {pid}"));
        self.open_transport(adb, device, app_id, pid)?;

        let mut cmd = self.tools.adb(adb, device);
        cmd.args(["logcat", "--pid", &pid.to_string()]);
        let Spawned { token, .. } = self.spawn("logcat", cmd, Sink::Log)?;
        self.enter_running("logcat")?;
        let finished = self.finish(token, "logcat");
        // The app ended on its own: its forward goes with it. After Stop (`Err`), Stop removes it
        // before it returns, so the worker leaves it alone.
        if finished.is_ok() {
            let forward = self.shared().lock().forward.take();
            if let Some(host_port) = forward {
                remove_forward(&self.tools, adb, device, host_port);
            }
        }
        finished
    }

    /// ADR-8472: forwards a host loopback port to the device game's agent port and writes the host
    /// endpoint into the launch's agent directory, so the asset pusher and the remote panels reach
    /// the device. A failure is logged and the launch goes on without the transport.
    fn open_transport(&self, adb: &Path, device: &str, app_id: &AndroidAppId, pid: u32) -> StepResult<()> {
        let (Some(transport), Some(dir)) = (&self.transport, &self.agent_dir) else {
            return Ok(());
        };
        let mut cmd = self.tools.adb(adb, device);
        cmd.args(crate::device_transport::forward_args(transport.port));
        let forwarded = self.run("adb forward", cmd, Sink::Collect)?;
        let host_port = crate::device_transport::parse_forward_port(&forwarded.stdout.join("\n"))
            .filter(|_| forwarded.status.success());
        let Some(host_port) = host_port else {
            self.note(format!(
                "agent transport not opened: adb forward tcp:0 tcp:{} failed ({}); no asset push or remote panels for this launch",
                transport.port, forwarded.status
            ));
            return Ok(());
        };
        self.shared().lock().forward = Some(host_port);
        match crate::device_transport::write_host_endpoint(
            dir,
            host_port,
            pid,
            app_id.as_str(),
            &transport.token,
        ) {
            Ok(()) => self.note(format!(
                "agent transport: 127.0.0.1:{host_port} -> {device} port {} (asset push and remote panels)",
                transport.port
            )),
            Err(e) => self.note(format!(
                "agent transport not advertised in {}: {e}",
                dir.display()
            )),
        }
        Ok(())
    }

    fn run_target(&self, target: LaunchTarget) {
        let _span = tracing::info_span!(target: LOG_TARGET, "launch", kind = target.as_str()).entered();
        let result = match target {
            LaunchTarget::Standalone => self.run_standalone(),
            LaunchTarget::Android => self.run_android(),
        };
        if let Err(Halt::Failed(error)) = result {
            self.note(format!("{}: {}", error.code, error.message));
            let mut inner = self.shared().lock();
            if !inner.stop {
                inner.state = LaunchState::Failed;
                inner.error = Some(error);
            }
            drop(inner);
            self.shared().changed.notify_all();
        }
    }
}

// Refuse combinations the launcher cannot carry out; never silently drop settings.
fn validate_profile_launch(profile: &crate::launch_profiles::LaunchProfile) -> Result<(), LaunchError> {
    profile
        .validate()
        .map_err(|e| LaunchError::new(codes::PROFILE_INVALID, e))?;
    // The start level and arguments reach the device in the launch intent's extra (ADR-8472). A
    // package-off launch would start whatever APK the device has, which may not hold the project's
    // current content: refused until the fast-iterate dev APK (P-300) defines what it reuses.
    if profile.target == "android" && !profile.package {
        return Err(LaunchError::new(
            codes::PROFILE_UNSUPPORTED,
            "Android launches always package and install the APK: set package to true (a package-off launch needs the fast-iterate dev APK, not built yet; ADR-8472)",
        ));
    }
    Ok(())
}

// Mirrors the existing project build-profile interpretation, using only the project's manifest.
// No command is assembled as shell text; names and feature tokens remain separate argv values.
/// The shipping profile `regia package` builds with when the project's `Cargo.toml` has no
/// `[profile.shipping]` (ADR-5627; the same values as `regia_cli`'s package command, which the
/// editor does not link).
const SHIPPING_PROFILE: [(&str, &str); 6] = [
    ("inherits", "\"release\""),
    ("lto", "\"fat\""),
    ("codegen-units", "1"),
    ("panic", "\"abort\""),
    ("debug", "\"line-tables-only\""),
    ("strip", "\"none\""),
];

/// `--config` arguments that define the shipping profile when `project`'s `Cargo.toml` has none,
/// so a shipping launch builds where `regia package --profile shipping` does (review
/// sprint-w12-rev-g15 #5). A manifest with the table (or one that cannot be read: cargo reports
/// it) gets none.
fn shipping_config_args(project: &Path) -> Vec<String> {
    let defines = std::fs::read_to_string(project.join("Cargo.toml"))
        .ok()
        .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
        .is_none_or(|table| {
            table
                .get("profile")
                .and_then(|p| p.get("shipping"))
                .is_some_and(toml::Value::is_table)
        });
    if defines {
        return Vec::new();
    }
    SHIPPING_PROFILE
        .iter()
        .flat_map(|(key, value)| ["--config".to_owned(), format!("profile.shipping.{key}={value}")])
        .collect()
}

fn profile_cargo_args(project: &Path, name: &str) -> Result<Vec<String>, String> {
    let mut args = vec!["--profile".into()];
    if matches!(name, "dev" | "shipping") {
        args.push(name.into());
        if name == "shipping" {
            args.extend(shipping_config_args(project));
        }
        return Ok(args);
    }
    let manifest = regia_toml(project).ok_or("Cannot read Regia.toml build profiles")?;
    let profile = manifest
        .get("profiles")
        .and_then(|p| p.get(name))
        .ok_or_else(|| format!("Unknown build profile {name:?}"))?;
    if profile.get("target").and_then(toml::Value::as_str) == Some("server") {
        return Err("A launch profile needs a client build profile, not a server profile".into());
    }
    let base = profile
        .get("base")
        .and_then(toml::Value::as_str)
        .unwrap_or("shipping");
    if !matches!(base, "dev" | "shipping") {
        return Err(format!("Invalid build profile base {base:?}"));
    }
    args.push(base.into());
    if base == "shipping" {
        args.extend(shipping_config_args(project));
    }
    if profile.get("no_default_features").and_then(toml::Value::as_bool) == Some(true) {
        args.push("--no-default-features".into());
    }
    if let Some(features) = profile.get("features") {
        let features = features.as_array().ok_or("features must be an array")?;
        let names: Result<Vec<_>, _> = features
            .iter()
            .map(|f| f.as_str().ok_or("features must contain strings"))
            .collect();
        let names = names?;
        if !names.is_empty() {
            args.extend(["--features".into(), names.join(",")]);
        }
    }
    Ok(args)
}

fn packaged_binary(project: &Path, directory: &Path) -> Result<PathBuf, String> {
    for manifest in [project.join("Cargo.toml"), project.join("launcher/Cargo.toml")] {
        let Ok(text) = std::fs::read_to_string(manifest) else {
            continue;
        };
        let Ok(doc) = toml::from_str::<toml::Value>(&text) else {
            continue;
        };
        let Some(package) = doc
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(toml::Value::as_str)
        else {
            continue;
        };
        let binary = doc
            .get("bin")
            .and_then(toml::Value::as_array)
            .and_then(|b| b.first())
            .and_then(|b| b.get("name"))
            .and_then(toml::Value::as_str)
            .unwrap_or(package);
        if binary.contains(['/', '\\']) || binary == ".." {
            return Err("Invalid package binary name".into());
        }
        let path = directory.join(format!("{binary}{}", std::env::consts::EXE_SUFFIX));
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!("Packaged binary does not exist: {}", path.display()));
    }
    Err("No package or launcher manifest names the packaged binary".into())
}

/// The launcher package's name from `launcher/Cargo.toml` (what `regia run` builds).
fn launcher_package(project: &Path) -> Result<String, String> {
    let manifest = project.join("launcher").join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| format!("{}: {e} (is this a Regia Engine project?)", manifest.display()))?;
    let value: toml::Value = toml::from_str(&text).map_err(|e| format!("{}: {e}", manifest.display()))?;
    value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("{} has no [package] name", manifest.display()))
}

fn regia_toml(project: &Path) -> Option<toml::Value> {
    let text = std::fs::read_to_string(project.join("Regia.toml")).ok()?;
    toml::from_str(&text).ok()
}

/// `[project] id` of `Regia.toml` (the Android application id).
#[must_use]
pub fn app_id(project: &Path) -> Option<String> {
    regia_toml(project)?
        .get("project")?
        .get("id")?
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// An Android application id that passed the package-name grammar
/// (`project_settings::validate_app_id`, the packager's rule): only ASCII letters, digits, `_` and
/// `.`, so it is one plain word on the device's `sh` command line, which `adb shell` builds by
/// joining its arguments (ADR-7548). Only `AndroidAppId::parse` makes one, and every adb call
/// passes it as its own argument.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AndroidAppId(String);

impl AndroidAppId {
    fn parse(id: &str) -> Result<Self, LaunchError> {
        crate::project_settings::validate_app_id(id).map_err(|e| LaunchError::new(codes::BAD_APP_ID, e))?;
        // The grammar admits nothing else; this keeps the shell guarantee should it ever widen.
        if !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
        {
            return Err(LaunchError::new(
                codes::BAD_APP_ID,
                format!("[project] id {id:?} has characters other than ASCII letters, digits, `_` and `.`"),
            ));
        }
        Ok(Self(id.to_owned()))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AndroidAppId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `[platforms.android] abis` of `Regia.toml`.
fn manifest_abis(project: &Path) -> Vec<String> {
    regia_toml(project)
        .and_then(|v| {
            v.get("platforms")?
                .get("android")?
                .get("abis")?
                .as_array()
                .map(|abis| {
                    abis.iter()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The editor's launcher
// ---------------------------------------------------------------------------

/// What to launch.
#[derive(Debug, Clone)]
pub struct LaunchRequest {
    /// Where.
    pub target: LaunchTarget,
    /// The Android serial (`None`: the only attached device).
    pub device: Option<String>,
    /// The project directory.
    pub project: PathBuf,
    /// The start level (standalone), absolute.
    pub level: Option<PathBuf>,
}

/// The editor's launches: at most one runs at a time. A world resource shared by the launch bar
/// and the agent methods.
#[derive(Resource, Default)]
pub struct Launcher {
    /// The programs launches run.
    pub tools: LaunchTools,
    next_id: u32,
    current: Option<Run>,
    /// The launch bar's choices and device list.
    ui: LaunchBar,
    /// How the asset pusher reaches a launched game (default: the agent endpoint the game wrote in
    /// its launch agent directory). Tests replace it.
    pub push_connector: Option<PushConnector>,
    /// How an Android launch's pusher cooks for the device before a push (default: an incremental
    /// `android` cook of the project). Tests replace it.
    pub push_cooker: Option<PushCooker>,
}

/// The launch bar's state.
#[derive(Debug, Default)]
struct LaunchBar {
    target: LaunchTarget,
    device: Option<String>,
    devices: Vec<AdbDevice>,
    note: String,
    /// The egui frame whose click was handled (a frame may run the UI twice).
    handled_frame: Option<u64>,
}

impl Drop for Launcher {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Launcher {
    /// Lists the attached Android devices (`adb devices -l`).
    ///
    /// # Errors
    ///
    /// [`codes::NO_ADB`] when adb is not found or does not answer.
    pub fn list_devices(&self) -> Result<(PathBuf, Vec<AdbDevice>), LaunchError> {
        let Some(adb) = self.tools.adb_program() else {
            return Err(LaunchError::new(
                codes::NO_ADB,
                "adb not found (no SDK platform-tools, not on PATH)",
            ));
        };
        let mut cmd = self.tools.command(&adb);
        cmd.args(["devices", "-l"]);
        match run_short(cmd, SHORT_CALL) {
            Ok((Some(status), out, _)) if status.success() => Ok((adb, parse_adb_devices(&out))),
            Ok((status, _, err)) => Err(LaunchError {
                code: codes::NO_ADB,
                message: format!(
                    "`{} devices -l` failed ({})",
                    adb.display(),
                    status.map_or_else(|| "timed out".to_owned(), |s| s.to_string())
                ),
                log_tail: err.lines().map(str::to_owned).collect(),
            }),
            Err(e) => Err(LaunchError::new(
                codes::NO_ADB,
                format!("cannot run {}: {e}", adb.display()),
            )),
        }
    }

    /// Whether a launch is building or running.
    #[must_use]
    pub fn active(&self) -> bool {
        self.current
            .as_ref()
            .is_some_and(|run| run.status().state.is_active())
    }

    /// The current (or last) launch.
    #[must_use]
    pub fn status(&self) -> Option<LaunchStatus> {
        self.current.as_ref().map(Run::status)
    }

    /// Starts a launch: refusals (busy, no adb, no device, no app id) answer at once; the steps
    /// run on a worker thread. Returns the launch id.
    ///
    /// # Errors
    ///
    /// See [`codes`].
    pub fn launch(&mut self, request: LaunchRequest) -> Result<u32, LaunchError> {
        self.launch_with_profile(request, None)
    }

    fn launch_with_profile(
        &mut self,
        request: LaunchRequest,
        profile: Option<crate::launch_profiles::LaunchProfile>,
    ) -> Result<u32, LaunchError> {
        if let Some(profile) = &profile {
            validate_profile_launch(profile)?;
        }
        if self.active() {
            return Err(LaunchError::new(
                codes::BUSY,
                "a launch is already building or running",
            ));
        }
        self.clear_finished();
        let (adb, device, app_id) = match request.target {
            LaunchTarget::Standalone => (None, None, None),
            LaunchTarget::Android => {
                let Some(app_id) = app_id(&request.project) else {
                    return Err(LaunchError::new(
                        codes::NO_APP_ID,
                        "Regia.toml has no [project] id",
                    ));
                };
                // Checked before any adb call: the id reaches the device's shell (ADR-7548).
                let app_id = AndroidAppId::parse(&app_id)?;
                let (adb, devices) = self.list_devices()?;
                let device = choose_device(&devices, request.device.as_deref())?;
                (Some(adb), Some(device), Some(app_id))
            }
        };
        self.next_id += 1;
        let id = self.next_id;
        // Standalone: the game's own agent directory. Android: where the editor writes the device
        // game's forwarded endpoint (ADR-8472).
        let agent_dir = Some(agent_dir_in(&request.project, id));
        let transport =
            (request.target == LaunchTarget::Android).then(|| regia_framework::dev_launch::DevAgent {
                port: crate::device_transport::device_port(id),
                token: crate::device_transport::new_launch_token(),
            });
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                state: LaunchState::Building,
                step: String::new(),
                steps: Vec::new(),
                error: None,
                tail: VecDeque::new(),
                step_tail: VecDeque::new(),
                lines: 0,
                pid: None,
                exit_code: None,
                artifact: None,
                children: Vec::new(),
                next_token: 0,
                stop: false,
                app_started: false,
                group: Some(regia_platform::child_bind::ChildGroup::new()),
                forward: None,
            }),
            changed: Condvar::new(),
        });
        let keeps_level = profile.as_ref().is_some_and(|p| p.start_level.is_some());
        let worker = Worker {
            profile,
            log: LineLog {
                id,
                shared: Arc::clone(&shared),
            },
            tools: self.tools.clone(),
            project: request.project.clone(),
            level: request.level.clone(),
            device: device.clone(),
            app_id: app_id.clone(),
            adb: adb.clone(),
            agent_dir: agent_dir.clone(),
            transport,
        };
        worker.note(format!(
            "launch {id}: {}{} of {}",
            request.target.as_str(),
            device.as_deref().map(|d| format!(" on {d}")).unwrap_or_default(),
            request.project.display()
        ));
        let target = request.target;
        // The worker lives as long as the launch's children: on Linux their parent-death signal
        // follows the thread that spawned them (`regia_platform::child_bind`).
        let spawned = std::thread::Builder::new()
            .name(format!("launch-{id}"))
            .spawn(move || worker.run_target(target));
        if let Err(e) = spawned {
            return Err(LaunchError::new(
                codes::SPAWN_FAILED,
                format!("cannot start the launch thread: {e}"),
            ));
        }
        self.current = Some(Run {
            id,
            target,
            device,
            app_id,
            adb,
            level_copy: if keeps_level { None } else { request.level },
            shared,
            push: agent_dir.as_ref().map(|dir| {
                PushLink::spawn(
                    id,
                    dir.clone(),
                    request.project.clone(),
                    self.push_connector.clone(),
                    // A device game reads Android-cooked assets: they are cooked for it before the
                    // push (ADR-8472).
                    (target == LaunchTarget::Android)
                        .then(|| self.push_cooker.clone().unwrap_or_else(default_push_cooker)),
                )
            }),
            agent_dir,
        });
        Ok(id)
    }

    /// Removes the play copy of a finished launch.
    fn clear_finished(&mut self) {
        if let Some(run) = &mut self.current
            && !run.status().state.is_active()
        {
            if let Some(copy) = run.level_copy.take() {
                let _ = std::fs::remove_file(copy);
            }
            if let Some(dir) = run.agent_dir.take() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    /// Stops the current launch: an Android app is force-stopped, every child is killed with its
    /// process tree and reaped before this returns. `false` when nothing was building or running.
    pub fn stop(&mut self) -> bool {
        let Some(run) = &mut self.current else {
            return false;
        };
        let (children, app_started) = {
            let mut inner = run.shared.lock();
            if !inner.state.is_active() {
                drop(inner);
                self.clear_finished();
                return false;
            }
            inner.stop = true;
            (std::mem::take(&mut inner.children), inner.app_started)
        };
        // Only an app this launch started is force-stopped: during the build or the install an app
        // with the id on the device is an earlier install's (ADR-7548).
        if let (LaunchTarget::Android, true, Some(adb), Some(device), Some(app_id)) =
            (run.target, app_started, &run.adb, &run.device, &run.app_id)
        {
            let mut cmd = self.tools.adb(adb, device);
            cmd.args(["shell", "am", "force-stop", app_id.as_str()]);
            if let Err(e) = run_short(cmd, SHORT_CALL) {
                tracing::warn!(target: LOG_TARGET, "launch: force-stop of {app_id}: {e}");
            }
        }
        // ADR-8472: the device game's forward goes with the launch.
        let forward = run.shared.lock().forward.take();
        if let (Some(host_port), Some(adb), Some(device)) = (forward, &run.adb, &run.device) {
            remove_forward(&self.tools, adb, device, host_port);
        }
        // Windows: closing the job ends every child's tree (KILL_ON_JOB_CLOSE).
        run.shared.lock().group = None;
        let log = LineLog {
            id: run.id,
            shared: Arc::clone(&run.shared),
        };
        for mut tracked in children {
            kill_tree(&mut tracked.child);
            let status = tracked.child.wait();
            log.line(&format!(
                "{} stopped by the editor{}",
                tracked.role,
                status.map(|s| format!(" ({s})")).unwrap_or_default()
            ));
        }
        {
            let mut inner = run.shared.lock();
            inner.state = LaunchState::Stopped;
            inner.step = "stopped".to_owned();
        }
        run.shared.changed.notify_all();
        if let Some(copy) = run.level_copy.take() {
            let _ = std::fs::remove_file(copy);
        }
        if let Some(dir) = run.agent_dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
        true
    }

    /// Blocks until `ready` holds for the current launch (re-checked on every line and state
    /// change) or `timeout` passes; returns whether it held. `false` without a launch.
    pub fn wait_for(&self, ready: impl Fn(&LaunchStatus) -> bool, timeout: Duration) -> bool {
        let Some(run) = &self.current else {
            return false;
        };
        let deadline = Instant::now() + timeout;
        let mut inner = run.shared.lock();
        loop {
            if ready(&run.snapshot(&inner)) {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            inner = match run.shared.changed.wait_timeout(inner, left) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// The `editor.launch_status` view with the newest `tail` lines.
    #[must_use]
    pub fn status_json(&self, tail: usize) -> Value {
        let Some(status) = self.status() else {
            return json!({
                "id": null, "target": null, "device": null, "state": "idle", "running": false,
                "step": null, "steps": [], "pid": null, "exit_code": null, "artifact": null, "agent": null,
                "error": null, "lines": 0, "log_tail": [], "pushed": 0, "push_failed": 0, "push_error": null,
                "transport": null,
            });
        };
        let push = self.push_counts();
        let forward = self.current.as_ref().and_then(|run| run.shared.lock().forward);
        let skip = status.tail.len().saturating_sub(tail);
        json!({
            "id": status.id,
            "target": status.target.as_str(),
            "device": status.device,
            "state": status.state.as_str(),
            "running": status.state.is_active(),
            "step": status.step,
            "steps": status.steps,
            "pid": status.pid,
            "exit_code": status.exit_code,
            "artifact": status.artifact.map(|p| p.display().to_string()),
            "agent": status.agent,
            "error": status.error.as_ref().map(LaunchError::to_json),
            "lines": status.lines,
            "log_tail": status.tail[skip..],
            "pushed": push.pushed,
            "push_failed": push.failed,
            "push_error": push.last_error,
            "transport": forward.map(|host_port| json!({ "host_port": host_port })),
        })
    }
}

/// Starts a launch of the editor's open project. A standalone launch starts in a play copy of the
/// open level (unsaved edits included), when one is open.
///
/// # Errors
///
/// See [`codes`].
pub fn launch(world: &mut World, target: LaunchTarget, device: Option<String>) -> Result<u32, LaunchError> {
    launch_selected(world, target, device, None)
}

fn launch_selected(
    world: &mut World,
    target: LaunchTarget,
    device: Option<String>,
    profile: Option<crate::launch_profiles::LaunchProfile>,
) -> Result<u32, LaunchError> {
    if let Some(profile) = &profile {
        validate_profile_launch(profile)?;
    }
    let Some(project) = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
    else {
        return Err(LaunchError::new(codes::NO_PROJECT, "no project is open"));
    };
    if world.get_resource::<Launcher>().is_some_and(Launcher::active) {
        return Err(LaunchError::new(
            codes::BUSY,
            "a launch is already building or running",
        ));
    }
    // Extension before-launch hooks (ADR-7791): an abort refuses the launch with its message.
    let hooks = crate::extension::hooks::run_stage(
        world,
        &crate::extension::hooks::HookInput {
            stage: crate::extension::hooks::HookStage::BeforeLaunch,
            project: project.clone(),
            target: target.as_str().to_owned(),
            profile: String::new(),
            out: None,
        },
    );
    if let Some(message) = hooks.aborted {
        return Err(LaunchError {
            code: codes::HOOK_ABORTED,
            message,
            log_tail: hooks.log,
        });
    }
    let explicit_level = profile.as_ref().and_then(|p| p.start_level.as_ref());
    let level = if let Some(level) = explicit_level {
        let path = crate::agent_ext::assets_request_path(&project, "start_level", level)
            .map_err(|e| LaunchError::new(codes::PROFILE_INVALID, e.message))?;
        if !path.exists() {
            return Err(LaunchError::new(
                codes::PROFILE_INVALID,
                format!("Start level does not exist: {}", path.display()),
            ));
        }
        Some(path)
    } else if target == LaunchTarget::Standalone && world.contains_resource::<crate::level_io::OpenLevel>() {
        match crate::pie_process::write_play_copy(world) {
            Ok((root, rel)) => Some(root.join("assets").join(rel)),
            Err(e) => {
                tracing::warn!(target: LOG_TARGET, "launch: no start level ({}: {})", e.code, e.message);
                None
            }
        }
    } else {
        None
    };
    let remove_on_error = explicit_level.is_none();
    let request = LaunchRequest {
        target,
        device,
        project,
        level: level.clone(),
    };
    let result = world
        .get_resource_or_insert_with(Launcher::default)
        .launch_with_profile(request, profile);
    if result.is_err()
        && remove_on_error
        && let Some(copy) = level
    {
        let _ = std::fs::remove_file(copy);
    }
    result
}

/// Stops the editor's launch; `false` when none was building or running.
pub fn stop(world: &mut World) -> bool {
    world.get_resource_mut::<Launcher>().is_some_and(|mut l| l.stop())
}

// ---------------------------------------------------------------------------
// The launch bar (top of the Output Log)
// ---------------------------------------------------------------------------

/// Draws the launch bar: target and device pickers, Launch, Stop and the running indicator. The
/// Output Log under it shows the launch's lines (`[launch <id>]`).
pub fn launch_bar(ui: &mut egui::Ui, world: &mut World) {
    let _span = tracing::trace_span!(target: LOG_TARGET, "launch_bar").entered();
    let frame = ui.ctx().cumulative_frame_nr();
    crate::platforms_panel::tick(world);
    ui.horizontal(|ui| crate::launch_profiles::launch_bar(ui, world));
    let profile_name = crate::launch_profiles::selected(world);
    let profile = crate::launch_profiles::display_selection(world);
    let mut launcher = world.remove_resource::<Launcher>().unwrap_or_default();
    if let Some(profile) = &profile {
        launcher.ui.target = LaunchTarget::parse(&profile.target).unwrap_or_default();
        launcher.ui.device = profile.device.clone();
    }
    let status = launcher.status();
    let active = status.as_ref().is_some_and(|s| s.state.is_active());
    let mut clicked_launch = false;
    let mut clicked_stop = false;
    let mut clicked_refresh = false;
    ui.horizontal(|ui| {
        ui.label("Launch:");
        let bar = &mut launcher.ui;
        ui.add_enabled_ui(profile_name.is_none(), |ui| {
            egui::ComboBox::from_id_salt("launch-target")
                .selected_text(match bar.target {
                    LaunchTarget::Standalone => "Standalone",
                    LaunchTarget::Android => "Android device",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut bar.target, LaunchTarget::Standalone, "Standalone");
                    ui.selectable_value(&mut bar.target, LaunchTarget::Android, "Android device");
                })
                .response
                .on_hover_text("Where Launch runs the game");
        });
        if crate::platforms_panel::target_failed(world, bar.target) {
            ui.colored_label(ui.visuals().warn_fg_color, "⚠ Platform check failed")
                .on_hover_text("Open Window → Platforms for fixes");
        }
        if bar.target == LaunchTarget::Android {
            let shown = bar.device.clone().unwrap_or_else(|| "(only device)".to_owned());
            ui.add_enabled_ui(profile_name.is_none(), |ui| {
                egui::ComboBox::from_id_salt("launch-device")
                    .selected_text(shown)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut bar.device, None, "(only device)");
                        for device in &bar.devices {
                            let label = format!(
                                "{} {}{}",
                                device.serial,
                                device.model.as_deref().unwrap_or(""),
                                if device.usable() {
                                    String::new()
                                } else {
                                    format!(" ({})", device.state)
                                }
                            );
                            ui.selectable_value(&mut bar.device, Some(device.serial.clone()), label);
                        }
                    })
                    .response
                    .on_hover_text("The adb serial to install to");
            });
            clicked_refresh =
                crate::agent_parity::action_button(ui, &crate::agent_parity::LAUNCH_REFRESH_DEVICES)
                    .clicked();
        }
        let action = match bar.target {
            LaunchTarget::Standalone => &crate::agent_parity::LAUNCH_STANDALONE,
            LaunchTarget::Android => &crate::agent_parity::LAUNCH_ANDROID,
        };
        clicked_launch = crate::agent_parity::action_enabled(ui, !active, action, None).clicked();
        clicked_stop =
            crate::agent_parity::action_enabled(ui, active, &crate::agent_parity::LAUNCH_STOP, None)
                .clicked();
        if let Some(status) = &status {
            let text = format!(
                "launch {}: {} {}{}",
                status.id,
                status.target.as_str(),
                status.state.as_str(),
                status.pid.map(|pid| format!(", pid {pid}")).unwrap_or_default()
            );
            if active {
                ui.spinner();
                ui.label(text);
            } else if let Some(error) = &status.error {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("{text}: {}", error.code))
                    .on_hover_text(&error.message);
            } else {
                ui.weak(text);
            }
        }
        let push = launcher.push_counts();
        if push.pushed > 0 || push.failed > 0 {
            // ADR-7836: assets the live cooker rebuilt and the pusher sent into the game.
            let text = if push.failed > 0 {
                format!("pushed {}, {} failed", push.pushed, push.failed)
            } else {
                format!("pushed {}", push.pushed)
            };
            let shown = ui.weak(text);
            if let Some(error) = &push.last_error {
                shown.on_hover_text(error);
            }
        }
        if !launcher.ui.note.is_empty() {
            ui.weak(&launcher.ui.note);
        }
    });
    let fresh = launcher.ui.handled_frame != Some(frame);
    if fresh && (clicked_launch || clicked_stop || clicked_refresh) {
        launcher.ui.handled_frame = Some(frame);
    }
    if fresh && clicked_refresh {
        launcher.ui.note = match launcher.list_devices() {
            Ok((_, devices)) => {
                let note = format!("{} device(s)", devices.len());
                launcher.ui.devices = devices;
                note
            }
            Err(e) => format!("{}: {}", e.code, e.message),
        };
    }
    if fresh && clicked_stop {
        launcher.stop();
    }
    let request = (fresh && clicked_launch).then(|| (launcher.ui.target, launcher.ui.device.clone()));
    let android = launcher.ui.target == LaunchTarget::Android;
    world.insert_resource(launcher);
    // The device screen with tap forwarding, under the bar while Android is chosen (ADR-7836).
    if android {
        crate::device_mirror::mirror_ui(ui, world);
    }
    if let Some((target, device)) = request {
        let launched = match profile_name
            .as_deref()
            .map(|name| crate::launch_profiles::resolve(world, name))
        {
            Some(Ok(profile)) => launch_selected(
                world,
                LaunchTarget::parse(&profile.target).unwrap_or_default(),
                profile.device.clone(),
                Some(profile),
            ),
            Some(Err(e)) => Err(LaunchError::new(codes::PROFILE_INVALID, e.message)),
            None => launch(world, target, device),
        };
        let note = match launched {
            Ok(id) => format!("launch {id} started"),
            Err(e) => format!("{}: {}", e.code, e.message),
        };
        world.resource_mut::<Launcher>().ui.note = note;
    }
}

// ---------------------------------------------------------------------------
// Agent methods
// ---------------------------------------------------------------------------

fn tail_param(params: &Value, default: usize, max: usize) -> Result<usize, RpcError> {
    match params.get("tail") {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .map(|n| usize::try_from(n).unwrap_or(max).min(max))
            .ok_or_else(|| RpcError::wrong_type("tail", "a whole number")),
    }
}

/// `editor.launch { target, device?, wait?, timeout_ms? }`. The answer is the status plus `pending`:
/// `true` while the launch has neither run nor ended (follow it with `editor.launch_status`).
fn editor_launch(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let target = match params.get("target") {
        None | Some(Value::Null) => LaunchTarget::Standalone,
        Some(Value::String(name)) => LaunchTarget::parse(name)
            .ok_or_else(|| LaunchError::new(codes::BAD_TARGET, format!("unknown target `{name}`")).rpc())?,
        Some(_) => return Err(RpcError::wrong_type("target", "a string")),
    };
    let device = match params.get("device") {
        None | Some(Value::Null) => None,
        Some(Value::String(serial)) if !serial.is_empty() => Some(serial.clone()),
        Some(_) => return Err(RpcError::wrong_type("device", "an adb serial string")),
    };
    let wait = params.get("wait").and_then(Value::as_bool).unwrap_or(false);
    let mirror = match params.get("mirror") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(on)) => *on,
        Some(_) => return Err(RpcError::wrong_type("mirror", "a boolean")),
    };
    if mirror && target != LaunchTarget::Android {
        return Err(RpcError::new(
            INVALID_PARAMS,
            crate::device_mirror::codes::ANDROID_ONLY,
            "mirror is for android launches",
            "pass target: \"android\" with mirror, or leave mirror out",
        ));
    }
    // The handler holds the editor frame while it waits: never longer than the bound, whatever
    // `timeout_ms` asks (ADR-7548).
    let timeout = Duration::from_millis(
        params
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .map_or(WAIT_BOUND_MS, |ms| ms.min(WAIT_BOUND_MS)),
    );
    if let Some(name) = params.get("profile") {
        let name = name.as_str().ok_or_else(|| {
            LaunchError::new(codes::PROFILE_INVALID, "profile must be a saved profile name").rpc()
        })?;
        if params.get("target").is_some() || params.get("device").is_some() {
            return Err(LaunchError::new(
                codes::PROFILE_INVALID,
                "A saved profile owns target and device; omit those overrides",
            )
            .rpc());
        }
        let profile = crate::launch_profiles::resolve(world, name)?;
        let target = LaunchTarget::parse(&profile.target)
            .ok_or_else(|| LaunchError::new(codes::BAD_TARGET, "Bad saved target").rpc())?;
        launch_selected(world, target, profile.device.clone(), Some(profile)).map_err(|e| e.rpc())?;
    } else {
        launch(world, target, device).map_err(|e| e.rpc())?;
    }
    // ADR-7836: mirror the launch's device; a mirror that cannot start leaves the launch running and
    // says why in `mirror_error`.
    let mirror_error = if mirror {
        let serial = world.resource::<Launcher>().status().and_then(|s| s.device);
        crate::device_mirror::start(world, serial, crate::device_mirror::DEFAULT_INTERVAL_MS)
            .err()
            .map(|e| e.message)
    } else {
        None
    };
    let launcher = world.resource::<Launcher>();
    let settled = |s: &LaunchStatus| s.state == LaunchState::Running || !s.state.is_active();
    if wait
        && launcher.wait_for(settled, timeout)
        && let Some(error) = launcher.status().and_then(|s| s.error)
    {
        return Err(error.rpc());
    }
    let pending = !launcher.status().is_some_and(|s| settled(&s));
    let mut out = launcher.status_json(20);
    if let Value::Object(map) = &mut out {
        map.insert("pending".into(), json!(pending));
        if mirror {
            map.insert("mirror".into(), json!(mirror_error.is_none()));
            map.insert("mirror_error".into(), json!(mirror_error));
        }
    }
    Ok(out)
}

/// `editor.launch_status { tail? }`.
fn editor_launch_status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let tail = tail_param(params, 40, TAIL_LINES)?;
    Ok(world
        .get_resource::<Launcher>()
        .map_or_else(|| Launcher::default().status_json(0), |l| l.status_json(tail)))
}

/// `editor.launch_stop`.
fn editor_launch_stop(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let stopped = stop(world);
    let mut out = world
        .get_resource::<Launcher>()
        .map_or_else(|| Launcher::default().status_json(0), |l| l.status_json(20));
    if let Value::Object(map) = &mut out {
        map.insert("stopped".into(), json!(stopped));
    }
    Ok(out)
}

/// `editor.launch_devices`.
fn editor_launch_devices(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let launcher = world.get_resource_or_insert_with(Launcher::default);
    let (adb, devices) = launcher.list_devices().map_err(|e| e.rpc())?;
    Ok(json!({
        "adb": adb.display().to_string(),
        "devices": devices
            .iter()
            .map(|d| json!({ "serial": d.serial, "state": d.state, "model": d.model, "usable": d.usable() }))
            .collect::<Vec<_>>(),
    }))
}

const STATUS_RESULT: &str = "{ id, target, device, state, running, step, steps, pid, exit_code, artifact, \
    agent: string | null, error: { code, message, log_tail } | null, lines, log_tail, pushed, push_failed, push_error, transport: { host_port } | null }";

/// The launch methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.launch",
                tool: "launch",
                description: "Launch the open project outside the editor: `standalone` cooks for the host, builds the \
                    launcher with cargo and runs it in the open level; `android` lists devices with adb, packages the \
                    debug APK, installs it, starts the activity and streams logcat. Output goes to the Output Log as \
                    `[launch <id>]` lines. Refusals answer at once (launch_no_project, launch_bad_target, launch_busy, \
                    launch_no_adb, launch_no_device, launch_device_ambiguous, launch_no_app_id, launch_bad_app_id). \
                    The answer carries `pending: true` while the launch has neither run nor ended: poll \
                    editor.launch_status. `wait` holds the editor for at most 1 s: a step that fails by then answers \
                    its code (launch_build_failed, launch_install_failed, launch_start_failed, launch_spawn_failed) \
                    with `data.log_tail`; a longer build answers `pending: true`.",
                params: &[
                    ParamSchema {
                        name: "profile",
                        kind: "string",
                        required: false,
                        description: "Saved launch profile name (editor.launch_profiles); omit target and device when supplied. On Android the start level and extra arguments reach the game in the launch intent (ADR-8472); package: false returns launch_profile_unsupported.",
                    },
                    ParamSchema {
                        name: "target",
                        kind: "string",
                        required: false,
                        description: "\"standalone\" (default) or \"android\".",
                    },
                    ParamSchema {
                        name: "device",
                        kind: "string",
                        required: false,
                        description: "Android: the adb serial (editor.launch_devices). Default: the only attached device.",
                    },
                    ParamSchema {
                        name: "wait",
                        kind: "boolean",
                        required: false,
                        description: "Wait up to 1 s for the launch to run or fail (default false); still \
                            building after that answers `pending: true`.",
                    },
                    ParamSchema {
                        name: "timeout_ms",
                        kind: "integer",
                        required: false,
                        description: "With `wait`: wait at most this long (default and maximum 1000; larger \
                            values are clamped, the editor frame is never held longer).",
                    },
                    ParamSchema {
                        name: "mirror",
                        kind: "boolean",
                        required: false,
                        description: "Android: also mirror the device's screen in the editor \
                            (editor.device_mirror; mirror_android_only on other targets).",
                    },
                ],
                result: "{ pending, mirror?, mirror_error?, ...editor.launch_status }",
                mutating: true,
                permission: Permission::Build,
            },
            editor_launch as Handler,
        ),
        (
            MethodSchema {
                name: "editor.launch_status",
                tool: "launch_status",
                description: "The current or last launch: state (idle, building, installing, starting, running, \
                    stopped, exited, failed), the steps run, the game's or app's pid, the exit code, the built \
                    artifact, the error with its log tail, and the newest output lines.",
                params: &[ParamSchema {
                    name: "tail",
                    kind: "integer",
                    required: false,
                    description: "Output lines to return (default 40, at most 400).",
                }],
                result: STATUS_RESULT,
                mutating: false,
                permission: Permission::Read,
            },
            editor_launch_status as Handler,
        ),
        (
            MethodSchema {
                name: "editor.launch_stop",
                tool: "launch_stop",
                description: "Stop the launch: an Android app is force-stopped and logcat ends; the game (or the \
                    running build) is killed with its process tree and reaped before the answer. `stopped: false` \
                    when nothing was building or running.",
                params: &[],
                result: "{ stopped, ...editor.launch_status }",
                mutating: true,
                permission: Permission::Build,
            },
            editor_launch_stop as Handler,
        ),
        (
            MethodSchema {
                name: "editor.launch_devices",
                tool: "launch_devices",
                description: "The Android devices adb sees (`adb devices -l`) with their state; only `usable` ones \
                    (state `device`) can be launched on. launch_no_adb when adb is not found.",
                params: &[],
                result: "{ adb, devices: [{ serial, state, model, usable }] }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_launch_devices as Handler,
        ),
    ]
}

// ---------------------------------------------------------------------------
// The launched game's agent connection (ADR-7833)
// ---------------------------------------------------------------------------

/// The agent directory standalone launch `id` of `project` gives its game (`REGIA_AGENT_DIR`):
/// `<project>/.regia/launch/agent-<id>`, apart from the editor's own endpoint. A game built with
/// the agent feature advertises its endpoint and token there; the remote Outliner and Inspector
/// (ADR-7833) and the asset pusher (ADR-7836) both connect there (one path since integ-v06k).
#[must_use]
pub fn agent_dir_in(project: &Path, id: u32) -> PathBuf {
    regia_core::brand::project_state_dir(project)
        .join("launch")
        .join(format!("agent-{id}"))
}

/// The running standalone launch's id and agent directory, for the remote Outliner and Inspector
/// (`crate::remote_world`). `None` without a project, a launch, or while it builds, has ended or
/// runs on a device.
#[must_use]
pub fn launch_agent_dir(world: &World) -> Option<(u32, PathBuf)> {
    let project = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())?;
    let status = world.get_resource::<Launcher>()?.status()?;
    (status.target == LaunchTarget::Standalone && status.state == LaunchState::Running)
        .then(|| (status.id, agent_dir_in(&project, status.id)))
}

/// The running Android launch whose agent transport is open (ADR-8472): its id, the device serial
/// and the launch agent directory holding the forwarded endpoint. `None` without a project, an
/// Android launch, while it builds or has ended, or when the forward did not open.
#[must_use]
pub fn device_agent_dir(world: &World) -> Option<(u32, String, PathBuf)> {
    let project = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())?;
    let launcher = world.get_resource::<Launcher>()?;
    let run = launcher.current.as_ref()?;
    let forwarded = run.shared.lock().forward.is_some();
    let status = run.status();
    (status.target == LaunchTarget::Android && status.state == LaunchState::Running && forwarded).then(|| {
        (
            status.id,
            status.device.unwrap_or_default(),
            agent_dir_in(&project, status.id),
        )
    })
}

// ---------------------------------------------------------------------------
// Asset push into a launched game (P-301, ADR-7836, book/agents/asset-push.md)
// ---------------------------------------------------------------------------

/// How long one `assets.push` call may wait for the game's answer.
const PUSH_CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// One change the editor pushes into a launched game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushItem {
    /// A cooked asset the live cooker rebuilt (its `.fckd` in the editor's cooked cache).
    Cooked(regia_asset::AssetId),
    /// A source file the runtime reads directly (a UI document), absolute, under `assets/`.
    Source(PathBuf),
    /// An engine shader (absolute path of the `.wgsl`).
    Shader(PathBuf),
}

impl PushItem {
    /// The pushes for a batch of live-cooker messages: every cooked reload, every changed UI
    /// document under the project's `assets/` and every changed engine shader, each once.
    #[must_use]
    pub fn from_reloads(project: &Path, messages: &[regia_asset::ReloadMessage]) -> Vec<Self> {
        let mut out: Vec<Self> = Vec::new();
        for message in messages {
            let item = match message {
                regia_asset::ReloadMessage::Reload(id) => Self::Cooked(*id),
                regia_asset::ReloadMessage::Source(path) => match regia_asset::SourceKind::of(path) {
                    regia_asset::SourceKind::UiLayout if path.starts_with(project.join("assets")) => {
                        Self::Source(path.clone())
                    }
                    regia_asset::SourceKind::Shader
                        if regia_render::shader_reload::is_engine_shader(path) =>
                    {
                        Self::Shader(path.clone())
                    }
                    _ => continue,
                },
                regia_asset::ReloadMessage::Status(_) => continue,
            };
            if !out.contains(&item) {
                out.push(item);
            }
        }
        out
    }
}

/// A connection to a launched game's agent server that can carry `assets.push` calls.
pub trait PushTransport: Send {
    /// Calls `method` with `params`; the error is the reason as text.
    ///
    /// # Errors
    ///
    /// The transport failed or the game answered an error.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String>;
}

/// Opens a [`PushTransport`] to the game whose agent directory is the argument.
pub type PushConnector = Arc<dyn Fn(&Path) -> Result<Box<dyn PushTransport>, String> + Send + Sync>;

/// Cooks a project (first argument) for a platform (second) before a device push reads its cooked
/// cache; the error is the reason as text (ADR-8472).
pub type PushCooker = Arc<dyn Fn(&Path, &str) -> Result<(), String> + Send + Sync>;

/// The cooked-cache platform of an Android launch.
const ANDROID_PLATFORM: &str = "android";

/// The default [`PushCooker`]: an incremental cook of the project for the platform (only the sources
/// that changed since the last cook are cooked again).
fn default_push_cooker() -> PushCooker {
    Arc::new(|project: &Path, platform: &str| {
        let report = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
            project_root: project.to_path_buf(),
            platform: platform.to_owned(),
            incremental: true,
        })
        .map_err(|e| e.to_string())?;
        match report.errors.first() {
            Some((path, error)) => Err(format!("{path}: {error}")),
            None => Ok(()),
        }
    })
}

/// Removes the `adb forward` of `host_port` on `device` (ADR-8472); a failure is logged.
fn remove_forward(tools: &LaunchTools, adb: &Path, device: &str, host_port: u16) {
    let mut cmd = tools.adb(adb, device);
    cmd.args(crate::device_transport::remove_forward_args(host_port));
    if let Err(e) = run_short(cmd, SHORT_CALL) {
        tracing::warn!(target: LOG_TARGET, "launch: adb forward --remove tcp:{host_port}: {e}");
    }
}

/// The agent client as a [`PushTransport`].
struct ClientTransport(regia_agent::client::Client);

impl PushTransport for ClientTransport {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.0.call(method, params).map_err(|e| e.to_string())
    }
}

/// The default connector: the endpoint and token the game wrote in `dir`, over the WebSocket.
fn connect_agent_dir(dir: &Path) -> Result<Box<dyn PushTransport>, String> {
    let (endpoint, token) = regia_agent::endpoint::read(dir).map_err(|e| {
        format!(
            "the game's agent endpoint in {} is not readable yet: {e}",
            dir.display()
        )
    })?;
    let mut client =
        regia_agent::client::Client::connect(&endpoint.url, &token).map_err(|e| e.to_string())?;
    client
        .set_read_timeout(Some(PUSH_CALL_TIMEOUT))
        .map_err(|e| e.to_string())?;
    Ok(Box::new(ClientTransport(client)))
}

/// The push counters of the current launch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushCounts {
    /// Pushes the game accepted.
    pub pushed: u64,
    /// Pushes that failed (no connection, a refusal, a read error).
    pub failed: u64,
    /// The newest failure.
    pub last_error: Option<String>,
}

/// One launch's pusher: a worker thread fed by a channel; it ends when the launch is dropped.
struct PushLink {
    tx: std::sync::mpsc::Sender<PushItem>,
    counts: Arc<Mutex<PushCounts>>,
    /// Pushes queued (for waits in tests and the agent): the worker answers each.
    sent: Arc<(Mutex<u64>, Condvar)>,
}

impl PushLink {
    /// `device_cook`: the game runs on an Android device, so cooked assets come from the project's
    /// `android` cooked cache, which this cooks (once per batch of pushes) before reading it.
    fn spawn(
        id: u32,
        agent_dir: PathBuf,
        project: PathBuf,
        connector: Option<PushConnector>,
        device_cook: Option<PushCooker>,
    ) -> Self {
        let (tx, rx) = channel::<PushItem>();
        let counts = Arc::new(Mutex::new(PushCounts::default()));
        let sent = Arc::new((Mutex::new(0u64), Condvar::new()));
        let worker_counts = Arc::clone(&counts);
        let worker_sent = Arc::clone(&sent);
        let platform = if device_cook.is_some() {
            ANDROID_PLATFORM.to_owned()
        } else {
            regia_asset::current_platform()
        };
        let cache = regia_asset::cooked_cache_dir(&project, &platform);
        let spawned = std::thread::Builder::new()
            .name(format!("launch-{id}-push"))
            .spawn(move || {
                let connector: PushConnector = connector.unwrap_or_else(|| Arc::new(connect_agent_dir));
                let mut link: Option<Box<dyn PushTransport>> = None;
                // A batch is what is queued when the worker wakes: one device cook covers it.
                while let Ok(first) = rx.recv() {
                    let batch: Vec<PushItem> = std::iter::once(first).chain(rx.try_iter()).collect();
                    let _span = tracing::info_span!(target: LOG_TARGET, "launch_push", items = batch.len()).entered();
                    let cooked = match &device_cook {
                        Some(cook) if batch.iter().any(|item| matches!(item, PushItem::Cooked(_))) => {
                            let _cook = tracing::info_span!(target: LOG_TARGET, "launch_push_device_cook").entered();
                            (**cook)(&project, &platform)
                        }
                        _ => Ok(()),
                    };
                    for item in batch {
                        let result = match (&cooked, &item) {
                            (Err(e), PushItem::Cooked(_)) => Err(format!("the {platform} cook failed: {e}")),
                            _ => push_one(&item, &cache, &project, &agent_dir, &connector, &mut link),
                        };
                        {
                            let mut counts = worker_counts
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            match &result {
                                Ok(()) => counts.pushed += 1,
                                Err(e) => {
                                    counts.failed += 1;
                                    counts.last_error = Some(e.clone());
                                }
                            }
                        }
                        match result {
                            Ok(()) => tracing::info!(target: LOG_TARGET, "[launch {id}] pushed {item:?}"),
                            Err(e) => {
                                tracing::warn!(target: LOG_TARGET, "[launch {id}] push of {item:?} failed: {e}")
                            }
                        }
                        let (done, changed) = &*worker_sent;
                        *done.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
                        changed.notify_all();
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(target: LOG_TARGET, "launch {id}: no asset pusher ({e})");
        }
        Self { tx, counts, sent }
    }

    fn counts(&self) -> PushCounts {
        self.counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Reads the bytes of `item` and pushes them, connecting first when there is no connection. A
/// failed call drops the connection, so the next push reconnects (the game may have restarted).
fn push_one(
    item: &PushItem,
    cache: &Path,
    project: &Path,
    agent_dir: &Path,
    connector: &PushConnector,
    link: &mut Option<Box<dyn PushTransport>>,
) -> Result<(), String> {
    let (target, path) = match item {
        PushItem::Cooked(id) => (
            json!({ "asset": id.to_string() }),
            cache.join(regia_asset::cooked_file_name(*id)),
        ),
        PushItem::Source(path) => {
            let rel = path
                .strip_prefix(project)
                .map_err(|_| format!("{} is not in the project", path.display()))?;
            let rel: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            (json!({ "source": rel.join("/") }), path.clone())
        }
        PushItem::Shader(path) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            (json!({ "shader": name }), path.clone())
        }
    };
    let bytes = std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if bytes.len() > regia_asset::push::PUSH_MAX_BYTES {
        return Err(format!(
            "{} is {} bytes, over the push limit of {} (relaunch to see it)",
            path.display(),
            bytes.len(),
            regia_asset::push::PUSH_MAX_BYTES
        ));
    }
    let mut params = target;
    if let Value::Object(map) = &mut params {
        map.insert("data".into(), json!(regia_agent::base64_encode(&bytes)));
    }
    if link.is_none() {
        *link = Some((**connector)(agent_dir)?);
    }
    let Some(transport) = link.as_mut() else {
        return Err("no connection to the game".to_owned());
    };
    match transport.call("assets.push", params) {
        Ok(_) => Ok(()),
        Err(e) => {
            *link = None;
            Err(e)
        }
    }
}

impl Launcher {
    /// The push counters of the current (or last) launch; zero without one.
    #[must_use]
    pub fn push_counts(&self) -> PushCounts {
        self.current
            .as_ref()
            .and_then(|run| run.push.as_ref())
            .map(PushLink::counts)
            .unwrap_or_default()
    }

    /// Queues pushes into the running launched game (standalone, or on an Android device through
    /// its agent transport, ADR-8472); returns how many were queued (none when no launch is running).
    pub fn queue_push(&self, items: Vec<PushItem>) -> usize {
        let Some(run) = &self.current else {
            return 0;
        };
        let Some(push) = &run.push else {
            return 0;
        };
        if run.status().state != LaunchState::Running {
            return 0;
        }
        let mut queued = 0;
        for item in items {
            if push.tx.send(item).is_ok() {
                queued += 1;
            }
        }
        queued
    }

    /// Blocks until the pusher answered `count` pushes in all (counted from the launch's start) or
    /// `timeout` passes; returns whether it did.
    pub fn wait_pushes(&self, count: u64, timeout: Duration) -> bool {
        let Some(push) = self.current.as_ref().and_then(|run| run.push.as_ref()) else {
            return false;
        };
        let (done, changed) = &*push.sent;
        let deadline = Instant::now() + timeout;
        let mut guard = done.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        while *guard < count {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            guard = match changed.wait_timeout(guard, left) {
                Ok((g, _)) => g,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        true
    }
}

/// Forwards a batch of live-cooker messages to the running launch as pushes (called by
/// `asset_watch::pump_asset_watch` once per batch). Returns how many pushes were queued.
pub fn forward_reloads(world: &World, messages: &[regia_asset::ReloadMessage]) -> usize {
    let Some(launcher) = world.get_resource::<Launcher>() else {
        return 0;
    };
    let Some(project) = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
    else {
        return 0;
    };
    launcher.queue_push(PushItem::from_reloads(&project, messages))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review sprint-w12-rev-g15 #5: a shipping launch on a project whose `Cargo.toml` lacks
    /// `[profile.shipping]` defines it the way `regia package` does; a project with the table keeps
    /// its own.
    #[test]
    fn a_shipping_launch_defines_the_profile_when_the_manifest_lacks_it() {
        let dir = regia_test_utils::unique_temp_dir("launch-shipping-config");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"game\"\nversion = \"0.1.0\"\n",
        )
        .expect("manifest");
        let args = profile_cargo_args(&dir, "shipping").expect("args");
        assert_eq!(&args[..2], ["--profile", "shipping"]);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--config" && w[1] == "profile.shipping.inherits=\"release\""),
            "{args:?}"
        );
        assert_eq!(
            args.iter().filter(|a| *a == "--config").count(),
            SHIPPING_PROFILE.len()
        );
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"game\"\nversion = \"0.1.0\"\n[profile.shipping]\ninherits = \"release\"\n",
        )
        .expect("manifest");
        assert_eq!(
            profile_cargo_args(&dir, "shipping").expect("args"),
            ["--profile", "shipping"]
        );
        assert_eq!(
            profile_cargo_args(&dir, "dev").expect("args"),
            ["--profile", "dev"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn adb_devices_parse_with_states_and_models() {
        let text = "List of devices attached\n\
            * daemon started successfully\n\
            54101FDAP0015H         device usb:1-2 product:caiman model:Pixel_9_Pro device:caiman transport_id:4\n\
            emulator-5554\toffline\n\
            R9X\tunauthorized usb:3-1 transport_id:7\n\n";
        let devices = parse_adb_devices(text);
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0].serial, "54101FDAP0015H");
        assert_eq!(devices[0].model.as_deref(), Some("Pixel_9_Pro"));
        assert!(devices[0].usable());
        assert_eq!(devices[1].state, "offline");
        assert!(!devices[2].usable());
        assert!(parse_adb_devices("List of devices attached\n\n").is_empty());
    }

    #[test]
    fn a_device_is_chosen_only_when_unambiguous() {
        let devices = parse_adb_devices("A\tdevice\nB\tunauthorized\n");
        assert_eq!(choose_device(&devices, None).as_deref(), Ok("A"));
        assert_eq!(choose_device(&devices, Some("A")).as_deref(), Ok("A"));
        assert_eq!(
            choose_device(&devices, Some("B")).map_err(|e| e.code),
            Err(codes::NO_DEVICE)
        );
        assert_eq!(
            choose_device(&devices, Some("C")).map_err(|e| e.code),
            Err(codes::NO_DEVICE)
        );
        let two = parse_adb_devices("A\tdevice\nB\tdevice\n");
        assert_eq!(
            choose_device(&two, None).map_err(|e| e.code),
            Err(codes::DEVICE_AMBIGUOUS)
        );
        assert_eq!(
            choose_device(&[], None).map_err(|e| e.code),
            Err(codes::NO_DEVICE)
        );
    }

    #[test]
    fn the_cargo_executable_is_the_packages_bin() {
        let stdout = r#"{"reason":"compiler-artifact","package_id":"path+file:///w/engine#regia_core@0.6.0","target":{"kind":["lib"],"name":"regia_core"},"executable":null}
{"reason":"compiler-artifact","package_id":"path+file:///w/tool#helper@0.1.0","target":{"kind":["bin"],"name":"helper"},"executable":"/t/debug/helper"}
{"reason":"compiler-artifact","package_id":"path+file:///w/launcher#hello-launcher@0.1.0","target":{"kind":["bin"],"name":"hello"},"executable":"/t/debug/hello"}
{"reason":"build-finished","success":true}"#;
        assert_eq!(
            parse_cargo_executable(stdout, "hello-launcher"),
            Some(PathBuf::from("/t/debug/hello"))
        );
        assert_eq!(
            parse_cargo_executable(stdout, "helper"),
            Some(PathBuf::from("/t/debug/helper"))
        );
        assert_eq!(parse_cargo_executable("not json\n", "x"), None);
    }

    /// ADR-7548: when no artifact belongs to the launcher package the launch refuses instead of
    /// running some other binary the build produced.
    #[test]
    fn no_executable_of_the_package_is_refused_not_replaced() {
        let stdout = r#"{"reason":"compiler-artifact","package_id":"path+file:///w/tool#helper@0.1.0","target":{"kind":["bin"],"name":"helper"},"executable":"/t/debug/helper"}
{"reason":"compiler-artifact","package_id":"path+file:///w/launcher#hello-launcher@0.1.0","target":{"kind":["bin"],"name":"hello"},"executable":"/t/debug/hello"}
{"reason":"build-finished","success":true}"#;
        assert_eq!(
            parse_cargo_executable(stdout, "other"),
            None,
            "an executable of another package is never the launcher"
        );
        assert_eq!(
            parse_cargo_executable(stdout, "hello-laun"),
            None,
            "a name prefix is not the package"
        );
    }

    /// cargo's package id spells the name only in the URL path when it equals the directory name
    /// (`path+file:///w/my-launcher#0.1.0`), and as `name version (source)` before cargo 1.77.
    #[test]
    fn the_package_is_found_in_every_package_id_spelling() {
        let new_dir = r#"{"reason":"compiler-artifact","package_id":"path+file:///w/my-launcher#0.1.0","target":{"kind":["bin"],"name":"game"},"executable":"/t/debug/game"}"#;
        assert_eq!(
            parse_cargo_executable(new_dir, "my-launcher"),
            Some(PathBuf::from("/t/debug/game"))
        );
        assert_eq!(parse_cargo_executable(new_dir, "launcher"), None);
        let old = r#"{"reason":"compiler-artifact","package_id":"my-launcher 0.1.0 (path+file:///w/x)","target":{"kind":["bin"],"name":"game"},"executable":"/t/debug/game"}"#;
        assert_eq!(
            parse_cargo_executable(old, "my-launcher"),
            Some(PathBuf::from("/t/debug/game"))
        );
    }

    #[test]
    fn package_artifact_and_pidof_parse() {
        let out =
            "noise\n{\"ok\":true,\"artifact\":\"/p/dist/android/app-debug.apk\",\"platform\":\"android\"}\n";
        assert_eq!(
            parse_package_artifact(out),
            Some(PathBuf::from("/p/dist/android/app-debug.apk"))
        );
        assert_eq!(parse_package_artifact("{\"ok\":false,\"error\":\"x\"}"), None);
        assert_eq!(parse_pidof("4242 4243\n"), Some(4242));
        assert_eq!(parse_pidof("\n"), None);
    }

    #[test]
    fn error_codes_are_protocol_spelled_and_carry_the_tail() {
        for code in codes::ALL {
            assert!(code.starts_with("launch_"), "{code}");
            assert!(
                code.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{code}"
            );
        }
        let error = LaunchError {
            code: codes::BUILD_FAILED,
            message: "cargo build failed".into(),
            log_tail: vec!["error[E0425]".into()],
        };
        let rpc = error.rpc();
        let data = rpc.data.expect("data");
        assert_eq!(data["code"], "launch_build_failed");
        assert_eq!(data["log_tail"][0], "error[E0425]");
    }

    #[test]
    fn the_sdk_lookup_can_be_switched_off_and_path_is_searched_in_order() {
        let dir = std::env::temp_dir().join(format!("regia-launch-path-{}", std::process::id()));
        let first = dir.join("first");
        let second = dir.join("second");
        std::fs::create_dir_all(&first).expect("first");
        std::fs::create_dir_all(&second).expect("second");
        let exe = format!("adb{}", std::env::consts::EXE_SUFFIX);
        std::fs::write(second.join(&exe), b"").expect("fake");
        let tools = LaunchTools {
            sdk: SdkSource::None,
            path_env: Some(std::env::join_paths([&first, &second]).expect("join")),
            ..LaunchTools::default()
        };
        assert_eq!(tools.sdk_dir(), None);
        assert_eq!(tools.adb_program(), Some(second.join(&exe)));
        std::fs::write(first.join(&exe), b"").expect("fake");
        assert_eq!(
            tools.adb_program(),
            Some(first.join(&exe)),
            "the first PATH entry wins"
        );
        let sdk = dir.join("sdk");
        std::fs::create_dir_all(sdk.join("platform-tools")).expect("sdk");
        std::fs::write(sdk.join("platform-tools").join(&exe), b"").expect("sdk adb");
        let with_sdk = LaunchTools {
            sdk: SdkSource::Dir(sdk.clone()),
            ..tools
        };
        assert_eq!(
            with_sdk.adb_program(),
            Some(sdk.join("platform-tools").join(&exe)),
            "the SDK's adb comes before PATH"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn targets_and_states_have_protocol_names() {
        assert_eq!(LaunchTarget::parse("android"), Some(LaunchTarget::Android));
        assert_eq!(
            LaunchTarget::parse("standalone").map(LaunchTarget::as_str),
            Some("standalone")
        );
        assert_eq!(LaunchTarget::parse("ios"), None);
        assert!(LaunchState::Running.is_active());
        assert!(!LaunchState::Stopped.is_active());
        assert_eq!(LaunchState::Failed.as_str(), "failed");
        let idle = Launcher::default().status_json(5);
        assert_eq!(idle["state"], "idle");
    }

    /// A fake game connection that records each call and answers from a list.
    struct FakeTransport {
        calls: Arc<Mutex<Vec<(String, Value)>>>,
        fail: bool,
    }

    impl PushTransport for FakeTransport {
        fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((method.to_owned(), params));
            if self.fail {
                Err("push_io: disk full".to_owned())
            } else {
                Ok(json!({ "changed": true }))
            }
        }
    }

    #[test]
    fn launch_push_items_come_from_cooked_reloads_ui_documents_and_engine_shaders() {
        let project = Path::new("/p");
        let asset = regia_asset::AssetId::new_random();
        let shader_dir = regia_render::shader_reload::engine_shader_dir().unwrap_or_default();
        let messages = [
            regia_asset::ReloadMessage::Reload(asset),
            regia_asset::ReloadMessage::Reload(asset),
            regia_asset::ReloadMessage::Source(project.join("assets/ui/hud.ui.ron")),
            regia_asset::ReloadMessage::Source(PathBuf::from("/elsewhere/assets/ui/x.ui.ron")),
            regia_asset::ReloadMessage::Source(project.join("assets/levels/a.level.ron")),
            regia_asset::ReloadMessage::Source(shader_dir.join("pbr_lighting.wgsl")),
            regia_asset::ReloadMessage::Source(project.join("assets/shaders/mine.wgsl")),
        ];
        let items = PushItem::from_reloads(project, &messages);
        assert_eq!(
            items,
            vec![
                PushItem::Cooked(asset),
                PushItem::Source(project.join("assets/ui/hud.ui.ron")),
                PushItem::Shader(shader_dir.join("pbr_lighting.wgsl")),
            ],
            "each change once; UI documents only from this project; only engine shaders"
        );
    }

    #[test]
    fn launch_push_sends_base64_bytes_and_reconnects_after_a_failed_call() {
        let tmp = regia_test_utils::unique_temp_dir("launch-push-one");
        let project = tmp.join("project");
        let cache = tmp.join("cache");
        std::fs::create_dir_all(project.join("assets/ui")).expect("dirs");
        std::fs::create_dir_all(&cache).expect("cache");
        let asset = regia_asset::AssetId::new_random();
        std::fs::write(cache.join(regia_asset::cooked_file_name(asset)), b"abc").expect("cooked");
        std::fs::write(project.join("assets/ui/hud.ui.ron"), b"(root: ())").expect("ui");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let connects = Arc::new(Mutex::new(0u32));
        let fail = Arc::new(Mutex::new(true));
        let connector: PushConnector = {
            let calls = Arc::clone(&calls);
            let connects = Arc::clone(&connects);
            let fail = Arc::clone(&fail);
            Arc::new(move |_dir: &Path| {
                *connects.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
                let fail = *fail.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                Ok(Box::new(FakeTransport {
                    calls: Arc::clone(&calls),
                    fail,
                }) as Box<dyn PushTransport>)
            })
        };
        let mut link = None;
        let dir = tmp.join("agent");
        let first = push_one(
            &PushItem::Cooked(asset),
            &cache,
            &project,
            &dir,
            &connector,
            &mut link,
        );
        assert!(first.is_err(), "the game refused the first push");
        assert!(link.is_none(), "a failed call drops the connection");
        *fail.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = false;
        push_one(
            &PushItem::Cooked(asset),
            &cache,
            &project,
            &dir,
            &connector,
            &mut link,
        )
        .expect("pushed");
        let source = PushItem::Source(project.join("assets/ui/hud.ui.ron"));
        push_one(&source, &cache, &project, &dir, &connector, &mut link).expect("source pushed");
        assert_eq!(
            *connects.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
            2,
            "one reconnect after the failure, then the connection is kept"
        );
        let calls = calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[1].0, "assets.push");
        assert_eq!(calls[1].1["asset"], asset.to_string());
        assert_eq!(calls[1].1["data"], "YWJj", "standard base64 of the cooked bytes");
        assert_eq!(calls[2].1["source"], "assets/ui/hud.ui.ron");
        let missing = PushItem::Cooked(regia_asset::AssetId::new_random());
        let err = push_one(&missing, &cache, &project, &dir, &connector, &mut link).expect_err("no file");
        assert!(err.contains("cannot read"), "{err}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn launch_push_counts_are_zero_and_nothing_queues_without_a_running_launch() {
        let launcher = Launcher::default();
        assert_eq!(launcher.push_counts(), PushCounts::default());
        assert_eq!(
            launcher.queue_push(vec![PushItem::Cooked(regia_asset::AssetId::new_random())]),
            0
        );
        assert!(!launcher.wait_pushes(1, Duration::from_millis(1)));
        let idle = launcher.status_json(0);
        assert_eq!(idle["pushed"], 0);
        assert_eq!(
            agent_dir_in(Path::new("/p"), 7),
            regia_core::brand::project_state_dir(Path::new("/p"))
                .join("launch")
                .join("agent-7")
        );
    }
}
