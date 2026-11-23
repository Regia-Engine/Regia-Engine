// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask package-smoke` (ADR-3968): package, extract, play a scripted input recording for
//! 60 seconds, and fail on an `ERROR` record or a dirty `summary.json`.
//!
//! Linux runs under `xvfb` with lavapipe. `--android` builds the sim_bridge APK and AAB (the
//! manifest ABIs, arm64-v8a and x86_64) and records sizes against the 150 MB shipping-APK limit.
//! `--android-emulator` boots an x86_64 API 35 image when `/dev/kvm` can be opened read-write;
//! otherwise the commands are printed and the rest of the run still completes. `--require-kvm`
//! makes a missing KVM device a failure (the GitHub job). `--windows` on Linux prints the
//! commands for the owner's Windows rig and does not build.
//!
//! The JSON report (`format_version` 1) is a CI artifact, not a content format: nothing migrates
//! it and `cargo xtask check-formats` does not hash it.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use regia_framework::startup_playback::{
    self, MISSING_MESSAGE, PLAYING_MESSAGE, REJECTED_MESSAGE, TIMING_STARTED_MESSAGE,
};

/// Shipping APKs at or above this fail the smoke (150 MB).
pub const APK_LIMIT_BYTES: u64 = 150_000_000;

/// `summary.json` / the report's schema note for readers. Not a frozen content format.
pub const REPORT_FORMAT_VERSION: u32 = 1;

/// Android system image the emulator smoke creates its AVD from.
pub const EMULATOR_API: u32 = 35;

/// Application id of `samples/sim_bridge`.
pub const SIM_BRIDGE_PACKAGE: &str = "com.regiaengine.samples.sim_bridge";

/// Activity the emulator launches.
pub const SIM_BRIDGE_ACTIVITY: &str = ".RegiaActivity";

/// Default AVD name.
pub const SMOKE_AVD: &str = "regia_smoke";

/// Emulator console port (`emulator-5554`).
pub const EMULATOR_PORT: u16 = 5554;

/// One `ERROR` record from a JSON-lines log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogHit {
    /// 1-based line number.
    pub line: usize,
    /// The record's `target` field (empty when the line was not a record).
    pub target: String,
    /// The record's `msg` field, or why the line was rejected.
    pub message: String,
}

/// How a hit is named in the smoke report: `target: message`.
#[must_use]
pub fn name_log_hit(hit: &LogHit) -> String {
    if hit.target.is_empty() {
        hit.message.clone()
    } else {
        format!("{}: {}", hit.target, hit.message)
    }
}

/// Scans a JSON-lines log. Blank lines and lines that do not start with `{` are ignored. A line
/// that starts with `{` but is not JSON fails. A record whose `level` is exactly `ERROR` fails
/// and is named by [`name_log_hit`].
///
/// # Errors
/// The hits, in order. `Ok(())` when nothing failed.
pub fn scan_jsonl(text: &str) -> Result<(), Vec<LogHit>> {
    let mut hits = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line_no = index + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => {
                hits.push(LogHit {
                    line: line_no,
                    target: String::new(),
                    message: format!("unparseable JSON on line {line_no}"),
                });
                continue;
            }
        };
        if value.get("level").and_then(|level| level.as_str()) == Some("ERROR") {
            hits.push(LogHit {
                line: line_no,
                target: value
                    .get("target")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                message: value
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    if hits.is_empty() { Ok(()) } else { Err(hits) }
}

/// The packaged game must log playback start and [`TIMING_STARTED_MESSAGE`] after consuming the recording.
///
/// # Errors
/// Why playback was not confirmed.
pub fn confirm_playback(jsonl: &str) -> Result<(), String> {
    if jsonl.contains(MISSING_MESSAGE) {
        return Err("playback was skipped because the recording was missing".to_string());
    }
    if jsonl.contains(REJECTED_MESSAGE) {
        return Err("playback was skipped because the recording was rejected".to_string());
    }
    if !jsonl.contains(PLAYING_MESSAGE) {
        return Err(format!("the log never recorded \"{PLAYING_MESSAGE}\""));
    }
    if !jsonl.contains(TIMING_STARTED_MESSAGE) {
        return Err(format!("the log never recorded \"{TIMING_STARTED_MESSAGE}\""));
    }
    Ok(())
}

/// Confirms that Android playback started and the app reached its requested clean exit.
///
/// # Errors
/// A missing playback or smoke-exit marker. Crashes and error lines are checked separately.
pub fn confirm_android_smoke(logcat: &str) -> Result<(), String> {
    confirm_playback(logcat)?;
    if !logcat.contains("smoke seconds elapsed; exiting") {
        return Err("Android logcat has no smoke exit marker".into());
    }
    Ok(())
}

/// Checks a `summary.json` body. `gpu_errors`, `device_lost`, `surface_errors` and `panics`
/// above zero fail (a missing count is zero). The exit record must be present, `clean` must be
/// true, and `panic` must not be true. A partial record fails.
///
/// # Errors
/// The first problem, naming the field.
pub fn check_summary_json(text: &str) -> Result<(), String> {
    let summary: regia_telemetry::SessionSummary =
        serde_json::from_str(text).map_err(|err| format!("summary.json: {err}"))?;
    if summary.partial == Some(true) {
        return Err("summary.json is a partial record".to_string());
    }
    let errors = summary.errors.clone().unwrap_or_default();
    for (name, value) in [
        ("gpu_errors", errors.gpu_errors),
        ("device_lost", errors.device_lost),
        ("surface_errors", errors.surface_errors),
        ("panics", errors.panics),
    ] {
        let value = value.unwrap_or(0);
        if value > 0 {
            return Err(format!("summary.json {name} = {value}"));
        }
    }
    let exit = summary
        .exit
        .ok_or_else(|| "summary.json has no exit record".to_string())?;
    if exit.clean != Some(true) {
        return Err("summary.json exit.clean is not true".to_string());
    }
    if exit.panic == Some(true) {
        return Err("summary.json exit.panic is true".to_string());
    }
    Ok(())
}

/// One row of the size table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeRow {
    /// Sample or project name.
    pub name: String,
    /// `dev` or `shipping`.
    pub profile: String,
    /// `linux`, `android` or `windows`.
    pub platform: String,
    /// `tar`, `apk`, `aab` or `zip`.
    pub format: String,
    /// Archive size in bytes.
    pub bytes: u64,
}

/// Whether this row is a shipping APK at or above [`APK_LIMIT_BYTES`]. Dev APKs are recorded and do not
/// fail the limit.
#[must_use]
pub fn apk_over_limit(row: &SizeRow) -> bool {
    row.format == "apk" && row.profile == "shipping" && row.bytes >= APK_LIMIT_BYTES
}

/// Fixed-width size table. A shipping APK over the limit is marked `OVER 150 MB`.
#[must_use]
pub fn format_size_table(rows: &[SizeRow]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<12} {:<10} {:<10} {:<8} {:>10} {}\n",
        "name", "profile", "platform", "format", "bytes", "note"
    ));
    for row in rows {
        let note = if apk_over_limit(row) { "OVER 150 MB" } else { "" };
        out.push_str(&format!(
            "{:<12} {:<10} {:<10} {:<8} {:>10} {}\n",
            row.name, row.profile, row.platform, row.format, row.bytes, note
        ));
    }
    out
}

/// One argv array the emulator smoke runs, plus optional stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedCommand {
    /// Program and arguments.
    pub argv: Vec<String>,
    /// Bytes written to stdin, then the pipe is closed.
    pub stdin: Option<String>,
    /// File streamed to stdin instead of text (binary recordings).
    pub stdin_file: Option<PathBuf>,
    /// Spawn and do not wait (the emulator process).
    pub background: bool,
}

impl PlannedCommand {
    /// POSIX shell rendering, including text or binary stdin redirection.
    #[must_use]
    pub fn display(&self) -> String {
        let command = format_command(&self.argv);
        let mut command = if let Some(path) = &self.stdin_file {
            format!("{command} < {}", shell_quote(&path.display().to_string()))
        } else if let Some(input) = &self.stdin {
            format!("printf %s {} | {command}", shell_quote(input))
        } else {
            command
        };
        if self.background {
            command.push_str(" &");
        }
        command
    }
}

