// SPDX-License-Identifier: MPL-2.0
//! `android-lifecycle`: the automated check for the M1 Android acceptance criterion
//! (`docs/specs/11-platform-and-input.md` § 8): the app survives repeated home/resume cycles
//! and rotation changes with no crash and no PSS growth beyond a tolerance.
//!
//! The APK must already be installed. The tool starts it, streams `adb logcat` on a reader
//! thread, and waits for the engine's lifecycle markers (the `logic` submodule) with timeouts:
//!
//! 1. `logcat -c`, launch, wait for `Resumed` + first frame; one warm-up home/resume cycle.
//! 2. Baseline PSS: median of three `dumpsys meminfo` samples after a settle.
//! 3. N cycles: `KEYCODE_HOME` → `Suspended` → `am start` → `Resumed` + first frame, or with
//!    `--suspend-with screen`: `KEYCODE_SLEEP` → `Suspended` → `KEYCODE_WAKEUP` → `Resumed` +
//!    first frame (a PIN-locked test phone: HOME on the keyguard does not background the app,
//!    screen off does, and debug APKs show over the keyguard on wake).
//! 4. Rotations with auto-rotate off and `wm fixed-to-user-rotation enabled` (so a sensor-locked
//!    activity follows the request), alternating `user_rotation` 1 / 3 starting opposite the
//!    display's actual rotation. Each counts only when display 0's actual rotation
//!    (`dumpsys window displays`) reached the request; then it is confirmed by `WindowResized`,
//!    by a recreation that became ready, or by an uninterrupted run (a 180° turn). An ignored
//!    request fails the run. Device settings are restored even on failure.
//! 5. Final PSS: median of three samples after a settle; leak if it grew past the tolerance.
//!
//! Any crash line attributed to the app, a pid change, or a missing marker fails the run.

pub(crate) mod logic;

use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use self::logic::{
    AdbEnv, Crash, CrashTracker, FixedToUserRotation, Marker, Report, RotationMiss, RotationObservation,
    adb_candidates, choose_device, component, median, parse_devices, parse_display_rotation,
    parse_fixed_to_user_rotation, parse_meminfo_total_pss, parse_pidof, parse_setting, rotation_outcome,
    rotation_sequence, wakefulness_awake,
};

/// How long a cycle waits for `Suspended` after `KEYCODE_SLEEP` before it asks whether the device
/// woke itself up (ADR-3210).
const SLEEP_CHECK: Duration = Duration::from_secs(4);
/// Most times one cycle sends `KEYCODE_SLEEP` again after the system swallowed it.
const SLEEP_RESENDS: u32 = 2;
/// Pause before each set of PSS samples.
const SETTLE: Duration = Duration::from_secs(2);
/// Pause between PSS samples.
const SAMPLE_GAP: Duration = Duration::from_millis(500);
/// How long a rotation may take to produce `WindowResized` before it counts as "continuous".
const RESIZE_WINDOW: Duration = Duration::from_secs(3);
/// Pause after each rotation so consecutive rotations are not coalesced.
const ROTATION_GAP: Duration = Duration::from_millis(500);
/// Logcat lines kept for the diagnostic dump when a run aborts.
const RECENT_LINES: usize = 40;
/// Pause after each settings write when restoring, before reading the values back.
const RESTORE_SETTLE: Duration = Duration::from_millis(500);

/// Command-line options of `cargo xtask android-lifecycle`.
#[derive(clap::Args)]
pub struct Args {
    /// Application id of the installed APK.
    #[arg(long)]
    package: String,
    /// Activity to start: `.Name`, a full class name, or `package/.Name`.
    #[arg(long, default_value = ".RegiaActivity")]
    activity: String,
    /// adb serial of the device; required when several devices are attached.
    #[arg(long)]
    device: Option<String>,
    /// How each cycle backgrounds the app: `home` (HOME key, then `am start`) or `screen`
    /// (screen off, then on; works on a locked phone).
    #[arg(long, value_enum, default_value_t = SuspendWith::Home)]
    suspend_with: SuspendWith,
    /// Home/resume cycles after the warm-up cycle.
    #[arg(long, default_value_t = 50)]
    cycles: u32,
    /// Rotation changes (0 skips the rotation phase).
    #[arg(long, default_value_t = 10)]
    rotations: u32,
    /// Allowed PSS growth as a fraction of the baseline (0.05 = 5 %).
    #[arg(long, default_value_t = 0.05)]
    leak_tolerance: f64,
    /// Seconds to wait for each lifecycle marker.
    #[arg(long, default_value_t = 20)]
    timeout_secs: u64,
    /// Write a JSON report to this path.
    #[arg(long)]
    report: Option<PathBuf>,
}

