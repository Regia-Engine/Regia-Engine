// SPDX-License-Identifier: MPL-2.0
//! `android-pass`: the owner's one-command real-device pass (book `user-guide/android.md`).
//!
//! With the phone on USB and the APK built, one run does, in order:
//!
//! 1. `regia install-android` (build, install, launch, forward the agent port);
//! 2. `android-lifecycle` (suspend/resume cycles and rotations, PSS leak check);
//! 3. the presets: for each of Mobile Low / Mobile High, `render.set_profile`, a warm-up, then
//!    `play.hitch_report` (frame and GPU p50/p99, per-pass GPU times) and `seeing.memory_report`;
//! 4. optionally the DO27 map (`--do27-chase`, a `do27_chase` binary built for aarch64 and the
//!    content folder, pushed to `/data/local/tmp/regia-pass`);
//! 5. the thermal run: `--thermal-minutes` (20 by default) of continuous rendering at Mobile High
//!    with a sample every `--sample-secs`: frame p50, GPU p50, thermal status, skin temperature,
//!    render scale (so the thermal governor's steps show) and battery temperature.
//!
//! ADR-1749 adds, around those steps: installing a prebuilt APK (`--apk`) and driving a prebuilt
//! `regia` CLI (`--regia`) so a host that may not build (the workstation) runs the whole pass; cold and
//! warm start times (`--start-runs`, `startup.tsv`); a screenshot per preset; the phone checks
//! (touch seen by the engine, memory trim reaching `LowMemory`, the ASTC texture path, the surface
//! audit, attached gamepads); the back button; a whole-run logcat (`logcat-all.txt`) with a crash
//! scan (`crash-scan.txt`); and `--uninstall` at the end. The parsing and verdicts are in `phone`.
//!
//! ADR-1942 turns the numbers into verdicts: after the steps, `budgets` compares the run's files
//! with `--budgets` (`budgets/android-reference.ron` by default; `docs/06` § 4) and writes
//! `budgets.json` and `budgets.tsv`, one row per budget: `pass`, `fail` or `not_measured`. Until
//! the Android column gates (M20) a failed budget is reported and leaves the pass's verdict alone;
//! `--enforce-budgets` makes it fail the pass, and so does a row a skipped step left unmeasured.
//!
//! Everything lands in `--out` (default `target/android-pass/<timestamp>`): `lifecycle.json`,
//! `presets.jsonl`, `thermal.tsv`, `startup.tsv`, `checks.json`, `budgets.json`, `budgets.tsv`,
//! `screen-*.png`, `do27-*.txt`, `logcat-regia.txt`, `logcat-all.txt`, `crash-scan.txt` and
//! `summary.txt`.
//!
//! `--crash-test` runs only the native crash record check instead (ADR-328): install, launch with
//! the `debug.regia.crash_test` property set (the engine reads address `0x10` on a worker
//! thread a few seconds after startup), wait for the process to die, clear the property,
//! relaunch, and check the next launch's `native crash record from the last run` line
//! (`crash-test.txt`, `logcat-crash-test.txt`, `tombstone.txt`).

pub mod budgets;
pub mod phone;

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use regia_agent::client::Client;
use serde_json::{Value, json};

/// Command-line options of `cargo xtask android-pass`.
#[derive(clap::Args, Clone)]
pub struct Args {
    /// Application id of the APK (the project's `Regia.toml` id).
    #[arg(long)]
    package: String,
    /// Project directory handed to `regia install-android` and where the agent endpoint lands.
    #[arg(long, default_value = ".")]
    project: PathBuf,
    /// adb serial; required when several devices are attached.
    #[arg(long)]
    device: Option<String>,
    /// Output folder (default `target/android-pass/<unix seconds>`).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Skip building and installing (the app is already installed).
    #[arg(long)]
    no_build: bool,
    /// Minutes of the thermal run (0 skips it).
    #[arg(long, default_value_t = 20)]
    thermal_minutes: u32,
    /// Seconds between thermal samples.
    #[arg(long, default_value_t = 15)]
    sample_secs: u64,
    /// Seconds each `play.hitch_report` window lasts (presets and thermal samples).
    #[arg(long, default_value_t = 8)]
    window_secs: u64,
    /// Lifecycle cycles (0 skips the lifecycle step). The default is the M1 acceptance count
    /// (`docs/specs/11-platform-and-input.md` § 8: 50 home/resume cycles and 10 rotations).
    #[arg(long, default_value_t = 50)]
    cycles: u32,
    /// Lifecycle rotations.
    #[arg(long, default_value_t = 10)]
    rotations: u32,
    /// Suspend with the screen key instead of HOME (a PIN-locked phone needs this).
    #[arg(long)]
    screen_suspend: bool,
    /// An aarch64 `do27_chase` binary: also runs the DO27 map on the device (needs `--do27-content`).
    #[arg(long, requires = "do27_content")]
    do27_chase: Option<PathBuf>,
    /// DO27's read-only content folder to push (never copied into the repo).
    #[arg(long)]
    do27_content: Option<PathBuf>,
    /// Map name for the DO27 step.
    #[arg(long, default_value = "bore_twelve")]
    do27_map: String,
    /// Run only the native crash record check (ADR-328): crash the app on purpose, relaunch it and
    /// check that the record arrived. Skips every other step.
    #[arg(long)]
    crash_test: bool,
    /// Seconds after startup before the crash test's worker thread faults (past the first frame).
    #[arg(long, default_value_t = 8)]
    crash_delay_secs: u64,
    /// A prebuilt `regia` CLI to attach the agent with, instead of `cargo run -p regia_cli` (for a
    /// host that only runs adb; build it elsewhere with `cargo build -p regia_cli`).
    #[arg(long)]
    regia: Option<PathBuf>,
    /// Install this APK with `adb install -r` instead of building one (implies `--no-build`).
    #[arg(long)]
    apk: Option<PathBuf>,
    /// Cold starts, then as many warm starts, timed with `am start -W` (0 skips the step).
    #[arg(long, default_value_t = 3)]
    start_runs: u32,
    /// Uninstall the app at the end (the owner's phones keep nothing).
    #[arg(long)]
    uninstall: bool,
    /// Render preset of the thermal run (`mobile_high`, or `mobile_low` on a phone whose GPU
    /// times out at Mobile High).
    #[arg(long, default_value = "mobile_high")]
    thermal_preset: String,
    /// The budget file the run is judged against (`budgets/android-low.ron` on a low-end phone).
    #[arg(long, default_value = budgets::DEFAULT_BUDGETS)]
    budgets: PathBuf,
    /// A budget that fails or is not measured fails the pass (the M20 gate; off until then).
    #[arg(long)]
    enforce_budgets: bool,
}

/// The system property the engine reads at startup (`regia_platform::native_crash::CRASH_TEST_PROPERTY`).
pub const CRASH_TEST_PROPERTY: &str = "debug.regia.crash_test";
/// The next launch's log line for a collected record.
pub const RECORD_LINE: &str = "native crash record from the last run";
/// The thread the crash test faults on (its kernel name).
pub const CRASH_THREAD: &str = "regia-crasher";

/// What the crash test found in the relaunch's log.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CrashTestCheck {
    /// The record line, when present.
    pub record_line: Option<String>,
    /// The record names SIGSEGV.
    pub sigsegv: bool,
    /// The record names the crash test's thread.
    pub thread: bool,
    /// The feedback plugin wrote a bundle from it (only in apps with `FeedbackPlugin`).
    pub bundle_line: Option<String>,
}

impl CrashTestCheck {
    /// The pass condition: a record line naming SIGSEGV on the crash test's thread.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.record_line.is_some() && self.sigsegv && self.thread
    }
}

/// The lines of a `logcat` dump that mention `regia` (the engine's tags and messages).
#[must_use]
pub fn regia_lines(logcat: &str) -> String {
    logcat
        .lines()
        .filter(|l| l.contains("regia"))
        .fold(String::new(), |mut out, l| {
            out.push_str(l);
            out.push('\n');
            out
        })
}