/// Joins `argv` with spaces, quoting shell metacharacters for POSIX dry-run commands.
#[must_use]
pub fn format_command(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "_./:=+-".contains(ch))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// What [`emulator_commands`] needs. Paths are taken as given so a test can pin a fake SDK root.
#[derive(Debug, Clone)]
pub struct EmulatorRequest {
    /// Android SDK root (`cmdline-tools/latest`, `emulator`, `platform-tools`).
    pub sdk: PathBuf,
    /// Platform API level (`35`).
    pub api: u32,
    /// ABI (`x86_64`).
    pub abi: String,
    /// System-image tag (`google_apis`).
    pub image_tag: String,
    /// AVD name.
    pub avd: String,
    /// Application id.
    pub package_id: String,
    /// Activity class (`.RegiaActivity` or a full name).
    pub activity: String,
    /// adb serial (`emulator-5554`).
    pub serial: String,
    /// Emulator console port.
    pub port: u16,
    /// APK to install.
    pub apk: PathBuf,
    /// `.frec` file to push.
    pub playback: PathBuf,
    /// Seconds written into the app's smoke-seconds file.
    pub smoke_seconds: u64,
}

/// `adb` under `sdk`.
#[must_use]
pub fn adb_path(sdk: &Path) -> PathBuf {
    sdk.join("platform-tools").join(binary_name("adb"))
}

/// Builds the emulator smoke's command lines from a fixed SDK layout.
///
/// `avdmanager` is given `no` on stdin so the custom-hardware prompt does not block. The emulator
/// command is [`PlannedCommand::background`]. Logcat is not included; [`logcat_commands`] builds
/// those once a pid is known.
#[must_use]
pub fn emulator_commands(request: &EmulatorRequest) -> Vec<PlannedCommand> {
    let avdmanager = request
        .sdk
        .join("cmdline-tools")
        .join("latest")
        .join("bin")
        .join(binary_name("avdmanager"));
    let emulator = request.sdk.join("emulator").join(binary_name("emulator"));
    let adb = adb_path(&request.sdk);
    let image = format!(
        "system-images;android-{};{};{}",
        request.api, request.image_tag, request.abi
    );
    let port = request.port.to_string();
    let component = component_name(&request.package_id, &request.activity);
    let serial = request.serial.as_str();
    let pkg = request.package_id.as_str();
    let seconds_shell = format!("echo {} > files/regia-smoke-seconds", request.smoke_seconds);
    vec![
        planned(
            &[
                avdmanager.display().to_string(),
                "create".into(),
                "avd".into(),
                "-n".into(),
                request.avd.clone(),
                "-k".into(),
                image,
                "-d".into(),
                "pixel".into(),
                "--force".into(),
            ],
            Some("no\n".into()),
            false,
        ),
        planned(
            &[
                emulator.display().to_string(),
                "-avd".into(),
                request.avd.clone(),
                "-no-window".into(),
                "-no-audio".into(),
                "-no-snapshot".into(),
                "-no-boot-anim".into(),
                "-gpu".into(),
                "swiftshader_indirect".into(),
                "-port".into(),
                port,
                "-no-metrics".into(),
            ],
            None,
            true,
        ),
        adb_args(&adb, serial, &["wait-for-device"]),
        adb_args(&adb, serial, &["shell", "getprop", "sys.boot_completed"]),
        adb_args(
            &adb,
            serial,
            &["install", "-r", &request.apk.display().to_string()],
        ),
        adb_args(&adb, serial, &["shell", "run-as", pkg, "mkdir", "-p", "files"]),
        PlannedCommand {
            argv: vec![
                adb.display().to_string(),
                "-s".into(),
                serial.into(),
                "shell".into(),
                format!("run-as {pkg} sh -c 'cat > files/regia-play-input.frec'"),
            ],
            stdin: None,
            stdin_file: Some(request.playback.clone()),
            background: false,
        },
        adb_args(
            &adb,
            serial,
            &["shell", "run-as", pkg, "touch", "files/regia-play-input.on"],
        ),
        adb_args(
            &adb,
            serial,
            &["shell", &format!("run-as {pkg} sh -c '{seconds_shell}'")],
        ),
        adb_args(&adb, serial, &["logcat", "-c"]),
        adb_args(&adb, serial, &["shell", "am", "start", "-n", &component]),
    ]
}

/// Logcat pulls for one app pid, plus the crash buffer. `pid` of `None` omits the pid-scoped dump
/// (the process never appeared).
#[must_use]
pub fn logcat_commands(adb: &Path, serial: &str, package: &str, pid: Option<&str>) -> Vec<PlannedCommand> {
    let adb_s = adb.display().to_string();
    let mut commands = vec![planned(
        &[
            adb_s.clone(),
            "-s".into(),
            serial.into(),
            "shell".into(),
            "pidof".into(),
            package.into(),
        ],
        None,
        false,
    )];
    if let Some(pid) = pid {
        commands.push(planned(
            &[
                adb_s.clone(),
                "-s".into(),
                serial.into(),
                "logcat".into(),
                "-d".into(),
                "-v".into(),
                "threadtime".into(),
                format!("--pid={pid}"),
            ],
            None,
            false,
        ));
    }
    commands.push(planned(
        &[
            adb_s,
            "-s".into(),
            serial.into(),
            "logcat".into(),
            "-b".into(),
            "crash".into(),
            "-d".into(),
            "-v".into(),
            "threadtime".into(),
        ],
        None,
        false,
    ));
    commands
}

/// `package/activity`, expanding a leading-dot class name.
#[must_use]
pub fn component_name(package: &str, activity: &str) -> String {
    if activity.contains('/') {
        activity.to_string()
    } else if let Some(rest) = activity.strip_prefix('.') {
        format!("{package}/.{rest}")
    } else {
        format!("{package}/{activity}")
    }
}

/// The first pid in `pidof` output.
#[must_use]
pub fn parse_pidof(text: &str) -> Option<String> {
    let token = text.split_whitespace().next()?;
    token
        .chars()
        .all(|ch| ch.is_ascii_digit())
        .then(|| token.to_string())
}

/// Fails on `FATAL EXCEPTION`, `Fatal signal`, a threadtime priority of `E` or `F` (the fifth
/// whitespace token), or a brief-format `E/` / `F/` line.
///
/// # Errors
/// The matching lines.
pub fn scan_logcat(text: &str) -> Result<(), Vec<String>> {
    let mut hits = Vec::new();
    for line in text.lines() {
        if line.contains("FATAL EXCEPTION") || line.contains("Fatal signal") || logcat_is_error(line) {
            hits.push(line.to_string());
        }
    }
    if hits.is_empty() { Ok(()) } else { Err(hits) }
}

/// Fatal lines only (the crash buffer). An ordinary `E` line does not match.
///
/// # Errors
/// The matching lines.
pub fn scan_crash_buffer(text: &str) -> Result<(), Vec<String>> {
    let hits: Vec<String> = text
        .lines()
        .filter(|line| line.contains("FATAL EXCEPTION") || line.contains("Fatal signal"))
        .map(str::to_string)
        .collect();
    if hits.is_empty() { Ok(()) } else { Err(hits) }
}

fn logcat_is_error(line: &str) -> bool {
    if line.starts_with("E/") || line.starts_with("F/") || line.contains(" E/") || line.contains(" F/") {
        return true;
    }
    let mut tokens = line.split_whitespace();
    let priority = tokens.nth(4);
    matches!(priority, Some("E" | "F"))
}

/// Whether `path` can be opened read-write. Used for `/dev/kvm`; tests pass a temp file.
#[must_use]
pub fn kvm_device_usable(path: &Path) -> bool {
    fs::OpenOptions::new().read(true).write(true).open(path).is_ok()
}