/// How a cycle sends the app to the background and brings it back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SuspendWith {
    /// `KEYCODE_HOME`, then `am start` (the launcher path).
    Home,
    /// `KEYCODE_SLEEP`, then `KEYCODE_WAKEUP` (screen off/on).
    Screen,
}

/// Runs the check; `Err` only for setup problems (no adb, no device, app not installed).
pub fn run(args: &Args) -> Result<ExitCode> {
    let exe = find_adb()?;
    let serial = choose_device(&list_devices(&exe)?, args.device.as_deref()).map_err(|e| anyhow!(e))?;
    let adb = Adb { exe, serial };
    println!(
        "android-lifecycle: {} on {} (adb: {})",
        args.package,
        adb.serial,
        adb.exe.display()
    );

    let installed = adb.shell_unchecked(&["pm", "path", &args.package])?;
    if !installed.contains("package:") {
        bail!(
            "{} is not installed on {}; install it first (`regia install-android --no-logcat`)",
            args.package,
            adb.serial
        );
    }

    let mut report = Report {
        package: args.package.clone(),
        device: adb.serial.clone(),
        cycles_requested: args.cycles,
        rotations_requested: args.rotations,
        leak_tolerance: args.leak_tolerance,
        ..Report::default()
    };

    // Best effort: a sleeping or locked device never resumes the activity.
    let _ = adb.shell_unchecked(&["input", "keyevent", "KEYCODE_WAKEUP"]);
    let _ = adb.shell_unchecked(&["wm", "dismiss-keyguard"]);
    let mut awake = SettingsGuard::new(adb.clone());
    awake.set("global", "stay_on_while_plugged_in", "7")?;

    adb.shell(&["am", "force-stop", &args.package])?;
    if let Err(err) = adb.run(&["logcat", "-c"]) {
        println!("warning: could not clear logcat ({err:#}); continuing");
    }
    let mut logcat = Logcat::start(&adb, &args.package)?;

    let result = drive(&adb, args, &mut logcat, &mut report);
    if let Err(err) = awake.restore() {
        eprintln!("warning: {err:#}");
    }
    if let Err(err) = result {
        report.aborted = Some(format!("{err:#}"));
        logcat.tail(Duration::from_secs(1));
        eprintln!("last logcat lines:");
        for line in &logcat.recent {
            eprintln!("  {line}");
        }
    }
    report.crash_lines = std::mem::take(&mut logcat.crash_lines);
    report.foreign_crash_lines = std::mem::take(&mut logcat.foreign_crash_lines);
    report.surface_audit = logcat.surface_audit.take();
    drop(logcat);

    report.evaluate();
    for line in report.summary() {
        println!("{line}");
    }
    if let Some(path) = &args.report {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let json = serde_json::to_string_pretty(&report)?;
        std::fs::write(path, json + "\n").with_context(|| format!("writing {}", path.display()))?;
        println!("report written to {}", path.display());
    }
    Ok(if report.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The measured procedure. Fills `report` as it goes, so an early error keeps what was measured.
fn drive(adb: &Adb, args: &Args, logcat: &mut Logcat, report: &mut Report) -> Result<()> {
    let timeout = Duration::from_secs(args.timeout_secs);
    let launch = Launch {
        adb,
        component: component(&args.package, &args.activity),
        timeout,
        suspend_with: args.suspend_with,
    };

    println!("launching {}", launch.component);
    launch.start(true)?;
    logcat.wait_for(
        &[Marker::Resumed, Marker::FirstFrame],
        timeout,
        "the first launch (is the screen unlocked?)",
    )?;
    let pid = adb
        .pid(&args.package)?
        .context("the app is not running after launch")?;
    report.pids.push(pid);
    logcat.tracker.add_app_pid(pid);

    println!("warm-up home/resume cycle");
    let ms = launch.cycle(logcat)?;
    check_pid(adb, &args.package, report, logcat)?;
    println!("  ready {ms} ms after am start");

    logcat.idle(SETTLE)?;
    report.baseline_samples_kb = pss_samples(adb, &args.package, logcat)?;
    report.baseline_pss_kb = median(&report.baseline_samples_kb);
    println!(
        "baseline PSS {} KB (samples {:?})",
        report.baseline_pss_kb.unwrap_or_default(),
        report.baseline_samples_kb
    );

    for i in 1..=args.cycles {
        let ms = launch
            .cycle(logcat)
            .with_context(|| format!("cycle {i}/{}", args.cycles))?;
        check_pid(adb, &args.package, report, logcat)?;
        report.cycles_done = i;
        report.resume_ms.push(ms);
        println!("cycle {i}/{}: ready {ms} ms after am start", args.cycles);
    }

    if args.rotations > 0 {
        rotate(adb, args, logcat, report)?;
    }

    logcat.idle(SETTLE)?;
    check_pid(adb, &args.package, report, logcat)?;
    report.final_samples_kb = pss_samples(adb, &args.package, logcat)?;
    report.final_pss_kb = median(&report.final_samples_kb);
    println!(
        "final PSS {} KB (samples {:?})",
        report.final_pss_kb.unwrap_or_default(),
        report.final_samples_kb
    );
    Ok(())
}

/// The rotation phase. Auto-rotate is turned off and both settings are restored on exit.
fn rotate(adb: &Adb, args: &Args, logcat: &mut Logcat, report: &mut Report) -> Result<()> {
    let timeout = Duration::from_secs(args.timeout_secs);
    // Start from the display's actual rotation (not the user_rotation setting, which a
    // sensor-locked activity ignores), so the first request is a real change.
    let current = display_rotation(adb).context("rotation phase: reading the initial display rotation")?;
    let mut guard = SettingsGuard::new(adb.clone());
    // Pin user_rotation to the current rotation before auto-rotate goes off: where the window
    // manager ignores the app's orientation (a large screen), turning auto-rotate off would
    // otherwise snap the display to the stale user_rotation first, an uncounted rotation.
    guard.set("system", "user_rotation", &current.to_string())?;
    guard.set("system", "accelerometer_rotation", "0")?;
    println!(
        "rotation phase: auto-rotate off (restored afterwards; if interrupted: {})",
        guard.restore_hint()
    );
    // A sensor-locked activity (the arena's `sensorLandscape`) ignores user_rotation unless the
    // display is fixed to the user rotation; restored before the settings (drop order, too).
    let mut fixed = FixedRotationGuard::enable(adb.clone())?;
    if let Some(hint) = fixed.restore_hint() {
        println!("rotation phase: wm fixed-to-user-rotation enabled (if interrupted: {hint})");
    }

    for (i, rotation) in rotation_sequence(args.rotations, Some(current))
        .into_iter()
        .enumerate()
    {
        let n = i + 1;
        logcat.drain()?;
        guard.set("system", "user_rotation", &rotation.to_string())?;
        let start = Instant::now();
        let mut seen = RotationObservation::default();
        while !seen.settled() {
            // Without a teardown, give up waiting for a resize after RESIZE_WINDOW; a teardown
            // must become ready again within the normal timeout.
            let limit = if seen.recreated() { timeout } else { RESIZE_WINDOW };
            match logcat.next_marker(start + limit)? {
                Some(m) => seen.see(m),
                None if seen.recreated() => {
                    bail!(
                        "rotation {n}: the activity was recreated but did not become ready within {timeout:?}"
                    )
                }
                None => break,
            }
        }
        check_pid(adb, &args.package, report, logcat).with_context(|| format!("rotation {n}"))?;
        // A request the window manager ignored is no rotation: it must fail, not be recorded
        // as `Continuous` (B08, 2026-11-08: a sensorLandscape activity kept display rotation 1).
        let actual = display_rotation(adb).ok();
        let confirmation = match rotation_outcome(rotation, actual, &seen) {
            Ok(confirmation) => confirmation,
            Err(RotationMiss::Unreadable) => {
                bail!("rotation {n}: the display rotation could not be read after the request")
            }
            Err(RotationMiss::Ignored { requested, actual }) => bail!(
                "rotation {n}: user_rotation {requested} was requested but display 0 stayed at \
                 rotation {actual}; the request was ignored (sensor-locked activity or orientation \
                 policy), so it is not a rotation"
            ),
        };
        report.rotations.push(confirmation);
        println!(
            "rotation {n}/{}: user_rotation {rotation}, display rotation {rotation}, {confirmation:?}",
            args.rotations,
        );
        logcat.idle(ROTATION_GAP)?;
    }
    fixed.restore()?;
    guard.restore()
}

/// Turns `wm fixed-to-user-rotation` on for the rotation phase and puts the original mode back.
struct FixedRotationGuard {
    adb: Adb,
    /// The mode read before enabling, `None` once restored or when the device has no such mode.
    original: Option<FixedToUserRotation>,
}

impl FixedRotationGuard {
    /// Reads the current mode and enables it. A device without a recognizable mode (older than
    /// Android 10) is left alone with a warning; an ignored rotation then still fails the run.
    fn enable(adb: Adb) -> Result<Self> {
        let raw = adb.shell(&["wm", "fixed-to-user-rotation"])?;
        let Some(original) = parse_fixed_to_user_rotation(&raw) else {
            eprintln!(
                "warning: `wm fixed-to-user-rotation` returned {:?}; rotating without it",
                raw.trim()
            );
            return Ok(Self { adb, original: None });
        };
        let guard = Self {
            adb,
            original: Some(original),
        };
        guard
            .adb
            .shell(&["wm", "fixed-to-user-rotation", FixedToUserRotation::Enabled.arg()])?;
        Ok(guard)
    }

    fn restore_hint(&self) -> Option<String> {
        self.original
            .map(|mode| format!("adb shell wm fixed-to-user-rotation {}", mode.arg()))
    }

    /// Writes the original mode back and reads it back.
    fn restore(&mut self) -> Result<()> {
        let Some(original) = self.original.take() else {
            return Ok(());
        };
        self.adb
            .shell(&["wm", "fixed-to-user-rotation", original.arg()])?;
        let now = parse_fixed_to_user_rotation(&self.adb.shell(&["wm", "fixed-to-user-rotation"])?);
        if now != Some(original) {
            bail!(
                "could not restore wm fixed-to-user-rotation to {} (now {now:?})",
                original.arg()
            );
        }
        Ok(())
    }
}

impl Drop for FixedRotationGuard {
    fn drop(&mut self) {
        if let Err(err) = self.restore() {
            eprintln!("warning: {err:#}");
        }
    }
}

/// The actual rotation of display 0 (`dumpsys window displays`), or an error when it is missing
/// or ambiguous.
fn display_rotation(adb: &Adb) -> Result<u32> {
    let raw = adb.shell(&["dumpsys", "window", "displays"])?;
    parse_display_rotation(&raw).context("dumpsys window displays has no unambiguous mRotation for display 0")
}

/// Three PSS samples of the app, in KB.
fn pss_samples(adb: &Adb, package: &str, logcat: &mut Logcat) -> Result<Vec<u64>> {
    let mut samples = Vec::new();
    for i in 0..3 {
        if i > 0 {
            logcat.idle(SAMPLE_GAP)?;
        }
        let text = adb.shell(&["dumpsys", "meminfo", package])?;
        let kb = parse_meminfo_total_pss(&text).with_context(|| {
            let head: Vec<&str> = text.lines().take(8).collect();
            format!(
                "no TOTAL PSS in `dumpsys meminfo {package}`:\n{}",
                head.join("\n")
            )
        })?;
        samples.push(kb);
    }
    Ok(samples)
}

/// Fails when the app is gone or runs under a new pid.
fn check_pid(adb: &Adb, package: &str, report: &mut Report, logcat: &mut Logcat) -> Result<()> {
    let pid = adb.pid(package)?.context("the app is no longer running")?;
    if report.pids.last() != Some(&pid) {
        let old = report.pids.last().copied().unwrap_or_default();
        report.pids.push(pid);
        logcat.tracker.add_app_pid(pid);
        bail!("the app restarted (pid {old} -> {pid})");
    }
    Ok(())
}

/// Starts the activity the way the launcher does, so `am start` brings the existing task
/// forward instead of stacking a second activity instance.
struct Launch<'a> {
    adb: &'a Adb,
    component: String,
    timeout: Duration,
    suspend_with: SuspendWith,
}

impl Launch<'_> {
    fn start(&self, wait: bool) -> Result<()> {
        let mut args = vec!["am", "start"];
        if wait {
            args.push("-W");
        }
        args.extend([
            "-a",
            "android.intent.action.MAIN",
            "-c",
            "android.intent.category.LAUNCHER",
        ]);
        args.extend(["-n", &self.component]);
        let out = self.adb.shell(&args)?;
        if out.contains("Error") {
            bail!("`am start -n {}` failed: {}", self.component, out.trim());
        }
        Ok(())
    }

    /// After a `KEYCODE_SLEEP`: whether `Suspended` arrived within [`SLEEP_CHECK`]. When it did not
    /// and `dumpsys power` says the device is awake again (the system woke itself and swallowed
    /// the key, `logic::wakefulness_awake`), the key is sent again, up to [`SLEEP_RESENDS`]
    /// times. `false` hands over to the cycle's full wait, which still requires `Suspended`.
    fn resend_swallowed_sleep(&self, logcat: &mut Logcat) -> Result<bool> {
        for attempt in 0..=SLEEP_RESENDS {
            if logcat.wait_marker(Marker::Suspended, SLEEP_CHECK)? {
                return Ok(true);
            }
            let power = self.adb.shell_unchecked(&["dumpsys", "power"])?;
            if attempt == SLEEP_RESENDS || wakefulness_awake(&power) != Some(true) {
                return Ok(false);
            }
            println!("  the device woke itself after KEYCODE_SLEEP (mWakefulness=Awake): sending it again");
            self.adb.shell(&["input", "keyevent", "KEYCODE_SLEEP"])?;
        }
        Ok(false)
    }

    /// One suspend/resume cycle; returns milliseconds from the resume request (`am start` or
    /// `KEYCODE_WAKEUP`) to the first frame.
    fn cycle(&self, logcat: &mut Logcat) -> Result<u64> {
        logcat.drain()?;
        let (away, back) = match self.suspend_with {
            SuspendWith::Home => ("KEYCODE_HOME", "am start"),
            SuspendWith::Screen => ("KEYCODE_SLEEP", "KEYCODE_WAKEUP"),
        };
        self.adb.shell(&["input", "keyevent", away])?;
        let suspended = self.suspend_with == SuspendWith::Screen && self.resend_swallowed_sleep(logcat)?;
        if !suspended {
            logcat.wait_for(
                &[Marker::Suspended],
                self.timeout,
                &format!("suspend after {away}"),
            )?;
        }
        let start = Instant::now();
        match self.suspend_with {
            SuspendWith::Home => self.start(false)?,
            SuspendWith::Screen => {
                self.adb.shell(&["input", "keyevent", "KEYCODE_WAKEUP"])?;
            }
        }
        logcat.wait_for(
            &[Marker::Resumed, Marker::FirstFrame],
            self.timeout,
            &format!("resume after {back}"),
        )?;
        Ok(u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX))
    }
}

