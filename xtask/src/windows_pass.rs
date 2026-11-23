// SPDX-License-Identifier: MPL-2.0
//! `windows-pass`: the owner's one-command Windows pass (book `user-guide/windows.md`, ADR-296).
//!
//! Nothing in the Windows port has run on Windows hardware since 2026-10-14 (ADR-197, ADR-201,
//! ADR-276), and no Windows box is available to the builders. Run on the owner's box, this does,
//! in order:
//!
//! 1. the toolchain and machine check (Rust host triple, MSVC through `vswhere`, the Vulkan
//!    runtime, every GPU adapter with its backend and driver);
//! 2. the Windows-relevant test subset ([`TEST_STEPS`]: platform, native crash capture, the
//!    deliberate exit, paths and Saved Games, adapter selection), one log per step;
//! 3. per backend (Vulkan through the default adapter pick, then DX12 when a variable can reach
//!    it): a cold and a warm short run on a fresh pipeline cache (the pipeline-compile gap
//!    measurement of the 70b7abf2 note), then a measured run of `vfx_lab --deck-check`
//!    (`--run-secs`, 60 by default) whose engine `summary.json` is read back;
//! 4. `--quits` short windowed runs that must each exit with code 0.
//!
//! Everything lands in `--out` (default `docs/evidence/windows/<UTC date>`): `summary.json`
//! (this module's schema, [`SCHEMA`]), the per-run folders and the test logs. `--read
//! <summary.json>` prints the verdict for the lead and needs no Windows box, so the owner sends
//! the folder back and the lead reads it anywhere. The schema and the verdict logic are unit
//! tested on Linux against fixture JSON.
//!
//! The pass also runs on Linux (the harness itself can be tried there); only the MSVC and
//! Vulkan-runtime checks are Windows-only.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

/// Version of the `summary.json` this pass writes; [`verdict`] refuses any other.
pub const SCHEMA: u64 = 1;

/// The test subset: a label and the `cargo` arguments. Covers the platform seam, native crash
/// capture (the bundle and the real fault in a child process), the deliberate exit, paths and
/// Saved Games, and the adapter pick.
pub const TEST_STEPS: &[(&str, &[&str])] = &[
    ("platform", &["test", "-p", "regia_platform"]),
    (
        "feedback_native_crash_unit",
        &["test", "-p", "regia_framework", "--lib", "feedback"],
    ),
    (
        "feedback_crash",
        &["test", "-p", "regia_framework", "--test", "feedback_crash"],
    ),
    (
        "native_crash_fault",
        &["test", "-p", "xtask", "--test", "native_crash"],
    ),
    (
        "device_lost_exit",
        &["test", "-p", "regia", "--test", "device_lost_exit"],
    ),
    // `--lib`: the gpu_select tests are lib tests; see `the_adapter_pick_step_runs_only_the_render_lib`.
    (
        "adapter_pick",
        &["test", "-p", "regia_render", "--lib", "gpu_select"],
    ),
];

/// Environment variables that choose an adapter; removed for the default-pick run.
const GPU_ENV: [&str; 5] = [
    "REGIA_GPU_INDEX",
    "REGIA_GPU_NAME",
    "REGIA_GPU_PCI",
    "REGIA_GPU_UUID",
    "REGIA_GPU_STRICT",
];

/// Command-line options of `cargo xtask windows-pass`.
#[derive(clap::Args)]
pub struct Args {
    /// Output folder (default `docs/evidence/windows/<UTC date>` in the repository).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Seconds of each measured run (one per backend).
    #[arg(long, default_value_t = 60)]
    run_secs: u32,
    /// How many times the windowed sample is started and quit.
    #[arg(long, default_value_t = 5)]
    quits: u32,
    /// Skip the test subset.
    #[arg(long)]
    skip_tests: bool,
    /// Skip the measured runs, the pipeline-compile probes and the quits.
    #[arg(long)]
    skip_runs: bool,
    /// Print the verdict of an existing `summary.json` and exit (no Windows box needed).
    #[arg(long, value_name = "SUMMARY_JSON")]
    read: Option<PathBuf>,
}

/// Overall result of a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Every check ran and passed.
    Pass,
    /// Nothing failed, but something could not run (see [`Verdict::blocked`]).
    Blocked,
    /// At least one check failed.
    Fail,
}

impl Status {
    /// Upper-case name used in the printed verdict.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Blocked => "BLOCKED",
            Self::Fail => "FAIL",
        }
    }
}

/// What [`verdict`] concluded from a `summary.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    /// Pass, blocked or fail.
    pub status: Status,
    /// Checks that ran and failed.
    pub failures: Vec<String>,
    /// Checks that could not run (and why).
    pub blocked: Vec<String>,
    /// Measurements, not judged (pipeline-compile times, frame percentiles).
    pub info: Vec<String>,
}

impl Verdict {
    /// The text the lead reads.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!("windows-pass verdict: {}\n", self.status.label());
        for (title, lines) in [
            ("failed", &self.failures),
            ("blocked", &self.blocked),
            ("measured", &self.info),
        ] {
            for line in lines {
                out.push_str(&format!("  {title}: {line}\n"));
            }
        }
        out
    }
}

fn str_at<'a>(v: &'a Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer).and_then(Value::as_str)
}

fn num_at(v: &Value, pointer: &str) -> Option<f64> {
    v.pointer(pointer).and_then(Value::as_f64)
}