/// Command-line options of `cargo xtask package-smoke`.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// Seconds the packaged game runs after startup playback completes, excluding startup warmup.
    #[arg(long, default_value_t = 60)]
    pub seconds: u64,
    /// Directory for packages, extracts and logs. Default: `target/package-smoke`.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// JSON report path. Default: `<out>/package-smoke.json`.
    #[arg(long)]
    pub report: Option<PathBuf>,
    /// Build the sim_bridge APK and AAB and record sizes.
    #[arg(long)]
    pub android: bool,
    /// Boot an x86_64 emulator, install the dev APK and smoke it. Implies `--android`.
    #[arg(long)]
    pub android_emulator: bool,
    /// Print the emulator commands and do not boot.
    #[arg(long)]
    pub dry_run: bool,
    /// Write the committed playback recordings and exit.
    #[arg(long)]
    pub write_recordings: bool,
    /// Skip the Linux tar packages (the Android CI job).
    #[arg(long)]
    pub skip_linux: bool,
    /// Skip generated third-person packages for a focused sim_bridge acceptance run.
    #[arg(long)]
    pub skip_third_person: bool,
    /// Android SDK root. Default: `ANDROID_SDK_ROOT`, `ANDROID_HOME`, then `~/Android/Sdk`.
    #[arg(long)]
    pub sdk: Option<PathBuf>,
    /// Fail when `/dev/kvm` cannot be opened read-write.
    #[arg(long)]
    pub require_kvm: bool,
    /// Windows zip smoke. On Linux this prints the commands and does not build.
    #[arg(long)]
    pub windows: bool,
}

#[derive(Serialize)]
struct Report {
    format_version: u32,
    packages: Vec<PackageReport>,
    apk_limit_bytes: u64,
    size_table: String,
    emulator: EmulatorReport,
    failures: Vec<String>,
}

#[derive(Serialize)]
struct PackageReport {
    name: String,
    profile: String,
    platform: String,
    format: String,
    bytes: u64,
    smoke: String,
    errors: Vec<String>,
}

#[derive(Serialize)]
struct EmulatorReport {
    ran: bool,
    status: String,
    reason: String,
    commands: Vec<String>,
}

/// Runs the package smoke.
///
/// # Errors
/// A failure that prevented the report from being written. A failed smoke is `Ok(FAILURE)` with
/// the reasons in the report and on stderr.
pub fn run(args: &Args, root: &Path) -> Result<ExitCode> {
    if args.write_recordings {
        startup_playback::write_smoke_recordings(root).map_err(|err| anyhow::anyhow!(err))?;
        println!("wrote package-smoke recordings under {}", root.display());
        return Ok(ExitCode::SUCCESS);
    }
    if args.windows {
        #[cfg(windows)]
        {
            return run_windows(args, root);
        }
        #[cfg(not(windows))]
        {
            print_windows_plan(args, root);
            return Ok(ExitCode::SUCCESS);
        }
    }

    let out = args
        .out
        .clone()
        .unwrap_or_else(|| root.join("target").join("package-smoke"));
    fs::create_dir_all(&out)?;
    let out = fs::canonicalize(out)?;
    let report_path = args
        .report
        .clone()
        .unwrap_or_else(|| out.join("package-smoke.json"));
    let mut packages = Vec::new();
    let mut rows = Vec::new();
    let mut failures = Vec::new();

    if args.dry_run {
        let emulator = android_emulator(args, root, &out, None)?;
        return finish_report(&report_path, packages, rows, emulator, failures);
    }

    if !args.skip_linux {
        #[cfg(unix)]
        {
            if let Err(err) = linux_packages(args, root, &out, &mut packages, &mut rows) {
                failures.push(format!("linux packages: {err:#}"));
            }
        }
        #[cfg(not(unix))]
        {
            failures.push(
                "linux package smoke runs on Linux; on Windows use cargo xtask package-smoke --windows"
                    .to_string(),
            );
        }
    }

    let android = args.android || args.android_emulator;
    let mut emulator = EmulatorReport {
        ran: false,
        status: "not_requested".to_string(),
        reason: String::new(),
        commands: Vec::new(),
    };
    let mut debug_apk = None;
    if android {
        match android_packages(args, root, &out, &mut packages, &mut rows) {
            Ok(apk) => debug_apk = apk,
            Err(err) => failures.push(format!("android packages: {err:#}")),
        }
    }
    if args.android_emulator || args.dry_run {
        emulator = match android_emulator(args, root, &out, debug_apk.as_deref()) {
            Ok(report) => report,
            Err(err) => {
                failures.push(format!("android emulator: {err:#}"));
                EmulatorReport {
                    ran: false,
                    status: "fail".to_string(),
                    reason: err.to_string(),
                    commands: Vec::new(),
                }
            }
        };
        if emulator.status == "fail" && !failures.iter().any(|item| item.contains(&emulator.reason)) {
            failures.push(format!("android emulator: {}", emulator.reason));
        }
    }

    finish_report(&report_path, packages, rows, emulator, failures)
}

fn finish_report(
    report_path: &Path,
    packages: Vec<PackageReport>,
    rows: Vec<SizeRow>,
    emulator: EmulatorReport,
    mut failures: Vec<String>,
) -> Result<ExitCode> {
    for row in &rows {
        if apk_over_limit(row) {
            failures.push(format!(
                "{} {} APK is {} bytes, over the {} byte limit",
                row.name, row.profile, row.bytes, APK_LIMIT_BYTES
            ));
        }
    }
    for package in &packages {
        if package.smoke == "fail" {
            failures.push(format!(
                "{} {} {} smoke failed: {}",
                package.name,
                package.profile,
                package.platform,
                package.errors.join("; ")
            ));
        }
    }
    let table = format_size_table(&rows);
    print!("{table}");
    let report = Report {
        format_version: REPORT_FORMAT_VERSION,
        packages,
        apk_limit_bytes: APK_LIMIT_BYTES,
        size_table: table,
        emulator,
        failures: failures.clone(),
    };
    write_report_file(report_path, &report)?;
    println!("report: {}", report_path.display());
    if failures.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        for failure in &failures {
            eprintln!("package-smoke: {failure}");
        }
        Ok(ExitCode::FAILURE)
    }
}