/// Reads the relaunch's `logcat` text for the record (and bundle) lines.
#[must_use]
pub fn check_crash_log(logcat: &str) -> CrashTestCheck {
    let record_line = logcat
        .lines()
        .find(|l| l.contains(RECORD_LINE))
        .map(str::to_owned);
    let bundle_line = logcat
        .lines()
        .find(|l| l.contains("native crash bundle from the last run's record"))
        .map(str::to_owned);
    CrashTestCheck {
        sigsegv: record_line.as_deref().is_some_and(|l| l.contains("SIGSEGV")),
        thread: record_line.as_deref().is_some_and(|l| l.contains(CRASH_THREAD)),
        record_line,
        bundle_line,
    }
}

/// One reading of `dumpsys thermalservice`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ThermalSample {
    /// `Thermal Status:` (0 none … 3 severe … 6 shutdown).
    pub status: Option<i32>,
    /// Skin temperature in °C.
    pub skin_c: Option<f32>,
    /// Hottest non-skin sensor (SoC, GPU, CPU) in °C.
    pub soc_c: Option<f32>,
}

/// Reads status and temperatures out of `dumpsys thermalservice`.
///
/// When the dump has a `Current temperatures from HAL:` section only that one counts: the
/// `Cached temperatures:` section above it keeps stale maxima (a Pixel 6 showed a 95 °C CPU in the
/// cache for the whole run while the live reading was 42 °C). Skin is the `SKIN` type (3) or a
/// name starting with `skin`; a device that reports none (the Pixel 6) falls back to the battery
/// sensor (type 2), which is the closest thing to what the hand feels.
pub fn parse_thermal_dump(text: &str) -> ThermalSample {
    let mut sample = ThermalSample::default();
    let live = text.contains("Current temperatures from HAL:");
    let mut in_live = false;
    let mut battery: Option<f32> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("Thermal Status:") {
            sample.status = rest.trim().parse().ok();
        }
        if !raw.starts_with([' ', '\t']) && !line.is_empty() {
            in_live = line.starts_with("Current temperatures from HAL:");
        }
        if live && !in_live {
            continue;
        }
        if let (Some(value), Some(name)) = (field(line, "mValue="), field(line, "mName=")) {
            let Ok(v) = value.parse::<f32>() else { continue };
            let kind = field(line, "mType=").and_then(|t| t.parse::<i32>().ok());
            let max = |old: Option<f32>| Some(old.map_or(v, |o: f32| o.max(v)));
            if kind == Some(3) || name.to_ascii_lowercase().starts_with("skin") {
                sample.skin_c = max(sample.skin_c);
            } else if kind == Some(2) {
                battery = max(battery);
            } else if kind != Some(-1) {
                sample.soc_c = max(sample.soc_c);
            }
        }
    }
    if sample.skin_c.is_none() {
        sample.skin_c = battery;
    }
    sample
}

/// The text after `key` up to the next `,` or `}`.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = &line[line.find(key)? + key.len()..];
    Some(rest.split([',', '}']).next().unwrap_or(rest).trim())
}

/// The header of `thermal.tsv`.
pub const THERMAL_HEADER: &str =
    "elapsed_s\tframe_p50_ms\tframe_p99_ms\tgpu_p50_ms\trender_scale\tthermal_status\tskin_c\tsoc_c\trss_mib";

/// One `thermal.tsv` row.
pub fn thermal_row(
    elapsed_s: u64,
    report: &Value,
    scale: Option<f64>,
    thermal: &ThermalSample,
    rss_mib: Option<f64>,
) -> String {
    let num = |v: Option<f64>| v.map_or("-".to_owned(), |v| format!("{v:.2}"));
    format!(
        "{elapsed_s}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        num(report["p50_ms"].as_f64()),
        num(report["p99_ms"].as_f64()),
        num(report["gpu"]["p50_ms"].as_f64()),
        num(scale),
        thermal.status.map_or("-".to_owned(), |s| s.to_string()),
        num(thermal.skin_c.map(f64::from)),
        num(thermal.soc_c.map(f64::from)),
        num(rss_mib),
    )
}

#[derive(Clone)]
struct Device {
    adb: PathBuf,
    serial: Option<String>,
}

impl Device {
    fn command(&self) -> Command {
        let mut c = Command::new(&self.adb);
        if let Some(s) = &self.serial {
            c.args(["-s", s]);
        }
        c
    }

    fn shell(&self, script: &str) -> Result<String> {
        let out = self
            .command()
            .args(["shell", script])
            .output()
            .context("running adb")?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// `adb forward --list` (every device's forwards, `serial local remote` per line), or `None`
    /// when adb does not run or fails.
    fn forward_list(&self) -> Option<String> {
        let out = self.command().args(["forward", "--list"]).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// The device's serial: `--device` when given, else `adb get-serialno` of the one device.
    fn serial_no(&self) -> Option<String> {
        if let Some(s) = &self.serial {
            return Some(s.clone());
        }
        let out = self.command().arg("get-serialno").output().ok()?;
        let serial = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        (out.status.success() && !serial.is_empty() && serial != "unknown").then_some(serial)
    }

    /// Whether adb reaches the device now (`adb get-state` prints `device`). [`Device::shell`]
    /// returns stdout whatever the exit status, so a device that dropped off USB or wireless
    /// reads as empty output; check this before trusting an empty answer (ADR-3210).
    fn reachable(&self) -> bool {
        self.command()
            .arg("get-state")
            .output()
            .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "device")
    }

    /// `adb logcat -d <args>` output, or `None` when adb does not reach the device: `logcat -d`
    /// waits for a device that dropped off instead of failing, which hung the pass's last steps
    /// (ADR-3210).
    fn logcat_dump(&self, args: &[&str]) -> Option<Vec<u8>> {
        if !self.reachable() {
            return None;
        }
        self.command()
            .args(["logcat", "-d"])
            .args(args)
            .output()
            .ok()
            .map(|o| o.stdout)
    }

    /// The app's first pid, when it runs.
    fn pid(&self, package: &str) -> Option<u32> {
        self.shell(&format!("pidof {package}"))
            .ok()?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    }
}

fn find_adb() -> PathBuf {
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(home) = std::env::var_os(var) {
            let p = Path::new(&home).join("platform-tools/adb");
            if p.exists() {
                return p;
            }
        }
    }
    PathBuf::from("adb")
}