/// Judges one backend's run entry (`runs.vulkan` / `runs.dx12`) against the backend it must have
/// used. `None` entries are handled by the caller.
fn judge_run(label: &str, expected_backend: &str, run: &Value, out: &mut Verdict) {
    match str_at(run, "/status") {
        Some("ok") => {}
        Some("unreachable") => {
            let why = str_at(run, "/reason").unwrap_or("no adapter of that backend could be selected");
            out.blocked.push(format!("{label} run: {why}"));
            return;
        }
        Some(other) => {
            let detail = str_at(run, "/reason").unwrap_or("");
            out.failures.push(
                format!("{label} run ended as `{other}` {detail}")
                    .trim_end()
                    .to_owned(),
            );
            return;
        }
        None => {
            out.failures.push(format!("{label} run has no status"));
            return;
        }
    }
    match str_at(run, "/summary/shaders/backend") {
        Some(backend) if backend.eq_ignore_ascii_case(expected_backend) => {}
        Some(backend) => out.failures.push(format!(
            "{label} run used backend {backend}, wanted {expected_backend}"
        )),
        None => out
            .failures
            .push(format!("{label} run: summary.json has no shaders.backend")),
    }
    for counter in ["gpu_errors", "device_lost", "surface_errors", "panics"] {
        match num_at(run, &format!("/summary/errors/{counter}")) {
            Some(0.0) => {}
            Some(n) => out.failures.push(format!("{label} run: errors.{counter} = {n}")),
            None => out
                .failures
                .push(format!("{label} run: summary.json has no errors.{counter}")),
        }
    }
    if run.pointer("/summary/exit/clean").and_then(Value::as_bool) != Some(true) {
        out.failures.push(format!("{label} run: exit was not clean"));
    }
    if run.pointer("/summary/exit/watchdog").and_then(Value::as_bool) == Some(true) {
        out.failures.push(format!("{label} run: the exit watchdog fired"));
    }
    if num_at(run, "/summary/session/frames").unwrap_or(0.0) <= 0.0 {
        out.failures.push(format!("{label} run: no frames recorded"));
    }
    if let Some(hidden) = num_at(run, "/deck_check/hidden_frames")
        && hidden > 0.0
    {
        out.failures.push(format!(
            "{label} run: {hidden} frames ran with the window hidden (wake the screen and rerun)"
        ));
    }
    if let (Some(p50), Some(p99)) = (
        num_at(run, "/summary/frames/p50_ms"),
        num_at(run, "/summary/frames/p99_ms"),
    ) {
        out.info
            .push(format!("{label} frames: p50 {p50:.2} ms, p99 {p99:.2} ms"));
    }
}

/// Reads a `summary.json` and decides: failures first, then what could not run, else pass.
///
/// Rules: the toolchain check has no problems; every test step passed; the Vulkan run (default
/// adapter pick) used Vulkan with zero GPU errors, device losses, surface errors and panics, a
/// clean exit and frames; the DX12 run, when a variable could reach it, the same on DX12 (an
/// unreachable DX12 is `blocked`, not failed); every quit exited with code 0. Pipeline-compile
/// times are reported, never judged.
#[must_use]
pub fn verdict(summary: &Value) -> Verdict {
    let mut out = Verdict {
        status: Status::Pass,
        failures: Vec::new(),
        blocked: Vec::new(),
        info: Vec::new(),
    };
    if summary.get("schema").and_then(Value::as_u64) != Some(SCHEMA) {
        out.failures
            .push(format!("not a schema {SCHEMA} windows-pass summary"));
        out.status = Status::Fail;
        return out;
    }

    if let Some(problems) = summary.pointer("/toolchain/problems").and_then(Value::as_array) {
        for problem in problems.iter().filter_map(Value::as_str) {
            out.failures.push(format!("toolchain: {problem}"));
        }
    } else {
        out.failures.push("toolchain check missing".into());
    }

    match summary.get("tests").and_then(Value::as_array) {
        Some(tests) if !tests.is_empty() => {
            for test in tests {
                let name = str_at(test, "/name").unwrap_or("?");
                match str_at(test, "/status") {
                    Some("pass") => {}
                    Some("skipped") => out.blocked.push(match str_at(test, "/skip_reason") {
                        Some(reason) => format!("test step {name} skipped ({reason})"),
                        None => format!("test step {name} skipped"),
                    }),
                    Some(other) => {
                        let code = test
                            .get("exit_code")
                            .map_or("none".to_owned(), ToString::to_string);
                        out.failures
                            .push(format!("test step {name}: {other} (exit {code})"));
                    }
                    None => out.failures.push(format!("test step {name} has no status")),
                }
            }
        }
        _ => out.blocked.push("tests not run".into()),
    }

    for (key, label, backend) in [("vulkan", "Vulkan", "Vulkan"), ("dx12", "DX12", "Dx12")] {
        match summary.pointer(&format!("/runs/{key}")) {
            Some(run) if !run.is_null() => judge_run(label, backend, run, &mut out),
            _ => out.blocked.push(format!("{label} run not recorded")),
        }
        for phase in ["cold", "warm"] {
            let base = format!("/pipeline_compile/{key}/{phase}");
            if let (Some(each), Some(total), Some(n)) = (
                num_at(summary, &format!("{base}/ms_each")),
                num_at(summary, &format!("{base}/ms")),
                num_at(summary, &format!("{base}/pipelines")),
            ) {
                out.info.push(format!(
                    "{label} pipeline compile, {phase}: {n:.0} pipelines, {total:.1} ms, {each:.2} ms each (cache {})",
                    str_at(summary, &format!("{base}/cache")).unwrap_or("unknown")
                ));
            }
        }
    }

    match summary.pointer("/quits") {
        Some(quits) if !quits.is_null() => {
            let expected = num_at(quits, "/expected").unwrap_or(0.0);
            let codes = quits
                .get("exit_codes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if (codes.len() as f64) < expected {
                out.failures
                    .push(format!("quits: {} of {expected} runs recorded", codes.len()));
            }
            for (i, code) in codes.iter().enumerate() {
                if code.as_i64() != Some(0) {
                    out.failures.push(format!("quit {}: exit code {code}", i + 1));
                }
            }
        }
        _ => out.blocked.push("quits not run".into()),
    }

    out.status = if !out.failures.is_empty() {
        Status::Fail
    } else if !out.blocked.is_empty() {
        Status::Blocked
    } else {
        Status::Pass
    };
    out
}