#[cfg(unix)]
fn linux_packages(
    args: &Args,
    root: &Path,
    out: &Path,
    packages: &mut Vec<PackageReport>,
    rows: &mut Vec<SizeRow>,
) -> Result<()> {
    let regia = ensure_regia(root)?;
    let projects = out.join("projects");
    let third = ensure_third_person(&regia, &projects)?;
    let targets = [
        (
            "sim_bridge",
            root.join("samples").join("sim_bridge"),
            root.join("samples")
                .join("sim_bridge")
                .join("smoke")
                .join("playback.frec"),
        ),
        (
            "third_person",
            third,
            root.join("crates")
                .join("regia_templates")
                .join("templates")
                .join("third_person")
                .join("smoke")
                .join("playback.frec"),
        ),
    ];
    for (name, project, playback) in targets {
        if args.skip_third_person && name == "third_person" {
            continue;
        }
        if !playback.is_file() {
            bail!(
                "recording {} is missing; run scripts/write-package-smoke-recordings.sh",
                playback.display()
            );
        }
        let playback = abs_path(&playback);
        for profile in ["dev", "shipping"] {
            match smoke_one_linux(args, &regia, name, &project, profile, &playback, out) {
                Ok((bytes, errors)) => {
                    rows.push(SizeRow {
                        name: name.to_string(),
                        profile: profile.to_string(),
                        platform: "linux".to_string(),
                        format: "tar".to_string(),
                        bytes,
                    });
                    let smoke = if errors.is_empty() { "pass" } else { "fail" };
                    packages.push(PackageReport {
                        name: name.to_string(),
                        profile: profile.to_string(),
                        platform: "linux".to_string(),
                        format: "tar".to_string(),
                        bytes,
                        smoke: smoke.to_string(),
                        errors,
                    });
                }
                Err(err) => {
                    packages.push(PackageReport {
                        name: name.to_string(),
                        profile: profile.to_string(),
                        platform: "linux".to_string(),
                        format: "tar".to_string(),
                        bytes: 0,
                        smoke: "fail".to_string(),
                        errors: vec![format!("{err:#}")],
                    });
                }
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn smoke_one_linux(
    args: &Args,
    regia: &Path,
    name: &str,
    project: &Path,
    profile: &str,
    playback: &Path,
    out: &Path,
) -> Result<(u64, Vec<String>)> {
    let dest = out.join("linux").join(name).join(profile);
    let archive = package_project(
        regia,
        project,
        "linux",
        "tar",
        profile,
        &dest,
        PackageExtras::none(),
    )?;
    let bytes = file_len(&archive)?;
    let extracted = dest.join("extracted");
    if extracted.exists() {
        fs::remove_dir_all(&extracted)?;
    }
    extract_archive(&archive, &extracted)?;
    let binary = find_game_binary(&extracted).with_context(|| format!("{name} {profile}: no executable"))?;
    let json_log = dest.join("session.jsonl");
    let status = launch_game(&binary, args.seconds, playback, &json_log)?;
    let mut errors = check_run_logs(&json_log);
    if status.code() == Some(124) {
        errors.push("process timed out before a clean exit".to_string());
    } else if !status.success() {
        errors.push(format!("process exited with {status}"));
    }
    Ok((bytes, errors))
}

fn android_packages(
    args: &Args,
    root: &Path,
    out: &Path,
    packages: &mut Vec<PackageReport>,
    rows: &mut Vec<SizeRow>,
) -> Result<Option<PathBuf>> {
    let regia = ensure_regia(root)?;
    let project = root.join("samples").join("sim_bridge");
    let sdk = sdk_root(args);
    let mut debug_apk = None;
    for profile in ["dev", "shipping"] {
        let keystore = if profile == "shipping" {
            match throwaway_keystore(&out.join("keystore")) {
                Ok(keystore) => Some(keystore),
                Err(err) => {
                    packages.push(PackageReport {
                        name: "sim_bridge".to_string(),
                        profile: profile.to_string(),
                        platform: "android".to_string(),
                        format: "apk".to_string(),
                        bytes: 0,
                        smoke: "fail".to_string(),
                        errors: vec![format!("{err:#}")],
                    });
                    None
                }
            }
        } else {
            None
        };
        if profile == "shipping" && keystore.is_none() {
            continue;
        }
        for format in ["apk", "aab"] {
            let dest = out.join("android").join(profile).join(format);
            let archive = package_project(
                &regia,
                &project,
                "android",
                format,
                profile,
                &dest,
                PackageExtras {
                    sdk: Some(&sdk),
                    keystore: keystore.as_ref(),
                },
            )
            .with_context(|| format!("sim_bridge {profile} {format}"))?;
            let bytes = file_len(&archive)?;
            rows.push(SizeRow {
                name: "sim_bridge".to_string(),
                profile: profile.to_string(),
                platform: "android".to_string(),
                format: format.to_string(),
                bytes,
            });
            packages.push(PackageReport {
                name: "sim_bridge".to_string(),
                profile: profile.to_string(),
                platform: "android".to_string(),
                format: format.to_string(),
                bytes,
                smoke: "not_run".to_string(),
                errors: Vec::new(),
            });
            if profile == "dev" && format == "apk" {
                debug_apk = Some(archive);
            }
        }
    }
    Ok(debug_apk)
}

struct Keystore {
    path: PathBuf,
    password: String,
    alias: String,
}

fn throwaway_keystore(dir: &Path) -> Result<Keystore> {
    let path = dir.join("smoke.keystore");
    let password = "regiasmoke".to_string();
    let alias = "smoke".to_string();
    if !path.is_file() {
        fs::create_dir_all(dir)?;
        let mut cmd = Command::new("keytool");
        cmd.args(["-genkeypair", "-keystore"])
            .arg(&path)
            .args(["-storepass", &password, "-keypass", &password, "-alias", &alias])
            .args(["-keyalg", "RSA", "-keysize", "2048", "-validity", "10000"])
            .args(["-dname", "CN=Regia Smoke, OU=CI, O=Regia, L=None, ST=None, C=US"]);
        let status = cmd.status().context(
            "keytool not found; a shipping APK needs a throwaway keystore (JDK). Dev APKs use the debug keystore",
        )?;
        if !status.success() {
            bail!("keytool failed to create {}", path.display());
        }
    }
    Ok(Keystore {
        path,
        password,
        alias,
    })
}

fn android_emulator(args: &Args, root: &Path, out: &Path, apk: Option<&Path>) -> Result<EmulatorReport> {
    let sdk = sdk_root(args);
    let playback = root
        .join("samples")
        .join("sim_bridge")
        .join("smoke")
        .join("playback.frec");
    let apk_path = apk.map(Path::to_path_buf).unwrap_or_else(|| {
        out.join("android")
            .join("dev")
            .join("apk")
            .join(format!("{SIM_BRIDGE_PACKAGE}-debug.apk"))
    });
    let request = EmulatorRequest {
        sdk: sdk.clone(),
        api: EMULATOR_API,
        abi: "x86_64".to_string(),
        image_tag: "google_apis".to_string(),
        avd: SMOKE_AVD.to_string(),
        package_id: SIM_BRIDGE_PACKAGE.to_string(),
        activity: SIM_BRIDGE_ACTIVITY.to_string(),
        serial: format!("emulator-{EMULATOR_PORT}"),
        port: EMULATOR_PORT,
        apk: apk_path,
        playback,
        smoke_seconds: args.seconds,
    };
    let commands = emulator_commands(&request);
    let rendered: Vec<String> = commands.iter().map(PlannedCommand::display).collect();
    for line in &rendered {
        println!("{line}");
    }
    let kvm = kvm_device_usable(Path::new("/dev/kvm"));
    if args.dry_run || !kvm {
        if !args.dry_run && !kvm && args.require_kvm {
            bail!("kvm_unavailable: /dev/kvm is not openable read-write and --require-kvm was set");
        }
        let (status, reason) = if args.dry_run {
            ("dry_run", "dry-run; emulator was not started")
        } else {
            (
                "skipped_no_kvm",
                "/dev/kvm is not openable read-write; emulator commands printed and not started",
            )
        };
        println!("package-smoke: {reason}");
        return Ok(EmulatorReport {
            ran: false,
            status: status.to_string(),
            reason: reason.to_string(),
            commands: rendered,
        });
    }
    if !request.apk.is_file() {
        bail!("emulator APK {} is missing", request.apk.display());
    }
    if !request.playback.is_file() {
        bail!("emulator playback {} is missing", request.playback.display());
    }
    let mut background = KillOnDrop(None);
    for command in &commands {
        if command.background {
            background.0 = Some(spawn_background(command)?);
            continue;
        }
        if command.argv.iter().any(|arg| arg == "getprop") {
            poll_boot(&command.argv)?;
            continue;
        }
        if command.argv.iter().any(|arg| arg == "wait-for-device") {
            run_planned_timeout(command, 120)?;
            continue;
        }
        run_planned(command)?;
    }
    let adb = adb_path(&sdk);
    let serial = request.serial.as_str();
    let pid = wait_for_pid(&adb, serial, SIM_BRIDGE_PACKAGE, args.seconds.saturating_add(45))?;
    // The app exits itself from the smoke-seconds file. Poll until pidof is empty or the budget ends.
    wait_until_gone(&adb, serial, SIM_BRIDGE_PACKAGE, args.seconds.saturating_add(45))?;
    let errors = pull_and_scan_logcat(&adb, serial, SIM_BRIDGE_PACKAGE, Some(&pid), out)?;
    if !errors.is_empty() {
        let reason = errors.join("; ");
        return Ok(EmulatorReport {
            ran: true,
            status: "fail".to_string(),
            reason,
            commands: rendered,
        });
    }
    Ok(EmulatorReport {
        ran: true,
        status: "pass".to_string(),
        reason: String::new(),
        commands: rendered,
    })
}

fn pull_and_scan_logcat(
    adb: &Path,
    serial: &str,
    package: &str,
    pid: Option<&str>,
    out: &Path,
) -> Result<Vec<String>> {
    let commands = logcat_commands(adb, serial, package, pid);
    let mut errors = Vec::new();
    for command in commands.iter().skip(1) {
        let text = command_output(command)?;
        let path = if command.argv.iter().any(|arg| arg == "crash") {
            out.join("logcat-crash.txt")
        } else {
            out.join("logcat-app.txt")
        };
        fs::write(&path, &text)?;
        if !command.argv.iter().any(|arg| arg == "crash")
            && let Err(reason) = confirm_android_smoke(&text)
        {
            errors.push(reason);
        }
        let scanned = if command.argv.iter().any(|arg| arg == "crash") {
            scan_crash_buffer(&text)
        } else {
            scan_logcat(&text)
        };
        if let Err(hits) = scanned {
            errors.extend(hits);
        }
    }
    Ok(errors)
}

#[cfg(windows)]
fn run_windows(args: &Args, root: &Path) -> Result<ExitCode> {
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| root.join("target").join("package-smoke"));
    fs::create_dir_all(&out)?;
    let regia = ensure_regia(root)?;
    let project = root.join("samples").join("sim_bridge");
    let playback = abs_path(&project.join("smoke").join("playback.frec"));
    if !playback.is_file() {
        bail!(
            "recording {} is missing; run scripts/write-package-smoke-recordings.sh",
            playback.display()
        );
    }
    let mut failures = Vec::new();
    let mut rows = Vec::new();
    let mut packages = Vec::new();
    for profile in ["dev", "shipping"] {
        let dest = out.join("windows").join(profile);
        let archive = package_project(
            &regia,
            &project,
            "windows",
            "zip",
            profile,
            &dest,
            PackageExtras::none(),
        )?;
        let bytes = file_len(&archive)?;
        rows.push(SizeRow {
            name: "sim_bridge".to_string(),
            profile: profile.to_string(),
            platform: "windows".to_string(),
            format: "zip".to_string(),
            bytes,
        });
        let extracted = dest.join("extracted");
        extract_archive(&archive, &extracted)?;
        let binary = find_game_binary(&extracted)?;
        let json_log = dest.join("session.jsonl");
        let status = launch_game(&binary, args.seconds, &playback, &json_log)?;
        let mut errors = check_run_logs(&json_log);
        if !status.success() {
            errors.push(format!("process exited with {status}"));
        }
        let smoke = if errors.is_empty() { "pass" } else { "fail" };
        if smoke == "fail" {
            failures.push(format!("sim_bridge {profile} windows: {}", errors.join("; ")));
        }
        packages.push(PackageReport {
            name: "sim_bridge".to_string(),
            profile: profile.to_string(),
            platform: "windows".to_string(),
            format: "zip".to_string(),
            bytes,
            smoke: smoke.to_string(),
            errors,
        });
    }
    let table = format_size_table(&rows);
    print!("{table}");
    let report_path = args
        .report
        .clone()
        .unwrap_or_else(|| out.join("package-smoke.json"));
    write_report_file(
        &report_path,
        &Report {
            format_version: REPORT_FORMAT_VERSION,
            packages,
            apk_limit_bytes: APK_LIMIT_BYTES,
            size_table: table,
            emulator: EmulatorReport {
                ran: false,
                status: "not_requested".to_string(),
                reason: String::new(),
                commands: Vec::new(),
            },
            failures: failures.clone(),
        },
    )?;
    if failures.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        for failure in &failures {
            eprintln!("package-smoke: {failure}");
        }
        Ok(ExitCode::FAILURE)
    }
}

#[cfg(not(windows))]
fn print_windows_plan(args: &Args, root: &Path) {
    let project = root.join("samples").join("sim_bridge");
    let playback = project.join("smoke").join("playback.frec");
    println!(
        "Windows zip smoke is not run on this host. On the owner's Windows test machine, from the workspace root:"
    );
    println!();
    println!("cargo xtask package-smoke --windows");
    println!();
    println!("That command packages and runs both profiles. The underlying steps are:");
    for profile in ["dev", "shipping"] {
        println!(
            "regia package --platform windows --format zip --profile {profile} --project {} --out target/package-smoke/windows/{profile}",
            project.display()
        );
    }
    println!();
    println!("Then, for each extracted exe, with the working directory set to the exe's folder:");
    println!("  REGIA_PLAY_INPUT={}", playback.display());
    println!("  REGIA_SMOKE_SECONDS={}", args.seconds);
    println!("  REGIA_JSON_LOG=<package-dir>/session.jsonl");
    println!(
        "A passing run exits 0, the JSON log has no ERROR record, and session.summary.json has exit.clean true and no gpu errors."
    );
}

/// SDK root and signing material for one `regia package` call. Linux and Windows pass `NONE`.
struct PackageExtras<'a> {
    sdk: Option<&'a Path>,
    keystore: Option<&'a Keystore>,
}

impl PackageExtras<'_> {
    fn none() -> Self {
        Self {
            sdk: None,
            keystore: None,
        }
    }
}