/// Runs the pass; the exit code is failure when any checked step failed or a step aborted.
pub fn run(args: &Args, root: &Path) -> Result<ExitCode> {
    let device = Device {
        adb: find_adb(),
        serial: args.device.clone(),
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| root.join(format!("target/android-pass/{stamp}")));
    fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let budget_file = if args.budgets.is_absolute() {
        args.budgets.clone()
    } else {
        root.join(&args.budgets)
    };
    // Read first: a bad budget file should stop the run before twenty minutes of thermal samples.
    let device_budgets = budgets::AndroidBudgets::load(&budget_file)?;
    let mut summary = String::new();
    // The agent attaches below add `adb forward`s; `--uninstall` removes the ones it added.
    let forwards_before = device.forward_list();
    let model = device.shell("getprop ro.product.model").unwrap_or_default();
    let _awake = HeldSetting::hold(&device, "global", "stay_on_while_plugged_in", "7")?;
    let _notice = HeldSetting::hold(&device, "secure", "immersive_mode_confirmations", "confirmed")?;
    writeln!(summary, "android-pass: {} on {}", args.package, model.trim())?;
    // A PIN-locked phone keeps its keyguard after a wake: HOME there stops backgrounding the app
    // part way through the lifecycle cycles (the Fairphone 5, ADR-1749), the screen key does not.
    let locked_args;
    let args = if !args.screen_suspend && keyguard_stays_up(&device) {
        println!("the keyguard stays up after a wake (a locked phone): suspending with the screen key");
        writeln!(
            summary,
            "keyguard: locked, suspend with the screen key (--screen-suspend)"
        )?;
        locked_args = Args {
            screen_suspend: true,
            ..args.clone()
        };
        &locked_args
    } else {
        args
    };

    if let Some(apk) = &args.apk {
        step("install apk");
        install_apk(&device, apk, &args.package)?;
        writeln!(summary, "installed {}", apk.display())?;
    } else if !args.no_build {
        step("install-android");
        let mut cmd = regia_command(root, args);
        cmd.args(["install-android", "--no-logcat", "--project"])
            .arg(&args.project);
        if let Some(s) = &args.device {
            cmd.args(["--device", s]);
        }
        if !cmd.status().context("running regia install-android")?.success() {
            bail!("install-android failed; fix that first");
        }
    }
    let _ = device.command().args(["logcat", "-c"]).status();
    let mut stream = LogStream::start(&device, &out.join("logcat-all.txt"))?;

    if args.crash_test {
        // `--uninstall` holds on this path too (it used to return before the uninstall step and
        // left the APK on the phone, lane android-pass-v0210), also when the crash test errs.
        let result = crash_test(&device, args, &out, &mut summary);
        stream.stop();
        if args.uninstall {
            uninstall_step(&device, &args.package, forwards_before.as_deref(), &mut summary)?;
        }
        let passed = match result {
            Ok(passed) => passed,
            Err(err) => {
                writeln!(summary, "crash test aborted: {err:#}")?;
                false
            }
        };
        fs::write(out.join("summary.txt"), &summary)?;
        println!("\n{summary}\nresults in {}", out.display());
        return Ok(if passed {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }

    let result = device_steps(&device, args, root, &stream, &out, &mut summary);
    let ok = match result {
        Ok(passed) => passed,
        Err(err) => {
            writeln!(summary, "aborted: {err:#}")?;
            false
        }
    };
    step("crash scan");
    let pid = device.pid(&args.package);
    stream.stop();
    if stream.restarts.get() > 0 {
        writeln!(
            summary,
            "logcat: the stream ended {} time(s) and was restarted (adb transport reset)",
            stream.restarts.get()
        )?;
    }
    let scanned = crash_scan_step(&args.package, pid, &out, &mut summary)?;

    match device.logcat_dump(&["-s", "regia_platform", "regia_framework", "regia_audio"]) {
        Some(log) => fs::write(out.join("logcat-regia.txt"), log)?,
        None => writeln!(
            summary,
            "device: adb no longer reaches it (it dropped off during the run); logcat-regia.txt not written"
        )?,
    }
    let mut ok = ok && scanned;
    step("budgets");
    match budgets::evaluate_run(&out, &device_budgets) {
        Ok(report) => {
            summary.push_str(&report.summary());
            if args.enforce_budgets && !report.enforced_pass() {
                writeln!(summary, "budgets: enforced, and not every row passed")?;
                ok = false;
            }
        }
        Err(err) => {
            writeln!(summary, "budgets: FAIL ({err:#})")?;
            ok &= !args.enforce_budgets;
        }
    }
    if args.uninstall {
        uninstall_step(&device, &args.package, forwards_before.as_deref(), &mut summary)?;
    }
    writeln!(summary, "verdict: {}", if ok { "PASS" } else { "FAIL" })?;
    fs::write(out.join("summary.txt"), &summary)?;
    println!("\n{summary}\nresults in {}", out.display());
    Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

/// `--uninstall`: removes the app and says whether `pm list packages` still lists it, then
/// removes the agent forwards the pass's attaches added on this device (`forwards_before` is
/// `adb forward --list` at the start of the pass; ADR-5423).
fn uninstall_step(
    device: &Device,
    package: &str,
    forwards_before: Option<&str>,
    summary: &mut String,
) -> Result<()> {
    step("uninstall");
    let _ = device.command().args(["uninstall", package]).output();
    let listed = device
        .reachable()
        .then(|| device.shell(&format!("pm list packages {package}")).ok())
        .flatten();
    writeln!(
        summary,
        "uninstall: {}",
        uninstall_verdict(listed.as_deref(), package)
    )?;
    let (Some(before), Some(after), Some(serial)) =
        (forwards_before, device.forward_list(), device.serial_no())
    else {
        writeln!(
            summary,
            "forwards: UNKNOWN (no `adb forward --list`; remove this pass's agent forward by hand)"
        )?;
        return Ok(());
    };
    let added = added_forwards(before, &after, &serial);
    for local in &added {
        let _ = device.command().args(["forward", "--remove", local]).output();
    }
    // Every forward the device still has, then the added ones among them.
    let still = device
        .forward_list()
        .map(|now| added_forwards("", &now, &serial))
        .unwrap_or_default();
    let left: Vec<String> = added.iter().filter(|l| still.contains(l)).cloned().collect();
    if added.is_empty() {
        writeln!(summary, "forwards: none added")?;
    } else if left.is_empty() {
        writeln!(summary, "forwards: removed {}", added.join(", "))?;
    } else {
        writeln!(summary, "forwards: FAILED to remove {}", left.join(", "))?;
    }
    Ok(())
}

/// The local specs (`tcp:46771`) of the forwards of device `serial` that `after` lists and
/// `before` does not: the ones this pass added. Both are `adb forward --list` output, one
/// `serial local remote` per line; any other line is ignored.
#[must_use]
pub fn added_forwards(before: &str, after: &str, serial: &str) -> Vec<String> {
    let entries = |text: &str| -> Vec<(String, String, String)> {
        text.lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                let entry = (parts.next()?, parts.next()?, parts.next()?);
                parts
                    .next()
                    .is_none()
                    .then(|| (entry.0.to_owned(), entry.1.to_owned(), entry.2.to_owned()))
            })
            .collect()
    };
    let old = entries(before);
    entries(after)
        .into_iter()
        .filter(|e| e.0 == serial && !old.contains(e))
        .map(|e| e.1)
        .collect()
}

/// The uninstall line from `pm list packages <package>` after `adb uninstall`: `done` when the
/// exact package is no longer listed (the filter also lists packages that only contain the name).
/// `None` means adb did not reach the device, so nothing is known (a phone that dropped off
/// mid-run once read as `done`, ADR-3210).
#[must_use]
pub fn uninstall_verdict(listed: Option<&str>, package: &str) -> &'static str {
    let Some(listed) = listed else {
        return "UNKNOWN (adb does not reach the device; check `pm list packages` and uninstall by hand)";
    };
    let still = listed
        .lines()
        .any(|l| l.trim().strip_prefix("package:") == Some(package));
    if still {
        "FAILED (still installed; uninstall by hand)"
    } else {
        "done"
    }
}