/// Keeps the parts of an engine `summary.json` (spec 28) this pass reports.
#[must_use]
pub fn extract_engine_summary(engine: &Value) -> Value {
    let mut keep = Map::new();
    for key in [
        "regia", "system", "session", "frames", "hitches", "stalls", "memory", "shaders", "errors", "exit",
    ] {
        if let Some(value) = engine.get(key) {
            keep.insert(key.to_owned(), value.clone());
        }
    }
    Value::Object(keep)
}

/// The cold or warm pipeline-compile numbers out of an engine summary's `shaders` object.
#[must_use]
pub fn compile_numbers(engine: &Value) -> Value {
    let shaders = engine.get("shaders").cloned().unwrap_or(Value::Null);
    json!({
        "pipelines": shaders.get("pipelines").cloned().unwrap_or(Value::Null),
        "ms": shaders.get("ms").cloned().unwrap_or(Value::Null),
        "ms_each": shaders.get("ms_each").cloned().unwrap_or(Value::Null),
        "cache": shaders.get("cache").cloned().unwrap_or(Value::Null),
        "driver_cache": shaders.get("driver_cache").cloned().unwrap_or(Value::Null),
        "backend": shaders.get("backend").cloned().unwrap_or(Value::Null),
    })
}

/// UTC calendar date `YYYY-MM-DD` for days since the Unix epoch (Howard Hinnant's
/// `civil_from_days`, public domain; `docs/provenance.md`).
#[must_use]
pub fn civil_date(days_since_epoch: u64) -> String {
    let z = days_since_epoch as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe as i64 + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn today() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    civil_date(secs / 86_400)
}

/// How a child process ended.
struct Ended {
    /// `Some` when it exited by itself; `None` after a timeout kill.
    code: Option<i32>,
    timed_out: bool,
    secs: f64,
}

/// Runs `cmd` to the end or kills it after `timeout`. A hung game must not hang the pass.
fn run_bounded(cmd: &mut Command, timeout: Duration) -> Result<Ended> {
    let started = Instant::now();
    let mut child = cmd.spawn().context("starting a child process")?;
    loop {
        if let Some(status) = child.try_wait().context("waiting for a child process")? {
            return Ok(Ended {
                code: status.code(),
                timed_out: false,
                secs: started.elapsed().as_secs_f64(),
            });
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Ended {
                code: None,
                timed_out: true,
                secs: started.elapsed().as_secs_f64(),
            });
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn cargo() -> std::ffi::OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

fn step(name: &str) {
    println!("== windows-pass: {name}");
}

fn capture(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Step 1: toolchain and machine. Never fails; problems are listed for the verdict.
fn toolchain_section() -> (Value, Vec<regia_render::AdapterSummary>) {
    let on_windows = cfg!(target_os = "windows");
    let mut problems: Vec<String> = Vec::new();

    let rustc = capture("rustc", &["-vV"]).unwrap_or_default();
    let host = rustc
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned);
    let release = rustc
        .lines()
        .find_map(|line| line.strip_prefix("release: "))
        .map(str::to_owned);
    match (&host, on_windows) {
        (None, _) => problems.push("rustc -vV failed".into()),
        (Some(h), true) if h != "x86_64-pc-windows-msvc" => {
            problems.push(format!("rustc host is {h}, wanted x86_64-pc-windows-msvc"));
        }
        _ => {}
    }
    let rustup_targets = capture("rustup", &["target", "list", "--installed"]);

    let msvc = if on_windows {
        let vswhere = std::env::var_os("ProgramFiles(x86)")
            .map(|root| {
                PathBuf::from(root)
                    .join("Microsoft Visual Studio")
                    .join("Installer")
                    .join("vswhere.exe")
            })
            .filter(|path| path.exists());
        let path = vswhere.and_then(|exe| {
            capture(
                &exe.to_string_lossy(),
                &[
                    "-latest",
                    "-products",
                    "*",
                    "-requires",
                    "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                    "-property",
                    "installationPath",
                ],
            )
        });
        if path.as_deref().is_none_or(str::is_empty) {
            problems.push("MSVC build tools (VC.Tools.x86.x64) not found through vswhere".into());
        }
        json!({ "installation": path })
    } else {
        Value::Null
    };

    let vulkan_runtime = if on_windows {
        let dll = std::env::var_os("SystemRoot")
            .map(|root| PathBuf::from(root).join("System32").join("vulkan-1.dll"));
        let found = dll.as_ref().is_some_and(|path| path.exists());
        if !found {
            problems.push("vulkan-1.dll not found in System32 (install a GPU driver with Vulkan)".into());
        }
        json!({ "vulkan_1_dll": found })
    } else {
        Value::Null
    };

    let adapters = regia_render::list_adapters();
    if adapters.is_empty() {
        problems.push("no GPU adapter listed".into());
    }
    let adapter_json: Vec<Value> = adapters
        .iter()
        .map(|a| {
            json!({
                "index": a.index,
                "name": a.name,
                "backend": format!("{:?}", a.backend),
                "device_type": format!("{:?}", a.device_type),
                "driver": a.driver,
                "driver_info": a.driver_info,
                "pci_bus": a.pci_bus,
                "resource_binding_tier": a.resource_binding_tier.map(|t| t.to_string()),
            })
        })
        .collect();

    (
        json!({
            "rustc_host": host,
            "rustc_release": release,
            "rustup_targets": rustup_targets,
            "msvc": msvc,
            "vulkan_runtime": vulkan_runtime,
            "adapters": adapter_json,
            "problems": problems,
        }),
        adapters,
    )
}

/// How long a test step may compile (`cargo test --no-run`). A cold build of a step with its own
/// target directory took over 30 minutes on the a test machine (ADR-4050), so the build has its own,
/// longer bound; [`TEST_RUN_TIMEOUT`] still stops a hung test.
const TEST_BUILD_TIMEOUT: Duration = Duration::from_secs(3 * 60 * 60);

/// How long a built test step may run.
const TEST_RUN_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The step's arguments with `--no-run`: compile the same test targets without launching them.
fn build_only_args<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut out = args.to_vec();
    out.push("--no-run");
    out
}

/// A step's status from its build and, when the build passed, its run. A build that fails or
/// times out is reported as such (`build_fail`, `build_timeout`), never as a test result.
fn step_status(build: &Ended, run: Option<&Ended>) -> &'static str {
    match (build.timed_out, build.code) {
        (true, _) => return "build_timeout",
        (false, Some(0)) => {}
        _ => return "build_fail",
    }
    match run {
        None => "build_fail",
        Some(run) => match (run.timed_out, run.code) {
            (false, Some(0)) => "pass",
            (true, _) => "timeout",
            _ => "fail",
        },
    }
}