fn package_project(
    regia: &Path,
    project: &Path,
    platform: &str,
    format: &str,
    profile: &str,
    out: &Path,
    extras: PackageExtras<'_>,
) -> Result<PathBuf> {
    fs::create_dir_all(out)?;
    let suffix = match format {
        "tar" => "-linux.tar.gz",
        "zip" => ".zip",
        "apk" => ".apk",
        "aab" => ".aab",
        other => bail!("unknown package format {other}"),
    };
    let mut cmd = Command::new(regia);
    cmd.args([
        "package",
        "--platform",
        platform,
        "--format",
        format,
        "--profile",
        profile,
        "--project",
    ])
    .arg(project)
    .arg("--out")
    .arg(out);
    apply_tool_env(&mut cmd);
    if let Some(sdk) = extras.sdk {
        cmd.env("ANDROID_HOME", sdk);
        cmd.env("ANDROID_SDK_ROOT", sdk);
    }
    if let Some(keystore) = extras.keystore {
        cmd.env("REGIA_ANDROID_KEYSTORE", &keystore.path)
            .env("REGIA_ANDROID_KEYSTORE_PASSWORD", &keystore.password)
            .env("REGIA_ANDROID_KEY_ALIAS", &keystore.alias)
            .env("REGIA_ANDROID_KEY_PASSWORD", &keystore.password);
    }
    run_command(&mut cmd).with_context(|| format!("regia package {platform} {format} {profile}"))?;
    newest_ending(out, suffix).with_context(|| format!("no *{suffix} in {}", out.display()))
}

fn apply_tool_env(cmd: &mut Command) {
    if std::env::var_os("ANDROID_NDK_HOME").is_none()
        && let Some(ndk) = std::env::var_os("ANDROID_NDK_LATEST_HOME")
    {
        cmd.env("ANDROID_NDK_HOME", ndk);
    }
}

fn ensure_regia(root: &Path) -> Result<PathBuf> {
    // Cargo checks freshness; an existing binary may embed an older template or packager.
    let mut cmd = Command::new(cargo_bin());
    cmd.args(["build", "--locked", "-p", "regia_cli", "--bin", "regia"])
        .current_dir(root);
    run_command(&mut cmd).context("building the regia binary")?;
    regia_beside_xtask().context("regia binary was not next to xtask after cargo build")
}

fn regia_beside_xtask() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.join(binary_name("regia"));
    path.is_file().then_some(path)
}

fn cargo_bin() -> String {
    match std::env::var("CARGO") {
        Ok(bin) if !bin.is_empty() => bin,
        _ => "cargo".to_string(),
    }
}

#[cfg(unix)]
fn ensure_third_person(regia: &Path, projects: &Path) -> Result<PathBuf> {
    let project = projects.join("ci_third_person");
    // This is a disposable fixture under the smoke output, regenerated from today's template.
    if project.exists() {
        fs::remove_dir_all(&project).with_context(|| format!("removing {}", project.display()))?;
    }
    fs::create_dir_all(projects)?;
    let mut cmd = Command::new(regia);
    cmd.args(["new", "ci_third_person", "--template", "third_person", "--path"])
        .arg(projects);
    run_command(&mut cmd).context("regia new third_person")?;
    if !project.join("Regia.toml").is_file() {
        bail!("regia new did not create {}", project.display());
    }
    Ok(project)
}