/// Every step after the install (startup, lifecycle, presets, phone checks, DO27, thermal, back
/// button). `Err` aborts the rest; `run` still scans for crashes, writes the summary and
/// uninstalls.
fn device_steps(
    device: &Device,
    args: &Args,
    root: &Path,
    stream: &LogStream,
    out: &Path,
    summary: &mut String,
) -> Result<bool> {
    let mut ok = true;
    if args.start_runs > 0 {
        step("startup");
        startup(device, args, stream, out, summary)?;
    }

    if args.cycles > 0 {
        step("lifecycle");
        let report = out.join("lifecycle.json");
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.args(["android-lifecycle", "--package", &args.package, "--cycles"])
            .arg(args.cycles.to_string())
            .args(["--rotations", &args.rotations.to_string(), "--report"])
            .arg(&report);
        if args.screen_suspend {
            cmd.args(["--suspend-with", "screen"]);
        }
        if let Some(s) = &args.device {
            cmd.args(["--device", s]);
        }
        let passed = cmd.status().context("running android-lifecycle")?.success();
        ok &= passed;
        writeln!(
            summary,
            "lifecycle: {} (see lifecycle.json)",
            if passed { "PASS" } else { "FAIL" }
        )?;
    }
    // The lifecycle step leaves the app running; (re-)attach the agent port for the rest.
    reattach(root, args, device)?;

    let mut client = Client::connect_project(
        &args
            .project
            .canonicalize()
            .with_context(|| args.project.display().to_string())?,
    )
    .context("cannot reach the app's agent (is it running? see `regia install-android`)")?;

    step("presets");
    let mut jsonl = String::new();
    for preset in ["mobile_low", "mobile_high"] {
        client.call("render.set_profile", json!({ "preset": preset }))?;
        let pacing_before = client.call("pacing.get", json!({})).unwrap_or(Value::Null);
        let report = hitch(&mut client, args.window_secs, 4.0)?;
        let pacing_after = client.call("pacing.get", json!({})).unwrap_or(Value::Null);
        let memory = client
            .call("seeing.memory_report", json!({}))
            .unwrap_or(Value::Null);
        let pss_kb = device
            .shell(&format!("dumpsys meminfo {}", args.package))
            .ok()
            .and_then(|text| crate::android_lifecycle::logic::parse_meminfo_total_pss(&text));
        let pacing = phone::pacing_window(&pacing_before, &pacing_after);
        writeln!(
            summary,
            "{preset}: frame p50 {} ms p99 {} ms, work p99 {} ms, gpu p50 {} ms, pacing {} at {} Hz, {} vsyncs per frame, {} % late",
            report["p50_ms"],
            report["p99_ms"],
            report["work"]["p99_ms"],
            report["gpu"]["p50_ms"],
            pacing["source"],
            pacing["refresh_hz"],
            pacing["swap_interval"],
            pacing["late_pct"]
        )?;
        jsonl.push_str(
            &json!({
                "preset": preset,
                "hitch": report,
                "memory": memory,
                "pss_kb": pss_kb,
                "pacing": pacing,
            })
            .to_string(),
        );
        jsonl.push('\n');
        screenshot(device, &out.join(format!("screen-{preset}.png")));
    }
    fs::write(out.join("presets.jsonl"), jsonl)?;

    step("phone checks");
    ok &= phone_checks(device, &mut client, args, stream, out, summary)?;

    if let (Some(bin), Some(content)) = (&args.do27_chase, &args.do27_content) {
        step("do27 map");
        do27(device, bin, content, &args.do27_map, out, summary)?;
        reattach(root, args, device)?;
        client = Client::connect_project(&args.project.canonicalize()?)?;
    }

    if args.thermal_minutes > 0 {
        step("thermal");
        client.call("render.set_profile", json!({ "preset": args.thermal_preset }))?;
        thermal_run(device, &mut client, args, root, stream, out, summary)?;
        screenshot(device, &out.join("screen-after-thermal.png"));
    }

    step("back button");
    ok &= back_button(device, args, stream, summary)?;

    Ok(ok)
}