/// What a test program prints, at the start of a line after its own name, when it exited 0 on
/// this host without testing anything: `native_crash: skipped (native crash bundles are Linux
/// only)`, `device_lost_exit: skipped: no GPU device (...)` (ADR-5806).
const SKIP_MARK: &str = ": skipped";

/// The first skip line in a step's log: a line `<program>: skipped...` whose `<program>` is one
/// word (ASCII letters, digits, `_`, `[` and `]`, as in `device_lost_exit[render]`). `None` when
/// the step tested something.
fn skip_line(log: &str) -> Option<&str> {
    log.lines().map(str::trim).find(|line| {
        line.split_once(SKIP_MARK).is_some_and(|(program, _)| {
            !program.is_empty()
                && program
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '[' | ']'))
        })
    })
}

/// A step's status and, for a skip, the program's skip line. A step that passed but whose log
/// says it skipped is `skipped` (the verdict lists it as blocked), never `pass` (ADR-5806).
fn judge_step(build: &Ended, run: Option<&Ended>, log: &str) -> (&'static str, Option<String>) {
    match step_status(build, run) {
        "pass" => match skip_line(log) {
            Some(line) => ("skipped", Some(line.to_owned())),
            None => ("pass", None),
        },
        other => (other, None),
    }
}

/// Starts `cargo <args>` for a test step, its output appended to `log_path`.
fn step_command(root: &Path, args: &[&str], log_path: &Path) -> Result<Command> {
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let log_err = log.try_clone().context("cloning the log handle")?;
    let mut cmd = Command::new(cargo());
    cmd.current_dir(root)
        .args(args)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    if let Some(dir) = isolated_target_dir(args, &cargo_target_dir(root)) {
        cmd.env("CARGO_TARGET_DIR", dir);
    }
    Ok(cmd)
}

/// Step 2: the test subset, one log per step. Each step is built first (`--no-run`, bounded by
/// [`TEST_BUILD_TIMEOUT`]) and then run (bounded by [`TEST_RUN_TIMEOUT`]), both into one log.
fn tests_section(root: &Path, out: &Path) -> Result<Vec<Value>> {
    let logs = out.join("tests");
    fs::create_dir_all(&logs).with_context(|| format!("creating {}", logs.display()))?;
    let mut results = Vec::new();
    for (name, args) in TEST_STEPS {
        step(&format!("test {name}"));
        let log_path = logs.join(format!("{name}.log"));
        fs::File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
        let built = run_bounded(
            &mut step_command(root, &build_only_args(args), &log_path)?,
            TEST_BUILD_TIMEOUT,
        )?;
        let ran = if !built.timed_out && built.code == Some(0) {
            Some(run_bounded(
                &mut step_command(root, args, &log_path)?,
                TEST_RUN_TIMEOUT,
            )?)
        } else {
            None
        };
        let log = fs::read(&log_path).with_context(|| format!("reading {}", log_path.display()))?;
        let (status, skip) = judge_step(&built, ran.as_ref(), &String::from_utf8_lossy(&log));
        let run_secs = ran.as_ref().map_or(0.0, |r| r.secs);
        println!(
            "   {status} (build {:.0} s, run {run_secs:.0} s), log {}",
            built.secs,
            log_path.display()
        );
        results.push(json!({
            "name": name,
            "args": args,
            "status": status,
            "skip_reason": skip,
            "exit_code": ran.as_ref().map_or(built.code, |r| r.code),
            "build_secs": (built.secs * 10.0).round() / 10.0,
            "secs": (run_secs * 10.0).round() / 10.0,
            "log": format!("tests/{name}.log"),
        }));
    }
    Ok(results)
}

/// The cargo target directory this process builds into.
fn cargo_target_dir(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from)
}

/// A step that builds the `xtask` package rebuilds `xtask.exe`, the program running this pass.
/// Windows refuses to replace a running executable (`failed to remove file ... xtask.exe:
/// Access is denied`), so such a step gets its own target directory under `target`.
fn isolated_target_dir(args: &[&str], target: &Path) -> Option<PathBuf> {
    let builds_xtask = args.windows(2).any(|w| w[0] == "-p" && w[1] == "xtask");
    builds_xtask.then(|| target.join("xtask-selftest"))
}