fn launch_game(
    binary: &Path,
    seconds: u64,
    playback: &Path,
    json_log: &Path,
) -> Result<std::process::ExitStatus> {
    let workdir = binary
        .parent()
        .context("game binary has no parent directory")?
        .to_path_buf();
    if let Some(parent) = json_log.parent() {
        fs::create_dir_all(parent)?;
    }
    clear_run_logs(json_log)?;
    #[cfg(unix)]
    {
        let limit = seconds.saturating_add(180).to_string();
        let mut cmd = Command::new("timeout");
        cmd.args(["--signal=TERM", &limit, "xvfb-run", "-a"]).arg(binary);
        apply_smoke_env(&mut cmd, &workdir, seconds, playback, json_log);
        cmd.status().context("failed to start timeout/xvfb-run")
    }
    #[cfg(not(unix))]
    {
        let mut cmd = Command::new(binary);
        apply_smoke_env(&mut cmd, &workdir, seconds, playback, json_log);
        cmd.status()
            .with_context(|| format!("failed to start {}", binary.display()))
    }
}

fn apply_smoke_env(cmd: &mut Command, workdir: &Path, seconds: u64, playback: &Path, json_log: &Path) {
    let xdg = json_log.parent().unwrap_or(workdir).join("xdg");
    let _ = fs::create_dir_all(xdg.join("data"));
    let _ = fs::create_dir_all(xdg.join("cache"));
    cmd.current_dir(workdir)
        .env("REGIA_PLAY_INPUT", playback)
        .env("REGIA_SMOKE_SECONDS", seconds.to_string())
        .env("REGIA_JSON_LOG", json_log)
        .env("REGIA_WINIT_ANY_THREAD", "1")
        .env("REGIA_AUDIO", "off")
        .env("LIBGL_ALWAYS_SOFTWARE", "1")
        .env("XDG_DATA_HOME", xdg.join("data"))
        .env("XDG_CACHE_HOME", xdg.join("cache"));
    if let Some(icd) = lavapipe_icd() {
        cmd.env("VK_DRIVER_FILES", &icd);
        cmd.env("VK_ICD_FILENAMES", &icd);
    }
}

fn lavapipe_icd() -> Option<PathBuf> {
    [
        "/usr/share/vulkan/icd.d/lvp_icd.x86_64.json",
        "/usr/share/vulkan/icd.d/lvp_icd.json",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
}

fn clear_run_logs(json_log: &Path) -> Result<()> {
    let paths = regia_telemetry::SummaryPaths::beside_log(json_log);
    for path in [json_log, &paths.final_path, &paths.partial_path] {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("removing stale {}", path.display())),
        }
    }
    Ok(())
}

fn check_run_logs(json_log: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    let text = match fs::read_to_string(json_log) {
        Ok(text) => text,
        Err(err) => {
            problems.push(format!("JSON log {}: {err}", json_log.display()));
            return problems;
        }
    };
    if let Err(msg) = confirm_playback(&text) {
        problems.push(msg);
    }
    if let Err(hits) = scan_jsonl(&text) {
        for hit in hits {
            problems.push(format!("JSON log line {}: {}", hit.line, name_log_hit(&hit)));
        }
    }
    let summary = summary_path_for(json_log);
    match fs::read_to_string(&summary) {
        Ok(text) => {
            if let Err(msg) = check_summary_json(&text) {
                problems.push(msg);
            }
        }
        Err(err) => problems.push(format!("{}: {err}", summary.display())),
    }
    problems
}

fn summary_path_for(json_log: &Path) -> PathBuf {
    regia_telemetry::SummaryPaths::beside_log(json_log).final_path
}

fn extract_archive(archive: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    let mut cmd = Command::new("tar");
    let name = archive.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.ends_with(".zip") {
        cmd.arg("-xf");
    } else {
        cmd.arg("-xzf");
    }
    cmd.arg(archive).arg("-C").arg(dest);
    run_command(&mut cmd).with_context(|| format!("extracting {}", archive.display()))
}

fn find_game_binary(root: &Path) -> Result<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) => bail!("reading {}: {err}", dir.display()),
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if is_game_binary(&path) {
                found.push(path);
            }
        }
    }
    found.sort_by(|a, b| {
        let rank = |path: &Path| {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let dotted = u8::from(name.contains('.'));
            (dotted, path.components().count())
        };
        rank(a).cmp(&rank(b)).then_with(|| a.cmp(b))
    });
    found
        .into_iter()
        .next()
        .with_context(|| format!("no executable under {}", root.display()))
}

fn is_game_binary(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.ends_with(".so")
        || name.ends_with(".sh")
        || name.ends_with(".debug")
        || name.ends_with(".desktop")
    {
        return false;
    }
    #[cfg(windows)]
    {
        name.ends_with(".exe") && fs::metadata(path).is_ok_and(|meta| meta.is_file())
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|meta| meta.is_file() && (meta.permissions().mode() & 0o111) != 0)
    }
}

fn newest_ending(dir: &Path, suffix: &str) -> Option<PathBuf> {
    let mut files = Vec::new();
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_file() && name.ends_with(suffix) {
            files.push(path);
        }
    }
    files
        .into_iter()
        .max_by_key(|path| fs::metadata(path).and_then(|meta| meta.modified()).ok())
}

fn file_len(path: &Path) -> Result<u64> {
    Ok(fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .len())
}

fn sdk_root(args: &Args) -> PathBuf {
    if let Some(sdk) = &args.sdk {
        return sdk.clone();
    }
    for key in ["ANDROID_SDK_ROOT", "ANDROID_HOME"] {
        if let Ok(value) = std::env::var(key) {
            let path = PathBuf::from(&value);
            if !value.is_empty() && path.is_dir() {
                return path;
            }
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Android").join("Sdk")
}

fn abs_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn write_report_file(path: &Path, report: &Report) -> Result<()> {
    let mut text = serde_json::to_string_pretty(report)?;
    text.push('\n');
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    regia_telemetry::write_atomic(path, &text).with_context(|| format!("writing {}", path.display()))
}

fn run_command(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("failed to start {}", command_display(cmd)))?;
    if !status.success() {
        bail!("{} exited with {status}", command_display(cmd));
    }
    Ok(())
}

fn command_display(cmd: &Command) -> String {
    let program = cmd.get_program().to_string_lossy();
    let args: Vec<String> = cmd
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    if args.is_empty() {
        program.into_owned()
    } else {
        format!("{program} {}", args.join(" "))
    }
}

fn run_planned_timeout(command: &PlannedCommand, seconds: u64) -> Result<()> {
    #[cfg(unix)]
    {
        let mut argv = vec![
            "timeout".to_string(),
            "--signal=TERM".to_string(),
            seconds.to_string(),
        ];
        argv.extend(command.argv.iter().cloned());
        let wrapped = PlannedCommand {
            argv,
            stdin: command.stdin.clone(),
            stdin_file: command.stdin_file.clone(),
            background: false,
        };
        run_planned(&wrapped)
    }
    #[cfg(not(unix))]
    {
        let _ = seconds;
        run_planned(command)
    }
}

fn run_planned(command: &PlannedCommand) -> Result<()> {
    let stdin = if let Some(path) = &command.stdin_file {
        Stdio::from(fs::File::open(path).with_context(|| format!("opening {}", path.display()))?)
    } else if command.stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let mut child = Command::new(&command.argv[0])
        .args(&command.argv[1..])
        .stdin(stdin)
        .spawn()
        .with_context(|| format!("failed to start {}", command.display()))?;
    if let Some(text) = &command.stdin
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("{} exited with {status}", command.display());
    }
    Ok(())
}