/// An `adb` executable bound to one device.
#[derive(Clone)]
struct Adb {
    exe: PathBuf,
    serial: String,
}

impl Adb {
    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.exe);
        cmd.arg("-s").arg(&self.serial);
        cmd
    }

    /// Runs `adb -s <serial> <args>` and returns stdout; fails on a non-zero exit.
    fn run(&self, args: &[&str]) -> Result<String> {
        let out = self
            .command()
            .args(args)
            .output()
            .with_context(|| format!("running adb {}", args.join(" ")))?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!(
                "`adb {}` failed ({}): {} {}",
                args.join(" "),
                out.status,
                stderr.trim(),
                stdout.trim()
            );
        }
        Ok(stdout)
    }

    /// `adb shell <args>`; fails on a non-zero exit.
    fn shell(&self, args: &[&str]) -> Result<String> {
        self.run(&[&["shell"], args].concat())
    }

    /// `adb shell <args>`, returning stdout whatever the exit status (for `pidof`, `pm path`).
    fn shell_unchecked(&self, args: &[&str]) -> Result<String> {
        let out = self
            .command()
            .arg("shell")
            .args(args)
            .output()
            .with_context(|| format!("running adb shell {}", args.join(" ")))?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn pid(&self, package: &str) -> Result<Option<u32>> {
        Ok(parse_pidof(&self.shell_unchecked(&["pidof", package])?))
    }
}