/// Where `cargo build --release -p vfx_lab` leaves the binary.
fn vfx_lab_exe(root: &Path) -> PathBuf {
    let target = cargo_target_dir(root);
    target
        .join("release")
        .join(format!("vfx_lab{}", std::env::consts::EXE_SUFFIX))
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// One `vfx_lab --deck-check` launch. Returns the run entry: status, exit code, the kept parts
/// of the engine `summary.json` and the sample's `deck_check.json`.
fn deck_run(
    exe: &Path,
    dir: &Path,
    secs: f32,
    cache_dir: &Path,
    gpu_index: Option<usize>,
    dxc_dir: Option<&Path>,
) -> Result<Value> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // A second pass on the same day reuses the directory: files from the earlier run must not
    // stand in for this run's (an exit 0 with no summary.json is a failure).
    for stale in ["summary.json", "deck_check.json"] {
        let _ = fs::remove_file(dir.join(stale));
    }
    let mut cmd = Command::new(exe);
    cmd.args(["--deck-check", "--minutes"])
        .arg(format!("{:.4}", secs / 60.0))
        .args(["--cap", "60", "--out"])
        .arg(dir)
        .env("REGIA_PIPELINE_CACHE_DIR", cache_dir)
        // Native crash capture is opt-in on Windows (ADR-197): the pass exercises it.
        .env("REGIA_NATIVE_CRASH", "1")
        .stdin(Stdio::null());
    for var in GPU_ENV {
        cmd.env_remove(var);
    }
    if let Some(index) = gpu_index {
        cmd.env("REGIA_GPU_INDEX", index.to_string());
    }
    if let Some(dir) = dxc_dir {
        cmd.env("PATH", path_with_first(dir, std::env::var_os("PATH")));
    }
    let ended = run_bounded(&mut cmd, Duration::from_secs_f32(secs + 180.0))?;
    let engine = read_json(&dir.join("summary.json"));
    let deck = read_json(&dir.join("deck_check.json"));
    let (status, reason) = match (ended.timed_out, ended.code, &engine) {
        (true, _, _) => ("timeout", "killed: the run did not end in time"),
        (false, Some(0), Some(_)) => ("ok", ""),
        (false, Some(0), None) => ("failed", "exit 0 but no summary.json was written"),
        (false, _, _) => ("failed", "non-zero exit code"),
    };
    Ok(json!({
        "status": status,
        "reason": reason,
        "exit_code": ended.code,
        "secs": (ended.secs * 10.0).round() / 10.0,
        "summary": engine.as_ref().map(extract_engine_summary),
        "deck_check": deck,
    }))
}

/// `dir` first, then the entries of `path` (the `PATH` value for a child process).
fn path_with_first(dir: &Path, path: Option<std::ffi::OsString>) -> std::ffi::OsString {
    let mut dirs = vec![dir.to_path_buf()];
    if let Some(path) = path {
        dirs.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

/// A folder holding both `dxcompiler.dll` and `dxil.dll` (DXC needs both): the first `PATH`
/// entry that does, else the newest Windows SDK redistributable folder (`Windows Kits/10/Redist/D3D/x64`).
fn find_dxc_dir() -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    for root in [
        "C:/Program Files (x86)/Windows Kits/10",
        "C:/Program Files/Windows Kits/10",
    ] {
        dirs.push(Path::new(root).join("Redist/D3D/x64"));
    }
    first_dxc_dir(&dirs)
}

/// The first of `dirs` that holds `dxcompiler.dll` and `dxil.dll`.
fn first_dxc_dir(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .find(|d| d.join("dxcompiler.dll").is_file() && d.join("dxil.dll").is_file())
        .cloned()
}

/// Step 3 for one backend: cold probe, warm probe, measured run.
fn backend_section(
    label: &str,
    exe: &Path,
    out: &Path,
    run_secs: u32,
    gpu_index: Option<usize>,
    dxc_dir: Option<&Path>,
) -> Result<(Value, Value)> {
    let base = out.join(label);
    let cache = base.join("pipeline-cache");
    if cache.exists() {
        fs::remove_dir_all(&cache).with_context(|| format!("clearing {}", cache.display()))?;
    }
    fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;

    step(&format!("{label}: cold pipeline compile (fresh pipeline cache)"));
    let cold = deck_run(exe, &base.join("cold"), 3.0, &cache, gpu_index, dxc_dir)?;
    step(&format!("{label}: warm pipeline compile (same cache)"));
    let warm = deck_run(exe, &base.join("warm"), 3.0, &cache, gpu_index, dxc_dir)?;
    step(&format!("{label}: measured run, {run_secs} s"));
    let measured = deck_run(
        exe,
        &base.join("measured"),
        run_secs as f32,
        &cache,
        gpu_index,
        dxc_dir,
    )?;

    let numbers = |run: &Value| compile_numbers(run.get("summary").unwrap_or(&Value::Null));
    let compile = json!({
        "cold": numbers(&cold),
        "warm": numbers(&warm),
        "note": "measurement only (70b7abf2); the OS driver shader cache is not cleared, so `cold` is a cold engine cache on a warm driver cache",
    });
    Ok((measured, compile))
}

/// Step 4: start and quit the windowed sample `count` times.
fn quits_section(exe: &Path, count: u32) -> Result<Value> {
    let mut codes = Vec::new();
    let mut secs = Vec::new();
    for i in 1..=count {
        step(&format!("quit {i} of {count}"));
        let mut cmd = Command::new(exe);
        cmd.args(["--window", "--exit-after-frames", "120"])
            .stdin(Stdio::null());
        for var in GPU_ENV {
            cmd.env_remove(var);
        }
        let ended = run_bounded(&mut cmd, Duration::from_secs(120))?;
        codes.push(match ended.code {
            Some(code) if !ended.timed_out => json!(code),
            _ => Value::Null,
        });
        secs.push((ended.secs * 10.0).round() / 10.0);
    }
    Ok(json!({ "expected": count, "exit_codes": codes, "secs": secs }))
}

/// Runs the pass, or with `--read` prints a verdict.
pub fn run(args: &Args, root: &Path) -> Result<ExitCode> {
    if let Some(path) = &args.read {
        let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let summary: Value =
            serde_json::from_str(&text).with_context(|| format!("{} is not JSON", path.display()))?;
        let verdict = verdict(&summary);
        print!("{}", verdict.render());
        return Ok(exit_code(verdict.status));
    }

    let out = args
        .out
        .clone()
        .unwrap_or_else(|| root.join("docs/evidence/windows").join(today()));
    fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    println!("windows-pass: results in {}", out.display());

    step("toolchain and machine");
    let (toolchain, adapters) = toolchain_section();
    let mut summary = json!({
        "schema": SCHEMA,
        "pass": "windows-pass",
        "date": today(),
        "regia": { "version": env!("CARGO_PKG_VERSION") },
        "host": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "threads": std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
        },
        "toolchain": toolchain,
        "tests": [],
        "runs": {},
        "pipeline_compile": {},
        "quits": null,
    });

    if !args.skip_tests {
        summary["tests"] = Value::Array(tests_section(root, &out)?);
        write_summary(&out, &summary)?;
    }

    if !args.skip_runs {
        step("build vfx_lab (release)");
        let built = Command::new(cargo())
            .current_dir(root)
            .args(["build", "--release", "-p", "vfx_lab"])
            .status()
            .context("running cargo build")?;
        if !built.success() {
            bail!("cargo build --release -p vfx_lab failed; fix that first");
        }
        let exe = vfx_lab_exe(root);
        if !exe.exists() {
            bail!("{} was not built", exe.display());
        }

        // Vulkan through the default pick (no REGIA_GPU_* variable: ADR-201's new code path).
        let (vulkan, vulkan_compile) = backend_section("vulkan", &exe, &out, args.run_secs, None, None)?;
        summary["runs"]["vulkan"] = vulkan;
        summary["pipeline_compile"]["vulkan"] = vulkan_compile;
        write_summary(&out, &summary)?;

        // DX12 only through a variable. `list_adapters` lists every adapter, Vulkan first then
        // DX12 (gpu_select::enumerate, ADR-326), so DX12 is unreachable here only on a box with
        // no DX12 hardware adapter. Recorded as `unreachable`, not as a failure (ADR-296).
        match adapters
            .iter()
            .find(|a| a.backend == wgpu::Backend::Dx12 && a.device_type != wgpu::DeviceType::Cpu)
        {
            Some(adapter) => {
                // wgpu picks DXC when `dxcompiler.dll` and `dxil.dll` are reachable and FXC
                // otherwise; FXC overflows its stack on the forward shaders (ADR-1583), so the
                // run puts a DXC it can find on PATH and records which compiler it used.
                let dxc = find_dxc_dir();
                summary["dx12_compiler"] = match &dxc {
                    Some(dir) => json!({ "kind": "dxc", "dir": dir.display().to_string() }),
                    None => {
                        json!({ "kind": "fxc", "note": "no dxcompiler.dll with dxil.dll on PATH or in the Windows SDK redist folder; FXC overflows its stack on lab_forward (ADR-1583)" })
                    }
                };
                let (dx12, dx12_compile) = backend_section(
                    "dx12",
                    &exe,
                    &out,
                    args.run_secs,
                    Some(adapter.index),
                    dxc.as_deref(),
                )?;
                summary["runs"]["dx12"] = dx12;
                summary["pipeline_compile"]["dx12"] = dx12_compile;
            }
            None => {
                summary["runs"]["dx12"] = json!({
                    "status": "unreachable",
                    "reason": "list_adapters shows no DX12 hardware adapter on this machine (only WARP or none), so REGIA_GPU_INDEX/NAME cannot force a DX12 run (ADR-296, ADR-326)",
                });
            }
        }
        write_summary(&out, &summary)?;

        summary["quits"] = quits_section(&exe, args.quits)?;
        write_summary(&out, &summary)?;
    }

    let verdict = verdict(&summary);
    summary["verdict"] = json!({
        "status": verdict.status.label(),
        "failures": verdict.failures,
        "blocked": verdict.blocked,
    });
    write_summary(&out, &summary)?;
    print!("\n{}", verdict.render());
    println!("\nsend back the folder {}", out.display());
    Ok(exit_code(verdict.status))
}