struct KillOnDrop(Option<std::process::Child>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_background(command: &PlannedCommand) -> Result<std::process::Child> {
    Command::new(&command.argv[0])
        .args(&command.argv[1..])
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start {}", command.display()))
}

fn command_output(command: &PlannedCommand) -> Result<String> {
    let output = Command::new(&command.argv[0])
        .args(&command.argv[1..])
        .output()
        .with_context(|| format!("failed to start {}", command.display()))?;
    if !output.status.success() {
        bail!(
            "{} exited with {}: {}",
            command.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn poll_boot(argv: &[String]) -> Result<()> {
    let start = Instant::now();
    let budget = Duration::from_secs(180);
    loop {
        let output = Command::new(&argv[0]).args(&argv[1..]).output()?;
        if String::from_utf8_lossy(&output.stdout).trim() == "1" {
            return Ok(());
        }
        if start.elapsed() > budget {
            bail!("emulator did not report sys.boot_completed=1 within 180s");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn wait_for_pid(adb: &Path, serial: &str, package: &str, budget_secs: u64) -> Result<String> {
    let start = Instant::now();
    let budget = Duration::from_secs(budget_secs.max(1));
    loop {
        let command = &logcat_commands(adb, serial, package, None)[0];
        if let Ok(text) = command_output(command)
            && let Some(pid) = parse_pidof(&text)
        {
            return Ok(pid);
        }
        if start.elapsed() > budget {
            bail!("{package} did not appear in pidof");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn wait_until_gone(adb: &Path, serial: &str, package: &str, budget_secs: u64) -> Result<()> {
    let start = Instant::now();
    let budget = Duration::from_secs(budget_secs.max(1));
    loop {
        let command = &logcat_commands(adb, serial, package, None)[0];
        match command_output(command) {
            Ok(text) if parse_pidof(&text).is_none() => return Ok(()),
            Err(_) => return Ok(()),
            Ok(_) => {}
        }
        if start.elapsed() > budget {
            bail!("{package} was still running after {budget_secs}s");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn binary_name(stem: &str) -> String {
    #[cfg(windows)]
    {
        format!("{stem}.exe")
    }
    #[cfg(not(windows))]
    {
        stem.to_string()
    }
}

fn planned(argv: &[String], stdin: Option<String>, background: bool) -> PlannedCommand {
    PlannedCommand {
        argv: argv.to_vec(),
        stdin,
        stdin_file: None,
        background,
    }
}

fn adb_args(adb: &Path, serial: &str, rest: &[&str]) -> PlannedCommand {
    let mut argv = vec![adb.display().to_string(), "-s".to_string(), serial.to_string()];
    argv.extend(rest.iter().map(|arg| (*arg).to_string()));
    PlannedCommand {
        argv,
        stdin: None,
        stdin_file: None,
        background: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_record_fails_and_names_the_line() {
        let log = "\
{\"level\":\"INFO\",\"target\":\"regia\",\"msg\":\"ok\"}

not json at all
{\"level\":\"ERROR\",\"target\":\"regia_render\",\"msg\":\"device lost\"}
{\"not-closed\"
";
        let hits = scan_jsonl(log).expect_err("error record");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].line, 4);
        assert_eq!(name_log_hit(&hits[0]), "regia_render: device lost");
        assert_eq!(hits[1].line, 5);
        assert!(hits[1].message.contains("unparseable"));
        assert!(scan_jsonl("{\"level\":\"WARN\",\"target\":\"regia\",\"msg\":\"nope\"}\n").is_ok());
    }

    #[test]
    fn summary_gpu_errors_fail_and_a_clean_exit_passes() {
        let dirty = r#"{"schema":1,"errors":{"gpu_errors":2},"exit":{"clean":true}}"#;
        let err = check_summary_json(dirty).expect_err("gpu errors");
        assert!(err.contains("gpu_errors = 2"), "{err}");
        let clean = r#"{"schema":1,"exit":{"clean":true}}"#;
        assert!(check_summary_json(clean).is_ok(), "{clean}");
        let zero = r#"{"schema":1,"errors":{"gpu_errors":0},"exit":{"clean":true,"panic":false}}"#;
        assert!(check_summary_json(zero).is_ok());
        let no_exit = r#"{"schema":1}"#;
        assert!(
            check_summary_json(no_exit)
                .expect_err("missing exit")
                .contains("no exit")
        );
        let panicked = r#"{"schema":1,"exit":{"clean":true,"panic":true}}"#;
        assert!(
            check_summary_json(panicked)
                .expect_err("panic flag")
                .contains("panic")
        );
        let partial = r#"{"schema":1,"partial":true,"exit":{"clean":true}}"#;
        assert!(
            check_summary_json(partial)
                .expect_err("partial record")
                .contains("partial")
        );
    }

    #[test]
    fn size_table_marks_only_a_shipping_apk_over_the_limit() {
        let rows = vec![
            SizeRow {
                name: "sim_bridge".into(),
                profile: "dev".into(),
                platform: "linux".into(),
                format: "tar".into(),
                bytes: 100,
            },
            SizeRow {
                name: "sim_bridge".into(),
                profile: "shipping".into(),
                platform: "android".into(),
                format: "apk".into(),
                bytes: APK_LIMIT_BYTES + 1,
            },
        ];
        assert!(!apk_over_limit(&rows[0]));
        assert!(apk_over_limit(&rows[1]));
        let dev_apk = SizeRow {
            name: "sim_bridge".into(),
            profile: "dev".into(),
            platform: "android".into(),
            format: "apk".into(),
            bytes: APK_LIMIT_BYTES + 1,
        };
        assert!(!apk_over_limit(&dev_apk));
        let expected = "\
name         profile    platform   format        bytes note
sim_bridge   dev        linux      tar             100 
sim_bridge   shipping   android    apk       150000001 OVER 150 MB
";
        assert_eq!(format_size_table(&rows), expected);
    }

    #[test]
    fn emulator_commands_come_from_the_sdk_layout() {
        let request = EmulatorRequest {
            sdk: PathBuf::from("/sdk"),
            api: 35,
            abi: "x86_64".into(),
            image_tag: "google_apis".into(),
            avd: "regia_smoke".into(),
            package_id: SIM_BRIDGE_PACKAGE.into(),
            activity: SIM_BRIDGE_ACTIVITY.into(),
            serial: "emulator-5554".into(),
            port: 5554,
            apk: PathBuf::from("/out/sim.apk"),
            playback: PathBuf::from("/rec/playback.frec"),
            smoke_seconds: 60,
        };
        let commands = emulator_commands(&request);
        let avd = Path::new("/sdk")
            .join("cmdline-tools")
            .join("latest")
            .join("bin")
            .join(binary_name("avdmanager"));
        let emulator = Path::new("/sdk").join("emulator").join(binary_name("emulator"));
        let adb = adb_path(Path::new("/sdk"));
        assert_eq!(commands[0].argv[0], avd.display().to_string());
        assert_eq!(
            &commands[0].argv[1..],
            [
                "create",
                "avd",
                "-n",
                "regia_smoke",
                "-k",
                "system-images;android-35;google_apis;x86_64",
                "-d",
                "pixel",
                "--force",
            ]
        );
        assert_eq!(commands[0].stdin.as_deref(), Some("no\n"));
        assert!(!commands[0].background);
        assert_eq!(commands[1].argv[0], emulator.display().to_string());
        assert!(commands[1].background);
        assert!(commands[1].argv.iter().any(|arg| arg == "swiftshader_indirect"));
        assert!(commands[1].argv.iter().any(|arg| arg == "-no-window"));
        assert_eq!(
            commands[2].argv,
            vec![
                adb.display().to_string(),
                "-s".into(),
                "emulator-5554".into(),
                "wait-for-device".into(),
            ]
        );
        let apk_arg = PathBuf::from("/out/sim.apk").display().to_string();
        let install = commands
            .iter()
            .find(|cmd| cmd.argv.iter().any(|arg| arg == "install"))
            .expect("install");
        assert!(install.argv.iter().any(|arg| arg == "-r"));
        assert!(
            install.argv.iter().any(|arg| arg == &apk_arg),
            "{:?}",
            install.argv
        );
        let joined = commands
            .iter()
            .map(PlannedCommand::display)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("system-images;android-35;google_apis;x86_64"),
            "{joined}"
        );
        assert!(
            joined.contains("com.regiaengine.samples.sim_bridge/.RegiaActivity"),
            "{joined}"
        );
        assert!(joined.contains("files/regia-play-input.frec"), "{joined}");
        assert!(joined.contains("echo 60 > files/regia-smoke-seconds"), "{joined}");
        let logcat = logcat_commands(&adb, "emulator-5554", SIM_BRIDGE_PACKAGE, Some("1234"));
        assert_eq!(logcat.len(), 3);
        assert!(logcat[1].argv.iter().any(|arg| arg == "--pid=1234"));
        assert!(logcat[2].argv.iter().any(|arg| arg == "crash"));
    }

    #[test]
    fn logcat_error_line_fails_and_an_info_line_does_not() {
        let sample = "\
11-07 12:00:00.000  1234  1234 I sim_bridge: hello
11-07 12:00:01.000  1234  1234 E sim_bridge: boom
";
        let hits = scan_logcat(sample).expect_err("error line");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].contains("boom"), "{hits:?}");
        assert!(scan_logcat("11-07 12:00:00.000  1  1 I Tag: all good\n").is_ok());
        assert!(scan_logcat("FATAL EXCEPTION: main\n").is_err());
        assert!(scan_crash_buffer("11-07 12:00:01.000  1  1 E Tag: boom\n").is_ok());
        assert!(scan_crash_buffer("Fatal signal 11 (SIGSEGV)\n").is_err());
        assert_eq!(parse_pidof("4321 4322\n").as_deref(), Some("4321"));
        assert_eq!(parse_pidof("\n"), None);
    }

    #[test]
    fn kvm_usable_follows_the_open() {
        let dir = regia_test_utils::unique_temp_dir("package-smoke-kvm");
        let path = dir.join("device");
        fs::write(&path, b"x").expect("temp device");
        assert!(kvm_device_usable(&path));
        assert!(!kvm_device_usable(&dir.join("missing")));
    }

    #[test]
    fn playback_confirmation_names_a_skip() {
        assert!(confirm_playback("{\"msg\":\"playing input recording\"}").is_err());
        assert!(confirm_playback(&format!("{PLAYING_MESSAGE}\n{TIMING_STARTED_MESSAGE}")).is_ok());
        assert!(confirm_playback("input recording missing; playback skipped").is_err());
        assert!(
            confirm_playback("nothing")
                .expect_err("missing playback")
                .contains(PLAYING_MESSAGE)
        );
    }

    #[test]
    fn a_rerun_cannot_pass_using_an_earlier_sessions_logs() {
        let dir = regia_test_utils::unique_temp_dir("package-smoke-stale");
        let log = dir.join("session.jsonl");
        let paths = regia_telemetry::SummaryPaths::beside_log(&log);
        fs::write(
            &log,
            format!("{{\"level\":\"INFO\",\"msg\":\"{PLAYING_MESSAGE}\"}}\n{{\"level\":\"INFO\",\"msg\":\"{TIMING_STARTED_MESSAGE}\"}}\n"),
        )
        .expect("old log");
        fs::write(&paths.final_path, r#"{"schema":1,"exit":{"clean":true}}"#).expect("old summary");
        fs::write(&paths.partial_path, "{}").expect("old partial");
        assert!(check_run_logs(&log).is_empty());
        clear_run_logs(&log).expect("remove stale evidence");
        assert!(!check_run_logs(&log).is_empty());
        assert!(!paths.final_path.exists());
        assert!(!paths.partial_path.exists());
        clear_run_logs(&log).expect("already absent");
    }

    #[test]
    fn dry_run_works_without_a_project_or_packaging_tools() {
        let dir = regia_test_utils::unique_temp_dir("package-smoke-dry-run");
        let args = Args {
            seconds: 60,
            out: Some(dir.join("report")),
            report: None,
            android: true,
            android_emulator: true,
            dry_run: true,
            write_recordings: false,
            skip_linux: false,
            skip_third_person: false,
            sdk: Some(dir.join("absent-sdk")),
            require_kvm: true,
            windows: false,
        };
        assert_eq!(run(&args, &dir).expect("dry-run report"), ExitCode::SUCCESS);
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("report/package-smoke.json")).expect("report file"))
                .expect("JSON report");
        assert_eq!(report["emulator"]["status"], "dry_run");
        assert_eq!(report["packages"].as_array().expect("packages").len(), 0);
        let commands = report["emulator"]["commands"].as_array().expect("commands");
        assert!(
            commands
                .iter()
                .filter_map(|command| command.as_str())
                .any(|command| command.contains("com.regiaengine.samples.sim_bridge-debug.apk"))
        );
    }

    #[test]
    fn android_playback_is_streamed_into_the_app_sandbox() {
        let request = EmulatorRequest {
            sdk: "/sdk".into(),
            api: 35,
            abi: "x86_64".into(),
            image_tag: "google_apis".into(),
            avd: "regia_smoke".into(),
            package_id: SIM_BRIDGE_PACKAGE.into(),
            activity: SIM_BRIDGE_ACTIVITY.into(),
            serial: "emulator-5554".into(),
            port: 5554,
            apk: "/out/app.apk".into(),
            playback: "/rec/input.frec".into(),
            smoke_seconds: 60,
        };
        let commands = emulator_commands(&request);
        let upload = commands
            .iter()
            .find(|cmd| cmd.stdin_file.is_some())
            .expect("recording stdin");
        assert_eq!(upload.stdin_file.as_deref(), Some(Path::new("/rec/input.frec")));
        assert!(
            upload
                .argv
                .last()
                .expect("remote shell command")
                .contains("sh -c 'cat > files/regia-play-input.frec'")
        );
        let seconds = commands
            .iter()
            .find(|cmd| cmd.argv.iter().any(|arg| arg.contains("echo 60")))
            .expect("limit command");
        assert!(
            seconds
                .argv
                .last()
                .expect("remote shell command")
                .contains("sh -c 'echo 60 > files/regia-smoke-seconds'")
        );
        assert!(
            !commands
                .iter()
                .any(|cmd| cmd.argv.iter().any(|arg| arg.contains("/data/local/tmp")))
        );
    }

    #[test]
    fn android_smoke_requires_playback_and_a_clean_exit_marker() {
        assert!(confirm_android_smoke("ordinary logcat output").is_err());
        assert!(confirm_android_smoke(PLAYING_MESSAGE).is_err());
        assert!(
            confirm_android_smoke(&format!("{PLAYING_MESSAGE}\nsmoke seconds elapsed; exiting")).is_err()
        );
        assert!(
            confirm_android_smoke(&format!(
                "{PLAYING_MESSAGE}\n{TIMING_STARTED_MESSAGE}\nsmoke seconds elapsed; exiting"
            ))
            .is_ok()
        );
        assert!(
            confirm_android_smoke(&format!("{MISSING_MESSAGE}\nsmoke seconds elapsed; exiting")).is_err()
        );
    }

    #[test]
    fn apk_limit_is_strictly_below_150_decimal_megabytes() {
        let mut row = SizeRow {
            name: "sim_bridge".into(),
            profile: "shipping".into(),
            platform: "android".into(),
            format: "apk".into(),
            bytes: 149_999_999,
        };
        assert!(!apk_over_limit(&row));
        row.bytes = 150_000_000;
        assert!(apk_over_limit(&row));
    }

    #[test]
    fn dry_run_quotes_shell_metacharacters_and_shows_stdin() {
        assert_eq!(
            format_command(&["tool".into(), "system-images;android-35".into()]),
            "tool 'system-images;android-35'"
        );
        assert_eq!(shell_quote("owner's file"), "'owner'\\''s file'");
        let command = PlannedCommand {
            argv: vec!["adb".into()],
            stdin: None,
            stdin_file: Some("/rec/input file.frec".into()),
            background: false,
        };
        assert_eq!(command.display(), "adb < '/rec/input file.frec'");
        assert!(
            planned(&["avdmanager".into()], Some("no\n".into()), false)
                .display()
                .starts_with("printf %s 'no\n' | ")
        );
    }

    #[test]
    fn quoted_command_lines_keep_spaces_together() {
        let rendered = format_command(&[
            "adb".into(),
            "sh".into(),
            "-c".into(),
            "echo 60 > files/regia-smoke-seconds".into(),
        ]);
        assert_eq!(rendered, "adb sh -c 'echo 60 > files/regia-smoke-seconds'");
    }
}