/// `--crash-test`: crash on purpose, relaunch, check the record. The property is cleared on every
/// path, so a failed run never leaves the app crashing at each launch.
fn crash_test(device: &Device, args: &Args, out: &Path, summary: &mut String) -> Result<bool> {
    step("crash test");
    let package = &args.package;
    // The engine logs under its tracing targets as logcat tags (`regia_platform`,
    // `regia_framework::feedback`); `logcat -s` cannot name a tag with `::`, so filter here.
    let regia_log = || {
        device
            .logcat_dump(&[])
            .map(|o| regia_lines(&String::from_utf8_lossy(&o)))
            .unwrap_or_default()
    };
    device.shell(&format!("am force-stop {package}"))?;
    let _ = device.command().args(["logcat", "-c"]).status();
    let _ = device.command().args(["logcat", "-b", "crash", "-c"]).status();
    let crashed = run_until_death(device, args);
    // Cleared on every path, before anything else can fail.
    let cleared = device.shell(&format!("setprop {CRASH_TEST_PROPERTY} ''"));
    let first_log = regia_log();
    let tombstone = device
        .logcat_dump(&["-b", "crash"])
        .map(|o| String::from_utf8_lossy(&o).into_owned())
        .unwrap_or_default();
    fs::write(out.join("tombstone.txt"), &tombstone)?;
    cleared?;
    let (started, died) = crashed?;
    if !died {
        device.shell(&format!("am force-stop {package}"))?;
        fs::write(out.join("logcat-crash-test.txt"), &first_log)?;
        writeln!(
            summary,
            "crash test: FAIL (the app {} within {} s; see logcat-crash-test.txt)",
            if started { "did not die" } else { "never started" },
            args.crash_delay_secs + 60
        )?;
        return Ok(false);
    }
    let _ = device.command().args(["logcat", "-c"]).status();
    launch_app(device, package)?;
    let mut log = String::new();
    let mut check = CrashTestCheck::default();
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        log = regia_log();
        check = check_crash_log(&log);
        if check.record_line.is_some() {
            // Give the feedback plugin a moment to log its bundle line too.
            std::thread::sleep(Duration::from_secs(3));
            log = regia_log();
            check = check_crash_log(&log);
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    // Debuggable builds only: the collected record files.
    let files = device
        .shell(&format!("run-as {package} ls -l files/crash 2>&1"))
        .unwrap_or_default();
    device.shell(&format!("am force-stop {package}"))?;
    fs::write(
        out.join("logcat-crash-test.txt"),
        format!("== crashing run ==\n{first_log}\n== next launch ==\n{log}"),
    )?;
    let mut report = String::new();
    writeln!(
        report,
        "record line: {}",
        check.record_line.as_deref().unwrap_or("MISSING")
    )?;
    writeln!(report, "names SIGSEGV: {}", check.sigsegv)?;
    writeln!(report, "names thread {CRASH_THREAD}: {}", check.thread)?;
    writeln!(
        report,
        "feedback bundle: {}",
        check
            .bundle_line
            .as_deref()
            .unwrap_or("none (the app has no FeedbackPlugin, or crash bundles are off)")
    )?;
    writeln!(report, "files/crash (run-as):\n{files}")?;
    fs::write(out.join("crash-test.txt"), &report)?;
    let passed = check.passed();
    writeln!(
        summary,
        "crash test: {} (see crash-test.txt)",
        if passed { "PASS" } else { "FAIL" }
    )?;
    Ok(passed)
}

/// Launches the app's launcher activity.
fn launch_app(device: &Device, package: &str) -> Result<String> {
    device.shell(&format!(
        "monkey -p {package} -c android.intent.category.LAUNCHER 1 >/dev/null 2>&1"
    ))
}

/// Sets the crash-test property, launches the app and waits for its process to start and then
/// die. Returns `(started, died)`. The caller clears the property whatever this returns.
fn run_until_death(device: &Device, args: &Args) -> Result<(bool, bool)> {
    let package = &args.package;
    device.shell(&format!(
        "setprop {CRASH_TEST_PROPERTY} segv:{}",
        args.crash_delay_secs
    ))?;
    launch_app(device, package)?;
    let mut started = false;
    let deadline = Instant::now() + Duration::from_secs(args.crash_delay_secs + 60);
    while Instant::now() < deadline {
        let running = device
            .shell(&format!("pidof {package}"))
            .is_ok_and(|p| !p.trim().is_empty());
        started |= running;
        if started && !running {
            return Ok((true, true));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok((started, false))
}

fn step(name: &str) {
    println!("== android-pass: {name}");
}

/// The `regia` CLI: the prebuilt `--regia` binary, else `cargo run -p regia_cli` in the repo.
fn regia_command(root: &Path, args: &Args) -> Command {
    if let Some(bin) = &args.regia {
        return Command::new(bin);
    }
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(root).args(["run", "-q", "-p", "regia_cli", "--"]);
    cmd
}

/// Attaches the agent to the running app (`regia install-android --attach`: `adb forward` and the
/// host endpoint file, no reinstall); launches the app first when it is not running.
fn reattach(root: &Path, args: &Args, device: &Device) -> Result<()> {
    if device.pid(&args.package).is_none() {
        launch_app(device, &args.package)?;
    }
    let mut cmd = regia_command(root, args);
    cmd.args(["install-android", "--attach", "--no-logcat", "--project"])
        .arg(&args.project);
    if let Some(s) = &args.device {
        cmd.args(["--device", s]);
    }
    if !cmd.status().context("attaching the agent")?.success() {
        bail!("could not attach the agent (is the app running? `adb logcat -s regia_agent`)");
    }
    Ok(())
}

/// `play.hitch_report` over a window; the call blocks until the report is ready.
fn hitch(client: &mut Client, seconds: u64, warmup: f64) -> Result<Value> {
    // A phone at several hundred ms a frame needs longer than the default 30 s answer deadline.
    let timeout_ms = ((seconds as f64 + warmup) * 1000.0 * 4.0 + 60_000.0).min(600_000.0);
    Ok(client.call(
        "play.hitch_report",
        json!({ "seconds": seconds as f64, "warmup_s": warmup, "timeout_ms": timeout_ms }),
    )?)
}

/// Re-attaches the agent after its connection dropped (an adb transport reset closes the forwarded
/// socket while the app keeps running) and puts the thermal preset back.
fn reconnect(root: &Path, args: &Args, device: &Device) -> Result<Client> {
    reattach(root, args, device)?;
    let mut client = Client::connect_project(&args.project.canonicalize()?)?;
    client.call("render.set_profile", json!({ "preset": args.thermal_preset }))?;
    Ok(client)
}

fn thermal_run(
    device: &Device,
    client: &mut Client,
    args: &Args,
    root: &Path,
    stream: &LogStream,
    out: &Path,
    summary: &mut String,
) -> Result<()> {
    let mut tsv = String::from(THERMAL_HEADER);
    tsv.push('\n');
    let total = Duration::from_secs(u64::from(args.thermal_minutes) * 60);
    let started = Instant::now();
    let mut worst = 0;
    let mut first_p50 = None;
    let mut last_p50 = None;
    let mut reconnects = 0u32;
    while started.elapsed() < total {
        stream.revive();
        let report = match hitch(client, args.window_secs, 0.0) {
            Ok(report) => report,
            // One reconnect per failed sample: a second failure in a row is a real fault.
            Err(err) => {
                println!("thermal sample failed ({err:#}); re-attaching the agent");
                *client = reconnect(root, args, device)
                    .with_context(|| format!("re-attaching after a failed thermal sample ({err:#})"))?;
                reconnects += 1;
                hitch(client, args.window_secs, 0.0)?
            }
        };
        let scale = client
            .call("render.get_profile", json!({}))
            .ok()
            .and_then(|p| p["profile"]["render_scale"].as_f64());
        let thermal = parse_thermal_dump(&device.shell("dumpsys thermalservice").unwrap_or_default());
        let rss = client
            .call("seeing.memory_report", json!({}))
            .ok()
            .and_then(|m| m["rss_bytes"].as_f64())
            .map(|b| b / 1_048_576.0);
        worst = worst.max(thermal.status.unwrap_or(0));
        first_p50.get_or_insert(report["p50_ms"].as_f64());
        last_p50 = Some(report["p50_ms"].as_f64());
        let row = thermal_row(started.elapsed().as_secs(), &report, scale, &thermal, rss);
        println!("{row}");
        tsv.push_str(&row);
        tsv.push('\n');
        fs::write(out.join("thermal.tsv"), &tsv)?;
        std::thread::sleep(Duration::from_secs(
            args.sample_secs.saturating_sub(args.window_secs),
        ));
    }
    writeln!(
        summary,
        "thermal: {} min at {}, worst status {worst}, frame p50 {:?} -> {:?} ms, agent reconnects {reconnects}",
        args.thermal_minutes,
        args.thermal_preset,
        first_p50.flatten(),
        last_p50.flatten()
    )?;
    Ok(())
}

fn do27(
    device: &Device,
    bin: &Path,
    content: &Path,
    map: &str,
    out: &Path,
    summary: &mut String,
) -> Result<()> {
    const DIR: &str = "/data/local/tmp/regia-pass";
    let push = |from: &Path, to: &str| -> Result<()> {
        let ok = device
            .command()
            .arg("push")
            .arg(from)
            .arg(to)
            .output()?
            .status
            .success();
        if !ok {
            bail!("adb push {} failed", from.display());
        }
        Ok(())
    };
    device.shell(&format!("mkdir -p {DIR}/content"))?;
    push(bin, &format!("{DIR}/do27_chase"))?;
    device.shell(&format!("chmod 755 {DIR}/do27_chase"))?;
    // The chase reads the plate cameras from the content root; the map folder alone is not enough.
    for spec in ["ue_camera_spec.json", "fx_camera_spec.json"] {
        let from = content.join(spec);
        if from.is_file() {
            push(&from, &format!("{DIR}/content/{spec}"))?;
        }
    }
    if device
        .shell(&format!("[ -d {DIR}/content/maps/{map} ] && echo yes"))?
        .trim()
        != "yes"
    {
        device.shell(&format!("mkdir -p {DIR}/content/maps"))?;
        push(&content.join("maps").join(map), &format!("{DIR}/content/maps/"))?;
    }
    for tier in ["mobile-low", "mobile-high"] {
        let text = device.shell(&format!(
            "cd {DIR} && REGIA_DO27_CONTENT={DIR}/content LOOK_R2_TIER={tier} ./do27_chase --map {map} --size 1200x540 --frames 60 2>&1"
        ))?;
        fs::write(out.join(format!("do27-{map}-{tier}.txt")), &text)?;
        let line = text
            .lines()
            .rev()
            .find(|l| l.contains("p50"))
            .unwrap_or("(no p50 line, see the file)");
        writeln!(summary, "do27 {map} {tier}: {}", line.trim())?;
    }
    device.shell(&format!("rm -rf {DIR}"))?;
    Ok(())
}

/// The app's launcher component (`<package>/.RegiaActivity`).
fn activity(package: &str) -> String {
    crate::android_lifecycle::logic::component(package, ".RegiaActivity")
}

/// `adb install -r <apk>`, then the `INTERNET` grant hardened builds leave off (as
/// `regia install-android` does; stock Android refuses the grant, which is fine).
fn install_apk(device: &Device, apk: &Path, package: &str) -> Result<()> {
    let out = device
        .command()
        .args(["install", "-r"])
        .arg(apk)
        .output()
        .context("running adb install")?;
    if !out.status.success() {
        bail!(
            "adb install {} failed: {}{}",
            apk.display(),
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let _ = device.shell(&format!("pm grant {package} android.permission.INTERNET"));
    Ok(())
}

/// Wakes the phone, asks the window manager to dismiss the keyguard and reports whether it is
/// still showing (a PIN or pattern lock). Unknown (no `isKeyguardShowing` line) counts as not
/// locked, so the pass keeps its HOME default.
fn keyguard_stays_up(device: &Device) -> bool {
    let _ = device.shell("input keyevent KEYCODE_WAKEUP");
    let _ = device.shell("wm dismiss-keyguard");
    std::thread::sleep(Duration::from_secs(1));
    device
        .shell("dumpsys window")
        .ok()
        .and_then(|text| phone::keyguard_showing(&text))
        .unwrap_or(false)
}

/// A device setting held at a value for the whole pass and put back on drop. The pass holds two:
/// `global stay_on_while_plugged_in` 7 (the screen stays on while the phone is on USB, as
/// `android-lifecycle` does for its own run), and `secure immersive_mode_confirmations`
/// `confirmed`, so the system's one-time "Viewing full screen" notice does not take input focus
/// from the app part way through (the test Pixel 9 Pro showed it after a relaunch; the back key
/// then went to the notice, ADR-3210).
struct HeldSetting<'a> {
    device: &'a Device,
    namespace: &'static str,
    key: &'static str,
    original: Option<String>,
}

impl<'a> HeldSetting<'a> {
    fn hold(device: &'a Device, namespace: &'static str, key: &'static str, value: &str) -> Result<Self> {
        // An unreachable device answers every `settings get` with nothing, which would read as
        // an unset key and be "restored" by deleting it.
        anyhow::ensure!(
            device.reachable(),
            "adb does not reach the device (`adb get-state`): cannot hold {namespace} {key}"
        );
        let now = device.shell(&format!("settings get {namespace} {key}"))?;
        let original = crate::android_lifecycle::logic::parse_setting(&now);
        device.shell(&format!("settings put {namespace} {key} {value}"))?;
        let held = Self {
            device,
            namespace,
            key,
            original,
        };
        println!(
            "{namespace} {key} held at {value} for the pass (if interrupted: adb shell {})",
            held.restore_command()
        );
        Ok(held)
    }

    fn restore_command(&self) -> String {
        let (namespace, key) = (self.namespace, self.key);
        self.original.as_deref().map_or_else(
            || format!("settings delete {namespace} {key}"),
            |v| format!("settings put {namespace} {key} {v}"),
        )
    }
}

impl Drop for HeldSetting<'_> {
    fn drop(&mut self) {
        let restored = self.device.reachable() && self.device.shell(&self.restore_command()).is_ok();
        if !restored {
            eprintln!(
                "warning: could not restore {} {} (adb does not reach the device): run `adb shell {}` when it is back",
                self.namespace,
                self.key,
                self.restore_command()
            );
        }
    }
}

/// `adb logcat -v threadtime -b main,system,crash` streamed into a file for the whole pass, so
/// the crash scan and the timed steps see every line even after `logcat -c` or a ring overflow.
/// An adb transport reset ends the `adb logcat` child; [`LogStream::revive`] starts a new one
/// that appends from the last complete line's time (`-T`), so the file keeps every line.
struct LogStream {
    device: Device,
    child: std::cell::RefCell<Option<Child>>,
    path: PathBuf,
    restarts: std::cell::Cell<u32>,
}

impl LogStream {
    fn start(device: &Device, path: &Path) -> Result<Self> {
        let file = fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let child = Self::spawn(device, file, None)?;
        Ok(Self {
            device: device.clone(),
            child: std::cell::RefCell::new(Some(child)),
            path: path.to_owned(),
            restarts: std::cell::Cell::new(0),
        })
    }

    fn spawn(device: &Device, file: fs::File, since: Option<&str>) -> Result<Child> {
        let mut cmd = device.command();
        cmd.args(["logcat", "-v", "threadtime", "-b", "main,system,crash"]);
        if let Some(stamp) = since {
            cmd.args(["-T", stamp]);
        }
        cmd.stdin(Stdio::null())
            .stdout(file)
            .stderr(Stdio::null())
            .spawn()
            .context("starting adb logcat")
    }

    /// Restarts `adb logcat` when it has exited (the stream was not stopped on purpose). The new
    /// stream appends after a newline, from the last complete line's stamp (that line repeats).
    fn revive(&self) {
        let mut child = self.child.borrow_mut();
        let Some(running) = child.as_mut() else {
            return;
        };
        if !matches!(running.try_wait(), Ok(Some(_))) {
            return;
        }
        let text = fs::read_to_string(&self.path).unwrap_or_default();
        let since = phone::last_log_stamp(&text);
        let file = fs::OpenOptions::new().append(true).open(&self.path);
        let restarted = file.map_err(anyhow::Error::from).and_then(|mut file| {
            use std::io::Write as _;
            if !text.is_empty() && !text.ends_with('\n') {
                file.write_all(b"\n")?;
            }
            Self::spawn(&self.device, file, since.as_deref())
        });
        match restarted {
            Ok(new) => {
                *child = Some(new);
                self.restarts.set(self.restarts.get() + 1);
                println!("adb logcat ended (transport reset?): restarted from {since:?}");
            }
            Err(err) => eprintln!("warning: could not restart adb logcat: {err:#}"),
        }
    }

    /// Bytes written so far: a mark to read from later.
    fn offset(&self) -> u64 {
        self.revive();
        fs::metadata(&self.path).map_or(0, |m| m.len())
    }

    /// The text written since `offset`.
    fn since(&self, offset: u64) -> String {
        let bytes = fs::read(&self.path).unwrap_or_default();
        let from = usize::try_from(offset).unwrap_or(usize::MAX).min(bytes.len());
        String::from_utf8_lossy(&bytes[from..]).into_owned()
    }

    /// Waits until a line containing `needle` arrives after `offset`; returns the text since
    /// `offset` either way.
    fn wait_for(&self, offset: u64, needle: &str, timeout: Duration) -> (bool, String) {
        let deadline = Instant::now() + timeout;
        loop {
            self.revive();
            let text = self.since(offset);
            if text.contains(needle) {
                return (true, text);
            }
            if Instant::now() >= deadline {
                return (false, text);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.get_mut().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for LogStream {
    fn drop(&mut self) {
        self.stop();
    }
}

/// `am start -W` of the launcher activity.
fn am_start_w(device: &Device, package: &str) -> Result<String> {
    device.shell(&format!(
        "am start -W -a android.intent.action.MAIN -c android.intent.category.LAUNCHER -n {}",
        activity(package)
    ))
}

/// Cold starts (force-stop, `am start -W`) then warm starts (suspend with HOME or the screen
/// key, `am start -W`): `startup.tsv` and one summary line. The engine's first frame is read from
/// the whole-run logcat.
fn startup(device: &Device, args: &Args, stream: &LogStream, out: &Path, summary: &mut String) -> Result<()> {
    use crate::android_lifecycle::logic::{MARK_FIRST_FRAME, MARK_SUSPENDED};
    let package = &args.package;
    let _ = device.shell("input keyevent KEYCODE_WAKEUP");
    let mut rows = Vec::new();
    for i in 0..args.start_runs {
        device.shell(&format!("am force-stop {package}"))?;
        std::thread::sleep(Duration::from_secs(2));
        let from = stream.offset();
        let text = am_start_w(device, package)?;
        let (_, seg) = stream.wait_for(from, MARK_FIRST_FRAME, Duration::from_secs(60));
        let row = phone::StartRow {
            kind: "cold",
            timing: phone::parse_am_start(&text),
            first_frame_ms: phone::start_to_first_frame_ms(&seg, package),
        };
        println!("cold start {}: {:?}", i + 1, row);
        rows.push(row);
        std::thread::sleep(Duration::from_secs(3));
    }
    let away = if args.screen_suspend {
        "KEYCODE_SLEEP"
    } else {
        "KEYCODE_HOME"
    };
    for i in 0..args.start_runs {
        let from = stream.offset();
        device.shell(&format!("input keyevent {away}"))?;
        let (suspended, _) = stream.wait_for(from, MARK_SUSPENDED, Duration::from_secs(20));
        if !suspended {
            writeln!(
                summary,
                "startup: warm start {} skipped ({away} did not suspend the app)",
                i + 1
            )?;
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
        let from = stream.offset();
        let text = am_start_w(device, package)?;
        let (_, seg) = stream.wait_for(from, MARK_FIRST_FRAME, Duration::from_secs(30));
        let row = phone::StartRow {
            kind: "warm",
            timing: phone::parse_am_start(&text),
            first_frame_ms: phone::start_to_first_frame_ms(&seg, package),
        };
        println!("warm start {}: {:?}", i + 1, row);
        rows.push(row);
        std::thread::sleep(Duration::from_secs(2));
    }
    let _ = device.shell("input keyevent KEYCODE_WAKEUP");
    let (tsv, line) = phone::startup_table(&rows);
    fs::write(out.join("startup.tsv"), tsv)?;
    writeln!(summary, "{line} (see startup.tsv)")?;
    Ok(())
}

/// `adb exec-out screencap -p` into `path`; a failure is only printed.
fn screenshot(device: &Device, path: &Path) {
    match device.command().args(["exec-out", "screencap", "-p"]).output() {
        Ok(o) if o.status.success() && o.stdout.starts_with(b"\x89PNG") => {
            if let Err(err) = fs::write(path, &o.stdout) {
                eprintln!("warning: writing {}: {err}", path.display());
            }
        }
        _ => eprintln!("warning: screencap for {} failed", path.display()),
    }
}

/// Touch, memory trim, ASTC, the surface audit and gamepads (`checks.json`). Returns false when
/// a required check failed (touch, trim, ASTC, audit).
fn phone_checks(
    device: &Device,
    client: &mut Client,
    args: &Args,
    stream: &LogStream,
    out: &Path,
    summary: &mut String,
) -> Result<bool> {
    let package = &args.package;
    let mut ok = true;
    let mut checks = serde_json::Map::new();

    // Touch: hold a finger low in the middle of the screen (`phone::touch_point`) for 6 s and ask
    // the engine what it sees at 2 s and
    // 4 s (a phone at over a second a frame needs the second look).
    let size = phone::parse_wm_size(&device.shell("wm size").unwrap_or_default());
    let landscape = phone::is_landscape(&device.shell("dumpsys input").unwrap_or_default());
    let touch = if let Some((w, h)) = size {
        let (x, y) = phone::touch_point(w, h, landscape.unwrap_or(false));
        let swipe = format!("input touchscreen swipe {x} {y} {x} {y} 6000");
        let mut hold = device
            .command()
            .args(["shell", swipe.as_str()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("adb input swipe")?;
        let mut report = Value::Null;
        for _ in 0..2 {
            std::thread::sleep(Duration::from_secs(2));
            report = client
                .call("input.touch_controls", json!({}))
                .unwrap_or(Value::Null);
            if phone::touch_check(&report).passed() {
                break;
            }
        }
        let _ = hold.wait();
        std::thread::sleep(Duration::from_millis(1500));
        let after = client
            .call("input.touch_controls", json!({}))
            .unwrap_or(Value::Null);
        let held = phone::touch_check(&report);
        let released = phone::touch_check(&after);
        checks.insert(
            "touch".into(),
            json!({ "tap": [x, y], "landscape": landscape, "held": report, "released": after }),
        );
        let passed = held.passed() && !released.passed();
        writeln!(
            summary,
            "touch: {} (held at {x},{y}: {} finger(s) at {:?}; after release {}; controls installed {}, held actions {:?})",
            if passed { "PASS" } else { "FAIL" },
            held.fingers,
            held.position,
            released.fingers,
            held.controls_installed,
            held.held_actions
        )?;
        passed
    } else {
        writeln!(summary, "touch: FAIL (no `wm size`)")?;
        false
    };
    ok &= touch;

    // Memory trim: onTrimMemory reaches the engine as LowMemory; the app keeps running.
    let pid = device.pid(package);
    let from = stream.offset();
    let trim = device
        .shell(&format!("am send-trim-memory {package} RUNNING_CRITICAL 2>&1"))
        .unwrap_or_default();
    let (seen, _) = stream.wait_for(from, phone::LOW_MEMORY_LINE, Duration::from_secs(10));
    std::thread::sleep(Duration::from_secs(2));
    let same = pid.is_some() && device.pid(package) == pid;
    checks.insert(
        "trim_memory".into(),
        json!({ "am": trim.trim(), "low_memory_line": seen, "same_pid": same }),
    );
    writeln!(
        summary,
        "trim memory: {} (RUNNING_CRITICAL: LowMemory line {seen}, still running as the same pid {same}{})",
        if seen && same { "PASS" } else { "FAIL" },
        if trim.trim().is_empty() {
            String::new()
        } else {
            format!("; am said `{}`", trim.trim())
        }
    )?;
    ok &= seen && same;

    // ASTC: the device feature and what the paks carry.
    let caps = client
        .call("render.get_capabilities", json!({}))
        .unwrap_or(Value::Null);
    let cooked = client
        .call("assets.cooked_pack", json!({}))
        .unwrap_or(Value::Null);
    let astc = phone::astc_check(&caps, &cooked);
    writeln!(summary, "{}", astc.line())?;
    ok &= astc.passed();
    checks.insert("capabilities".into(), caps);
    checks.insert("cooked_pack".into(), cooked);

    // The surface audit after the lifecycle step's cycles.
    let lifecycle = client.call("lifecycle.get", json!({})).unwrap_or(Value::Null);
    let clean = lifecycle["audit"]["clean"].as_bool();
    writeln!(
        summary,
        "surface audit: {} (suspends {}, resumes {}, violations {})",
        match clean {
            Some(true) => "PASS",
            Some(false) => "FAIL",
            None => "not reported",
        },
        lifecycle["audit"]["suspends"],
        lifecycle["audit"]["resumes"],
        lifecycle["audit"]["violation_count"]
    )?;
    ok &= clean != Some(false);
    checks.insert("lifecycle".into(), lifecycle);

    // Gamepads: report what is attached; none is not a failure.
    let pads = phone::gamepads_in_dumpsys(&device.shell("dumpsys input").unwrap_or_default());
    writeln!(
        summary,
        "gamepads: {}",
        if pads.is_empty() {
            "none attached (not tested)".to_owned()
        } else {
            pads.join(", ")
        }
    )?;
    checks.insert("gamepads".into(), json!(pads));
    let thermal = client.call("thermal.get", json!({})).unwrap_or(Value::Null);
    checks.insert("thermal".into(), thermal);

    fs::write(
        out.join("checks.json"),
        serde_json::to_string_pretty(&Value::Object(checks))?,
    )?;
    Ok(ok)
}

/// The back button: Android 12+ moves a root launcher activity to the back (the engine sees
/// `Suspended`); older releases finish it (`WillTerminate`). Either is fine; a crash or a
/// relaunch that does not render is not. The app runs again afterwards.
fn back_button(device: &Device, args: &Args, stream: &LogStream, summary: &mut String) -> Result<bool> {
    use crate::android_lifecycle::logic::{MARK_FIRST_FRAME, MARK_SUSPENDED};
    let package = &args.package;
    let pid = device.pid(package);
    // The soak before this step can leave the screen dimmed (or, without a held stay-on setting,
    // the keyguard back up): wake the phone and drop an unsecured keyguard first, as the pass's
    // start does (ADR-3210).
    let _ = keyguard_stays_up(device);
    // The key goes to the focused window. Name it when it is not the app's (a system notice or
    // dialog over the app), so a failure says what took the key.
    let focus = device
        .shell("dumpsys window")
        .ok()
        .and_then(|text| phone::focused_window(&text));
    let foreign_focus = focus
        .as_deref()
        .filter(|window| !phone::window_is_package(window, package))
        .map(|window| format!(", focus before the key {window}"))
        .unwrap_or_default();
    let from = stream.offset();
    device.shell("input keyevent KEYCODE_BACK")?;
    let (suspended, seg) = stream.wait_for(from, MARK_SUSPENDED, Duration::from_secs(15));
    std::thread::sleep(Duration::from_secs(2));
    let exited = phone::count_lines(&stream.since(from), phone::EXITING_LINE) > 0;
    let after = device.pid(package);
    let from = stream.offset();
    let _ = device.shell("input keyevent KEYCODE_WAKEUP");
    am_start_w(device, package)?;
    let (rendered, _) = stream.wait_for(from, MARK_FIRST_FRAME, Duration::from_secs(60));
    let scan = phone::crash_scan(&seg, package, &pid.into_iter().collect::<Vec<_>>());
    let passed = (suspended || exited) && rendered && scan.app.is_empty();
    writeln!(
        summary,
        "back button: {} (suspended {suspended}, exited {exited}, process {}, relaunch rendered {rendered}{foreign_focus})",
        if passed { "PASS" } else { "FAIL" },
        match (pid, after) {
            (Some(a), Some(b)) if a == b => "kept",
            (_, None) => "gone",
            _ => "replaced",
        }
    )?;
    Ok(passed)
}

/// Scans the whole-run logcat for crashes of the app (`crash-scan.txt`).
fn crash_scan_step(package: &str, pid: Option<u32>, out: &Path, summary: &mut String) -> Result<bool> {
    let log = fs::read_to_string(out.join("logcat-all.txt")).unwrap_or_default();
    let mut pids = phone::started_pids(&log, package);
    pids.extend(pid);
    let scan = phone::crash_scan(&log, package, &pids);
    let mut text = format!("pids: {pids:?}\napp crash lines: {}\n", scan.app.len());
    for line in &scan.app {
        writeln!(text, "  {line}")?;
    }
    writeln!(text, "other processes' crash lines: {}", scan.foreign.len())?;
    for line in &scan.foreign {
        writeln!(text, "  {line}")?;
    }
    fs::write(out.join("crash-scan.txt"), text)?;
    writeln!(
        summary,
        "crash scan: {} ({} app crash lines, {} from other processes, {} logcat lines, pids {pids:?})",
        if scan.app.is_empty() { "PASS" } else { "FAIL" },
        scan.app.len(),
        scan.foreign.len(),
        log.lines().count()
    )?;
    Ok(scan.app.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = "IsStatusOverride: false\nThermal Status: 3\nCached temperatures:\n\tTemperature{mValue=42.7, mType=3, mName=skin1, mStatus=1}\n\tTemperature{mValue=76.0, mType=1, mName=GPU, mStatus=0}\n\tTemperature{mValue=71.5, mType=0, mName=CPU0, mStatus=0}\n";

    #[test]
    fn the_uninstall_verdict_reads_the_exact_package() {
        let pkg = "com.regiaengine.samples.arena";
        assert_eq!(uninstall_verdict(Some(""), pkg), "done");
        assert_eq!(
            uninstall_verdict(Some("package:com.regiaengine.samples.arena_tools\n"), pkg),
            "done",
            "a package that only contains the name is another app"
        );
        assert!(
            uninstall_verdict(Some("package:com.regiaengine.samples.arena\r\n"), pkg).starts_with("FAILED"),
            "the app is still listed"
        );
        assert!(
            uninstall_verdict(None, pkg).starts_with("UNKNOWN"),
            "an unreachable device is not an uninstalled app"
        );
    }

    #[test]
    fn an_unreachable_device_gives_no_dump_and_an_unknown_uninstall() {
        // An adb that cannot run stands in for a phone that dropped off: nothing may block or
        // read as a clean result (ADR-3210).
        let device = Device {
            adb: PathBuf::from("/nonexistent/regia-android-pass-test/adb"),
            serial: Some("gone".to_owned()),
        };
        assert!(!device.reachable());
        assert!(device.logcat_dump(&["-b", "crash"]).is_none());
        assert!(device.forward_list().is_none());
        let mut summary = String::new();
        let before = Some("gone tcp:5000 tcp:6000\n");
        assert!(uninstall_step(&device, "com.regiaengine.samples.arena", before, &mut summary).is_ok());
        assert!(summary.contains("uninstall: UNKNOWN"), "{summary}");
        assert!(
            summary.contains("forwards: UNKNOWN"),
            "no forward list is not a clean one: {summary}"
        );
    }

    /// ADR-5423: `--uninstall` removes the agent forwards this pass's attaches added on its own
    /// device, and nothing that was there before or belongs to another device.
    #[test]
    fn uninstall_removes_only_the_forwards_this_pass_added_on_its_device() {
        let serial = "54101FDAP0015H";
        let before = "54101FDAP0015H tcp:40000 tcp:1111\nOTHER tcp:41000 tcp:2222\n";
        let after = "54101FDAP0015H tcp:40000 tcp:1111\nOTHER tcp:41000 tcp:2222\n\
                     OTHER tcp:42000 tcp:3333\n54101FDAP0015H tcp:46771 tcp:36975\r\n\
                     54101FDAP0015H tcp:46772 tcp:36975\n";
        assert_eq!(
            added_forwards(before, after, serial),
            vec!["tcp:46771".to_owned(), "tcp:46772".to_owned()]
        );
        assert!(added_forwards(after, after, serial).is_empty(), "nothing added");
        assert!(added_forwards("", "", serial).is_empty());
        assert!(
            added_forwards("", "garbage line\n54101FDAP0015H tcp:1\n", serial).is_empty(),
            "a line that is not `serial local remote` is left alone"
        );
        assert!(
            added_forwards(before, after, "54101FDAP0015").is_empty(),
            "a serial prefix is another device"
        );
    }

    #[test]
    fn thermal_dump_is_parsed() {
        let s = parse_thermal_dump(DUMP);
        assert_eq!(s.status, Some(3));
        assert_eq!(s.skin_c, Some(42.7));
        assert_eq!(s.soc_c, Some(76.0));
    }

    #[test]
    fn only_the_live_section_counts() {
        let dump = "Thermal Status: 1\nCached temperatures:\n\tTemperature{mValue=95.0, mType=0, mName=BIG, mStatus=0}\nHAL Ready: true\nCurrent temperatures from HAL:\n\tTemperature{mValue=48.0, mType=0, mName=BIG, mStatus=0}\n\tTemperature{mValue=41.0, mType=1, mName=G3D, mStatus=0}\n\tTemperature{mValue=29.0, mType=-1, mName=neutral_therm, mStatus=0}\n\tTemperature{mValue=27.9, mType=2, mName=battery, mStatus=0}\nCurrent cooling devices from HAL:\n\tTemperature{mValue=99.0, mType=0, mName=X, mStatus=0}\n";
        let s = parse_thermal_dump(dump);
        assert_eq!(s.soc_c, Some(48.0), "cached and later sections are ignored");
        assert_eq!(s.skin_c, Some(27.9), "no skin sensor: the battery stands in");
    }

    #[test]
    fn the_crash_test_reads_the_record_line() {
        let log = "10-21 13:00:01.000  900  900 W regia_platform: native crash record from the last run: \
                   SIGSEGV (code 1) at 0x10 on thread regia-crasher (tid 931), record /data/user/0/x/files/crash/a.crash.bin, \
                   report none\n\
                   10-21 13:00:01.200  900  900 W regia_framework: native crash bundle from the last run's record: /x.zip\n";
        let check = check_crash_log(log);
        assert!(check.passed(), "{check:?}");
        assert!(check.bundle_line.is_some());
        assert!(!check_crash_log("").passed());
        let wrong_thread = log.replace("regia-crasher", "main");
        assert!(!check_crash_log(&wrong_thread).passed());
        assert_eq!(
            regia_lines("I ActivityManager: start\nW regia_platform: x\nE libc: y\n"),
            "W regia_platform: x\n"
        );
    }

    #[test]
    fn empty_dump_gives_nothing() {
        assert_eq!(parse_thermal_dump(""), ThermalSample::default());
    }

    #[test]
    fn row_has_one_cell_per_header_column() {
        let report = json!({ "p50_ms": 56.9, "p99_ms": 70.2, "gpu": { "p50_ms": 54.1 } });
        let row = thermal_row(420, &report, Some(0.7), &parse_thermal_dump(DUMP), Some(1170.0));
        assert_eq!(row.split('\t').count(), THERMAL_HEADER.split('\t').count());
        assert!(row.starts_with("420\t56.90\t70.20\t54.10\t0.70\t3\t"));
        let blank = thermal_row(0, &json!({}), None, &ThermalSample::default(), None);
        assert_eq!(blank.split('\t').count(), THERMAL_HEADER.split('\t').count());
    }
}