fn find_adb() -> Result<PathBuf> {
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let env = AdbEnv {
        android_home: var("ANDROID_HOME"),
        sdk_root: var("ANDROID_SDK_ROOT"),
        home: var("HOME").or_else(|| var("USERPROFILE")),
        local_app_data: var("LOCALAPPDATA"),
        path: std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default(),
    };
    let candidates = adb_candidates(&env, cfg!(windows));
    candidates.iter().find(|p| p.is_file()).cloned().ok_or_else(|| {
        anyhow!(
            "adb not found. Install the Android SDK platform-tools and set ANDROID_HOME, or put adb on PATH \
             (looked in: {})",
            candidates
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

fn list_devices(exe: &PathBuf) -> Result<Vec<logic::Device>> {
    let out = Command::new(exe)
        .arg("devices")
        .output()
        .with_context(|| format!("running {} devices", exe.display()))?;
    if !out.status.success() {
        bail!(
            "`adb devices` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_devices(&String::from_utf8_lossy(&out.stdout)))
}

/// Device settings changed for the run, restored on [`SettingsGuard::restore`] or drop.
struct SettingsGuard {
    adb: Adb,
    /// `(namespace, key, original value)` in the order first changed.
    saved: Vec<(String, String, Option<String>)>,
}

impl SettingsGuard {
    fn new(adb: Adb) -> Self {
        Self {
            adb,
            saved: Vec::new(),
        }
    }

    fn get(&self, namespace: &str, key: &str) -> Result<Option<String>> {
        Ok(parse_setting(
            &self.adb.shell(&["settings", "get", namespace, key])?,
        ))
    }

    /// Records the current value of `key` so it is restored later, without changing it.
    fn save(&mut self, namespace: &str, key: &str) -> Result<()> {
        if !self.saved.iter().any(|(n, k, _)| n == namespace && k == key) {
            let original = self.get(namespace, key)?;
            self.saved.push((namespace.to_owned(), key.to_owned(), original));
        }
        Ok(())
    }

    fn set(&mut self, namespace: &str, key: &str, value: &str) -> Result<()> {
        self.save(namespace, key)?;
        self.adb.shell(&["settings", "put", namespace, key, value])?;
        Ok(())
    }

    /// The shell commands that restore the saved values, for the user to run by hand.
    fn restore_hint(&self) -> String {
        self.saved
            .iter()
            .rev()
            .map(|(n, k, v)| match v {
                Some(v) => format!("adb shell settings put {n} {k} {v}"),
                None => format!("adb shell settings delete {n} {k}"),
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Writes the original values back, last changed first, then reads them back after a
    /// settle and rewrites any that did not stick. The window manager applies rotation
    /// settings asynchronously; on the android-34 emulator `accelerometer_rotation` was once
    /// left at 0 when it was written right after `user_rotation`.
    fn restore(&mut self) -> Result<()> {
        let mut pending: Vec<_> = self.saved.drain(..).rev().collect();
        let mut errors = Vec::new();
        for _ in 0..3 {
            errors.clear();
            for (namespace, key, original) in &pending {
                let result = match original {
                    Some(v) => self.adb.shell(&["settings", "put", namespace, key, v]),
                    None => self.adb.shell(&["settings", "delete", namespace, key]),
                };
                if let Err(err) = result {
                    errors.push(format!("{err:#}"));
                }
                thread::sleep(RESTORE_SETTLE);
            }
            thread::sleep(RESTORE_SETTLE);
            pending.retain(|(namespace, key, original)| {
                self.get(namespace, key).map_or(true, |now| now != *original)
            });
            if pending.is_empty() {
                return Ok(());
            }
        }
        let wrong: Vec<String> = pending
            .iter()
            .map(|(n, k, v)| format!("{n} {k} should be {}", v.as_deref().unwrap_or("unset")))
            .collect();
        Err(anyhow!(
            "could not restore device settings ({}){}",
            wrong.join(", "),
            if errors.is_empty() {
                String::new()
            } else {
                format!(": {}", errors.join("; "))
            }
        ))
    }
}

impl Drop for SettingsGuard {
    fn drop(&mut self) {
        if !self.saved.is_empty()
            && let Err(err) = self.restore()
        {
            eprintln!("warning: {err:#}");
        }
    }
}

/// `adb logcat -v threadtime` streamed through a reader thread.
struct Logcat {
    child: Child,
    lines: Receiver<String>,
    tracker: CrashTracker,
    crash_lines: Vec<String>,
    foreign_crash_lines: Vec<String>,
    recent: VecDeque<String>,
    /// The last `regia surface audit:` line seen (ADR-527).
    surface_audit: Option<logic::SurfaceAuditLine>,
}

impl Logcat {
    fn start(adb: &Adb, package: &str) -> Result<Self> {
        let mut child = adb
            .command()
            .args(["logcat", "-v", "threadtime"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("starting adb logcat")?;
        let stdout = child.stdout.take().context("adb logcat has no stdout")?;
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&buf).trim_end().to_owned();
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Ok(Self {
            child,
            lines,
            tracker: CrashTracker::new(package),
            crash_lines: Vec::new(),
            foreign_crash_lines: Vec::new(),
            recent: VecDeque::new(),
            surface_audit: None,
        })
    }

    /// Records one line; fails on a crash attributed to the app.
    fn handle(&mut self, line: String) -> Result<Option<Marker>> {
        let found = logic::marker(&line);
        if let Some(audit) = logic::parse_surface_audit(&line) {
            self.surface_audit = Some(audit);
        }
        let crash = self.tracker.check(&line);
        if self.recent.len() == RECENT_LINES {
            self.recent.pop_front();
        }
        self.recent.push_back(line.clone());
        match crash {
            Some(Crash::App) => {
                self.crash_lines.push(line.clone());
                bail!("crash: {line}");
            }
            Some(Crash::Foreign) => self.foreign_crash_lines.push(line),
            None => {}
        }
        Ok(found)
    }

    /// The next marker before `until`, or `None` at the deadline.
    fn next_marker(&mut self, until: Instant) -> Result<Option<Marker>> {
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    if let Some(m) = self.handle(line)? {
                        return Ok(Some(m));
                    }
                }
                Err(RecvTimeoutError::Timeout) => return Ok(None),
                Err(RecvTimeoutError::Disconnected) => bail!("adb logcat stopped (device disconnected?)"),
            }
        }
    }

    /// Whether `wanted` arrives within `within` (other markers are discarded); never fails on a
    /// timeout.
    fn wait_marker(&mut self, wanted: Marker, within: Duration) -> Result<bool> {
        let until = Instant::now() + within;
        while let Some(m) = self.next_marker(until)? {
            if m == wanted {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Waits until every marker in `needed` has been seen.
    fn wait_for(&mut self, needed: &[Marker], timeout: Duration, what: &str) -> Result<()> {
        let until = Instant::now() + timeout;
        let mut seen = HashSet::new();
        while let Some(missing) = needed.iter().find(|m| !seen.contains(*m)) {
            match self.next_marker(until)? {
                Some(m) => {
                    seen.insert(m);
                }
                None => bail!(
                    "timed out after {timeout:?} waiting for `{}` ({what})",
                    missing.text()
                ),
            }
        }
        Ok(())
    }

    /// Processes lines (checking for crashes) for `duration`, discarding markers.
    fn idle(&mut self, duration: Duration) -> Result<()> {
        let until = Instant::now() + duration;
        while self.next_marker(until)?.is_some() {}
        Ok(())
    }

    /// Processes the lines already received, discarding markers.
    fn drain(&mut self) -> Result<()> {
        loop {
            match self.lines.try_recv() {
                Ok(line) => {
                    self.handle(line)?;
                }
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => bail!("adb logcat stopped (device disconnected?)"),
            }
        }
    }

    /// After an abort: collects crash lines for `duration` more, ignoring errors.
    fn tail(&mut self, duration: Duration) {
        let until = Instant::now() + duration;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    let _ = self.handle(line);
                }
                Err(_) => return,
            }
        }
    }
}

impl Drop for Logcat {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