fn exit_code(status: Status) -> ExitCode {
    match status {
        Status::Pass => ExitCode::SUCCESS,
        Status::Fail => ExitCode::FAILURE,
        Status::Blocked => ExitCode::from(2),
    }
}

/// Writes `summary.json` atomically (temp file, then rename), after every section, so an
/// interrupted pass still leaves what it measured.
fn write_summary(out: &Path, summary: &Value) -> Result<()> {
    let tmp = out.join("summary.json.tmp");
    let text = serde_json::to_string_pretty(summary).context("serializing summary.json")?;
    fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, out.join("summary.json")).context("replacing summary.json")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A summary as a healthy Windows box would produce it.
    fn healthy() -> Value {
        let run = |backend: &str| {
            json!({
                "status": "ok",
                "reason": "",
                "exit_code": 0,
                "summary": {
                    "session": { "seconds": 60.1, "frames": 3600 },
                    "frames": { "p50_ms": 16.7, "p99_ms": 20.1 },
                    "shaders": { "backend": backend, "pipelines": 91, "ms": 28.5, "ms_each": 0.31, "cache": "hit" },
                    "errors": { "gpu_errors": 0, "device_lost": 0, "surface_errors": 0, "panics": 0 },
                    "exit": { "clean": true, "ms": 50.5, "watchdog": false }
                },
                "deck_check": { "hidden_frames": 0 }
            })
        };
        json!({
            "schema": 1,
            "toolchain": { "problems": [] },
            "tests": [
                { "name": "platform", "status": "pass", "exit_code": 0 },
                { "name": "device_lost_exit", "status": "pass", "exit_code": 0 }
            ],
            "runs": { "vulkan": run("Vulkan"), "dx12": run("Dx12") },
            "pipeline_compile": {
                "vulkan": {
                    "cold": { "pipelines": 91, "ms": 900.0, "ms_each": 9.9, "cache": "miss" },
                    "warm": { "pipelines": 91, "ms": 30.0, "ms_each": 0.33, "cache": "hit" }
                }
            },
            "quits": { "expected": 5, "exit_codes": [0, 0, 0, 0, 0] }
        })
    }

    #[test]
    fn a_healthy_summary_passes_and_reports_compile_times() {
        let v = verdict(&healthy());
        assert_eq!(v.status, Status::Pass, "{}", v.render());
        assert!(v.failures.is_empty() && v.blocked.is_empty());
        assert!(
            v.info
                .iter()
                .any(|l| l.contains("Vulkan pipeline compile, cold: 91 pipelines")),
            "{:?}",
            v.info
        );
        assert!(v.info.iter().any(|l| l.contains("Vulkan frames: p50 16.70 ms")));
    }

    #[test]
    fn dxc_needs_both_dlls_and_the_first_folder_wins() {
        let root = regia_test_utils::unique_temp_dir("windows_pass_dxc");
        let (only_one, both, other) = (root.join("a"), root.join("b"), root.join("c"));
        for dir in [&only_one, &both, &other] {
            fs::create_dir_all(dir).expect("temp dir");
        }
        fs::write(only_one.join("dxcompiler.dll"), b"").expect("write dll");
        for dir in [&both, &other] {
            fs::write(dir.join("dxcompiler.dll"), b"").expect("write dll");
            fs::write(dir.join("dxil.dll"), b"").expect("write dll");
        }
        let dirs = vec![root.join("missing"), only_one, both.clone(), other];
        assert_eq!(first_dxc_dir(&dirs), Some(both.clone()));
        assert_eq!(first_dxc_dir(&dirs[..2]), None);
        let path = path_with_first(
            &both,
            Some(std::env::join_paths([root.join("z")]).expect("join paths")),
        );
        let parts: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(parts, vec![both, root.join("z")]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn an_unreachable_dx12_is_blocked_not_failed() {
        let mut s = healthy();
        s["runs"]["dx12"] = json!({ "status": "unreachable", "reason": "no way to force it" });
        let v = verdict(&s);
        assert_eq!(v.status, Status::Blocked);
        assert!(
            v.blocked
                .iter()
                .any(|l| l.contains("DX12 run: no way to force it"))
        );
        assert!(v.failures.is_empty());
    }

    #[test]
    fn a_wrong_backend_a_gpu_error_and_a_dirty_exit_each_fail() {
        let mut s = healthy();
        s["runs"]["vulkan"]["summary"]["shaders"]["backend"] = json!("Dx12");
        s["runs"]["dx12"]["summary"]["errors"]["device_lost"] = json!(1);
        s["runs"]["dx12"]["summary"]["exit"]["clean"] = json!(false);
        let v = verdict(&s);
        assert_eq!(v.status, Status::Fail);
        assert!(
            v.failures
                .iter()
                .any(|l| l.contains("Vulkan run used backend Dx12"))
        );
        assert!(v.failures.iter().any(|l| l.contains("errors.device_lost = 1")));
        assert!(
            v.failures
                .iter()
                .any(|l| l.contains("DX12 run: exit was not clean"))
        );
    }

    #[test]
    fn a_failed_test_step_and_a_bad_quit_fail() {
        let mut s = healthy();
        s["tests"][1] = json!({ "name": "device_lost_exit", "status": "fail", "exit_code": 101 });
        s["quits"]["exit_codes"] = json!([0, 0, null, 3, 0]);
        let v = verdict(&s);
        assert_eq!(v.status, Status::Fail);
        assert!(
            v.failures
                .iter()
                .any(|l| l == "test step device_lost_exit: fail (exit 101)")
        );
        assert!(v.failures.iter().any(|l| l == "quit 3: exit code null"));
        assert!(v.failures.iter().any(|l| l == "quit 4: exit code 3"));
    }

    #[test]
    fn too_few_quits_toolchain_problems_and_a_timeout_fail() {
        let mut s = healthy();
        s["quits"]["exit_codes"] = json!([0, 0]);
        s["toolchain"]["problems"] = json!(["vulkan-1.dll not found in System32"]);
        s["runs"]["vulkan"]["status"] = json!("timeout");
        s["runs"]["vulkan"]["reason"] = json!("killed");
        let v = verdict(&s);
        assert_eq!(v.status, Status::Fail);
        assert!(v.failures.iter().any(|l| l == "quits: 2 of 5 runs recorded"));
        assert!(
            v.failures
                .iter()
                .any(|l| l == "toolchain: vulkan-1.dll not found in System32")
        );
        assert!(
            v.failures
                .iter()
                .any(|l| l.starts_with("Vulkan run ended as `timeout`"))
        );
    }

    #[test]
    fn skipped_parts_are_blocked_and_a_foreign_file_is_refused() {
        let skipped = json!({
            "schema": 1,
            "toolchain": { "problems": [] },
            "tests": [],
            "runs": {},
            "quits": null
        });
        let v = verdict(&skipped);
        assert_eq!(v.status, Status::Blocked);
        assert!(v.blocked.iter().any(|l| l == "tests not run"));
        assert!(v.blocked.iter().any(|l| l == "Vulkan run not recorded"));
        assert!(v.blocked.iter().any(|l| l == "quits not run"));

        let foreign = verdict(&json!({ "schema": 2 }));
        assert_eq!(foreign.status, Status::Fail);
        assert!(foreign.render().contains("FAIL"));
    }

    #[test]
    fn engine_summary_extraction_keeps_only_the_reported_keys() {
        let engine = json!({
            "schema": 1,
            "host": { "app": "vfx_lab" },
            "frames": { "p50_ms": 25.0 },
            "shaders": { "backend": "Vulkan", "pipelines": 91, "ms": 28.56, "ms_each": 0.32, "cache": "hit", "threads": 2 },
            "errors": { "gpu_errors": 0 }
        });
        let kept = extract_engine_summary(&engine);
        assert!(kept.get("host").is_none() && kept.get("schema").is_none());
        assert_eq!(kept["frames"]["p50_ms"], json!(25.0));
        let numbers = compile_numbers(&kept);
        assert_eq!(numbers["pipelines"], json!(91));
        assert_eq!(numbers["cache"], json!("hit"));
        assert_eq!(numbers["driver_cache"], Value::Null);
        assert_eq!(compile_numbers(&Value::Null)["ms"], Value::Null);
    }

    #[test]
    fn civil_date_matches_known_days() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(20_738), "2026-10-12");
        assert_eq!(civil_date(11_016), "2000-02-29");
    }

    #[test]
    fn only_the_xtask_step_gets_its_own_target_dir() {
        let base = Path::new("target");
        let dirs: Vec<_> = TEST_STEPS
            .iter()
            .filter_map(|(name, args)| isolated_target_dir(args, base).map(|d| (*name, d)))
            .collect();
        assert_eq!(dirs, vec![("native_crash_fault", base.join("xtask-selftest"))]);
    }

    /// ADR-4050: each step is compiled first with the same arguments plus `--no-run`; no step
    /// passes `--` (which would hand `--no-run` to the test binary instead of cargo).
    #[test]
    fn every_step_is_built_with_its_own_arguments_before_it_runs() {
        for (name, args) in TEST_STEPS {
            assert!(!args.contains(&"--"), "{name} passes `--`");
            let built = build_only_args(args);
            assert_eq!(&built[..args.len()], *args, "{name}");
            assert_eq!(built[args.len()..], ["--no-run"], "{name}");
        }
    }

    /// ADR-4050: a slow or failed build is reported as a build problem, and only a build that
    /// passed lets the run's own outcome (and its 30-minute guard) decide the status.
    #[test]
    fn a_step_status_separates_the_build_from_the_run() {
        let ended = |code: Option<i32>, timed_out: bool| Ended {
            code,
            timed_out,
            secs: 1.0,
        };
        let built = ended(Some(0), false);
        assert_eq!(step_status(&ended(None, true), None), "build_timeout");
        assert_eq!(step_status(&ended(Some(101), false), None), "build_fail");
        assert_eq!(step_status(&built, None), "build_fail");
        assert_eq!(step_status(&built, Some(&ended(Some(0), false))), "pass");
        assert_eq!(step_status(&built, Some(&ended(Some(101), false))), "fail");
        assert_eq!(step_status(&built, Some(&ended(None, true))), "timeout");
    }

    /// ADR-5806: the 2026-11-10 N150 pass recorded `native_crash_fault` as `pass`, yet the step's
    /// own log (committed evidence) says it tested nothing there: the driver is Linux only. A
    /// passed run whose log holds a skip line is `skipped`; the other real logs of that pass,
    /// a failure and words that merely contain "skipped" are unchanged. The logs are byte copies of
    /// `docs/evidence/windows/2026-11-10/tests/` in `xtask/tests/fixtures/`: CI ignores changes
    /// under `docs/evidence/**`, so compiled-in files must live elsewhere (ci_workflows, ADR-7386).
    #[test]
    fn a_step_that_says_it_skipped_is_not_a_pass() {
        let ended = |code: Option<i32>| Ended {
            code,
            timed_out: false,
            secs: 1.0,
        };
        let (built, ok, failed) = (ended(Some(0)), ended(Some(0)), ended(Some(101)));
        let crash_log = include_str!("../tests/fixtures/windows-pass-2026-11-10/native_crash_fault.log");
        assert_eq!(
            judge_step(&built, Some(&ok), crash_log),
            (
                "skipped",
                Some("native_crash: skipped (native crash bundles are Linux only)".to_owned())
            )
        );
        // device_lost_exit's parent re-prints a child that found no GPU.
        let no_gpu = "device_lost_exit: skipped: no GPU device (none)\r\ntest device_lost_exit ... ok\r\n";
        assert_eq!(judge_step(&built, Some(&ok), no_gpu).0, "skipped");
        for log in [
            include_str!("../tests/fixtures/windows-pass-2026-11-10/platform.log"),
            include_str!("../tests/fixtures/windows-pass-2026-11-10/device_lost_exit.log"),
            include_str!("../tests/fixtures/windows-pass-2026-11-10/adapter_pick.log"),
            include_str!("../tests/fixtures/windows-pass-2026-11-10/feedback_crash.log"),
            include_str!("../tests/fixtures/windows-pass-2026-11-10/feedback_native_crash_unit.log"),
            "test vfs::dir::tests::an_entry_that_vanished_mid_listing_is_skipped_not_an_error ... ok\n",
            "note: 3 tests: skipped by filter\n",
        ] {
            assert_eq!(judge_step(&built, Some(&ok), log), ("pass", None));
        }
        assert_eq!(judge_step(&built, Some(&failed), crash_log), ("fail", None));
        assert_eq!(judge_step(&failed, None, crash_log), ("build_fail", None));
    }

    /// A skipped step keeps the verdict off `PASS`: it is blocked, with the program's reason.
    #[test]
    fn a_skipped_step_blocks_the_verdict_with_its_reason() {
        let mut s = healthy();
        s["tests"][0] = json!({
            "name": "native_crash_fault",
            "status": "skipped",
            "skip_reason": "native_crash: skipped (native crash bundles are Linux only)",
            "exit_code": 0
        });
        let v = verdict(&s);
        assert_eq!(v.status, Status::Blocked, "{v:?}");
        assert_eq!(
            v.blocked,
            vec![
                "test step native_crash_fault skipped (native_crash: skipped (native crash bundles are Linux only))"
                    .to_owned()
            ]
        );
    }

    /// The `gpu_select` tests live only in the `regia_render` lib (`src/gpu_select.rs`), so the
    /// step names the lib target: a bare name filter builds and launches every `regia_render`
    /// test binary (about 150), and on a host whose application control blocks some fresh
    /// unsigned files each extra binary is another chance to fail at launch (ADR-3150).
    #[test]
    fn the_adapter_pick_step_runs_only_the_render_lib() {
        let args = TEST_STEPS
            .iter()
            .find(|(name, _)| *name == "adapter_pick")
            .map(|(_, args)| *args)
            .unwrap_or_default();
        assert_eq!(
            args,
            ["test", "-p", "regia_render", "--lib", "gpu_select"].as_slice()
        );
    }

    #[test]
    fn every_test_step_names_a_cargo_subcommand_and_a_package() {
        let mut names = std::collections::BTreeSet::new();
        for (name, args) in TEST_STEPS {
            assert!(names.insert(*name), "duplicate step {name}");
            assert_eq!(args[0], "test");
            assert!(args.contains(&"-p"), "{name} has no -p");
        }
    }
}
