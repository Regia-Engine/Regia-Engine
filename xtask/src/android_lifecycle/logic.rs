// SPDX-License-Identifier: MPL-2.0
//! Pure parsing and decision logic for `android-lifecycle`: logcat lines, `dumpsys meminfo`,
//! `adb devices`, crash attribution, rotation bookkeeping and the pass/fail verdict.
//!
//! Nothing here touches a device, so every rule is unit-tested on fixture text.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::Serialize;

/// Logged by the engine after every resume, including the first.
pub const MARK_RESUMED: &str = "regia lifecycle: Resumed";
/// Logged by the engine on suspend.
pub const MARK_SUSPENDED: &str = "regia lifecycle: Suspended";
/// Logged by the renderer once per resume when a frame reached the screen (the "ready" signal).
pub const MARK_FIRST_FRAME: &str = "regia render: first frame presented after resume";
/// Logged by the sample on every window resize (rotation confirmation).
pub const MARK_RESIZED: &str = "WindowResized";
/// Logged by the platform layer after every resume (ADR-527): the surface audit's counters.
pub const MARK_SURFACE_AUDIT: &str = "regia surface audit:";

/// The counters of one `regia surface audit:` line (`regia_platform::SurfaceAudit`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SurfaceAuditLine {
    /// Suspends seen by the app.
    pub suspends: u64,
    /// Resumes seen by the app.
    pub resumes: u64,
    /// Surfaces created since start.
    pub created: u64,
    /// Surfaces dropped since start.
    pub dropped: u64,
    /// Surfaces alive after the resume.
    pub live: u64,
    /// Windows that exist.
    pub windows: u64,
    /// Broken surface rules so far (surface kept while suspended, missing after resume, counts
    /// that do not add up).
    pub violations: u64,
}

/// Parses a `regia surface audit: suspends=… resumes=… created=… dropped=… live=… windows=…
/// violations=…` line (any logcat prefix). Unknown keys are ignored; a line missing a key is not
/// an audit line.
pub fn parse_surface_audit(line: &str) -> Option<SurfaceAuditLine> {
    let (_, rest) = line.split_once(MARK_SURFACE_AUDIT)?;
    let mut fields = std::collections::HashMap::new();
    for pair in rest.split_whitespace() {
        if let Some((key, value)) = pair.split_once('=') {
            fields.insert(key, value.parse::<u64>().ok()?);
        }
    }
    let get = |key: &str| fields.get(key).copied();
    Some(SurfaceAuditLine {
        suspends: get("suspends")?,
        resumes: get("resumes")?,
        created: get("created")?,
        dropped: get("dropped")?,
        live: get("live")?,
        windows: get("windows")?,
        violations: get("violations")?,
    })
}

/// A lifecycle marker found in a logcat line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Marker {
    /// `regia lifecycle: Resumed`.
    Resumed,
    /// `regia lifecycle: Suspended`.
    Suspended,
    /// `regia render: first frame presented after resume`.
    FirstFrame,
    /// `WindowResized`.
    Resized,
}

impl Marker {
    /// The substring that identifies this marker in a logcat line.
    pub fn text(self) -> &'static str {
        match self {
            Marker::Resumed => MARK_RESUMED,
            Marker::Suspended => MARK_SUSPENDED,
            Marker::FirstFrame => MARK_FIRST_FRAME,
            Marker::Resized => MARK_RESIZED,
        }
    }
}

/// Finds the lifecycle marker in a raw logcat line (any tag, any format).
pub fn marker(line: &str) -> Option<Marker> {
    [
        Marker::Resumed,
        Marker::Suspended,
        Marker::FirstFrame,
        Marker::Resized,
    ]
    .into_iter()
    .find(|m| line.contains(m.text()))
}

/// The fields of one logcat line that crash attribution needs.
#[derive(Debug, PartialEq, Eq)]
pub struct LogLine<'a> {
    /// Process id that wrote the line, when the format carries one.
    pub pid: Option<u32>,
    /// Priority letter (`V D I W E F A`), when the format carries one.
    pub priority: Option<char>,
    /// Log tag, trimmed; empty when the line could not be parsed.
    pub tag: &'a str,
    /// Message text; the whole line when it could not be parsed.
    pub message: &'a str,
}

/// Parses a `logcat -v threadtime` or `-v brief` line. Unrecognised lines (such as
/// `--------- beginning of main`) come back with no pid, no priority and an empty tag.
pub fn parse_log_line(line: &str) -> LogLine<'_> {
    parse_threadtime(line)
        .or_else(|| parse_brief(line))
        .unwrap_or(LogLine {
            pid: None,
            priority: None,
            tag: "",
            message: line,
        })
}

/// `10-04 12:34:56.789  1234  5678 I Tag     : message`
fn parse_threadtime(line: &str) -> Option<LogLine<'_>> {
    let mut rest = line;
    let mut fields = [""; 5];
    for field in &mut fields {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace)?;
        *field = &rest[..end];
        rest = &rest[end..];
    }
    let [date, time, pid, _tid, priority] = fields;
    if !date.contains('-') || !time.contains(':') {
        return None;
    }
    let pid = pid.parse().ok()?;
    let priority = single_char(priority)?;
    let (tag, message) = rest
        .split_once(": ")
        .or_else(|| rest.trim_end().strip_suffix(':').map(|t| (t, "")))?;
    Some(LogLine {
        pid: Some(pid),
        priority: Some(priority),
        tag: tag.trim(),
        message,
    })
}

/// `I/Tag     ( 1234): message`
fn parse_brief(line: &str) -> Option<LogLine<'_>> {
    let (priority, rest) = line.split_once('/')?;
    let priority = single_char(priority)?;
    let open = rest.find('(')?;
    let close = open + rest[open..].find("):")?;
    let pid = rest[open + 1..close].trim().parse().ok()?;
    let message = rest[close + 2..].strip_prefix(' ').unwrap_or(&rest[close + 2..]);
    Some(LogLine {
        pid: Some(pid),
        priority: Some(priority),
        tag: rest[..open].trim(),
        message,
    })
}

fn single_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let c = chars.next()?;
    (chars.next().is_none() && "VDIWEFAS".contains(c)).then_some(c)
}

/// What a crash signal line means for the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Crash {
    /// Written by, or naming, the app under test: the run fails.
    App,
    /// A crash in some other process on the device: reported, not failed.
    Foreign,
}

/// Recognises crash signals in logcat and attributes them to the app under test or to
/// another process.
///
/// Signals: `FATAL EXCEPTION`, `Fatal signal`, priority `F` with tag `DEBUG` (tombstone),
/// tag `RustPanic`, and `ANR in <package>`. A signal belongs to the app when it carries the
/// tag `RustPanic`, is the app's ANR, comes from one of the app's pids, or names the package.
/// A tombstone header naming the package also claims the rest of that tombstone (same pid).
#[derive(Debug)]
pub struct CrashTracker {
    package: String,
    app_pids: HashSet<u32>,
    tombstone_pids: HashSet<u32>,
}

impl CrashTracker {
    /// A tracker for `package`, with no known pids yet.
    pub fn new(package: &str) -> Self {
        Self {
            package: package.to_owned(),
            app_pids: HashSet::new(),
            tombstone_pids: HashSet::new(),
        }
    }

    /// Records a pid the app has run under.
    pub fn add_app_pid(&mut self, pid: u32) {
        self.app_pids.insert(pid);
    }

    /// Classifies one raw logcat line; `None` when it is not a crash signal.
    pub fn check(&mut self, line: &str) -> Option<Crash> {
        let parsed = parse_log_line(line);
        let tombstone = (parsed.priority == Some('F') && parsed.tag == "DEBUG")
            || line.contains(" F DEBUG")
            || line.contains("F/DEBUG");
        let panic = parsed.tag == "RustPanic";
        let anr = line.contains(&format!("ANR in {}", self.package));
        let signal =
            tombstone || panic || anr || line.contains("FATAL EXCEPTION") || line.contains("Fatal signal");
        if !signal {
            return None;
        }
        let names_app = line.contains(&self.package);
        let from_app = parsed.pid.is_some_and(|p| self.app_pids.contains(&p));
        let continues_tombstone = tombstone && parsed.pid.is_some_and(|p| self.tombstone_pids.contains(&p));
        if tombstone
            && names_app
            && let Some(pid) = parsed.pid
        {
            self.tombstone_pids.insert(pid);
        }
        Some(if panic || anr || names_app || from_app || continues_tombstone {
            Crash::App
        } else {
            Crash::Foreign
        })
    }
}

/// Total PSS in KB from `dumpsys meminfo <package>` output, or `None` when the output has
/// none (for example `No process found for: ...`).
///
/// Prefers the `TOTAL PSS:` summary line (Android 11+), then the first column of the
/// table's `TOTAL` row (all versions), then the old summary `TOTAL:` line (Android 10).
/// Values are summed across `** MEMINFO in pid ... **` sections when several processes match.
pub fn parse_meminfo_total_pss(text: &str) -> Option<u64> {
    let summary_pss = sum_after(text, |l| l.strip_prefix("TOTAL PSS:"));
    let table = sum_after(text, |l| {
        l.strip_prefix("TOTAL")
            .filter(|r| r.starts_with(char::is_whitespace) && first_number(r).is_some())
    });
    let old_summary = sum_after(text, |l| l.strip_prefix("TOTAL:"));
    summary_pss.or(table).or(old_summary)
}

fn sum_after<'a>(text: &'a str, rest: impl Fn(&'a str) -> Option<&'a str>) -> Option<u64> {
    text.lines()
        .filter_map(|l| rest(l.trim_start()).and_then(first_number))
        .reduce(|a, b| a + b)
}

fn first_number(s: &str) -> Option<u64> {
    s.split_whitespace().next()?.parse().ok()
}

/// The first pid printed by `pidof <package>`; `None` when the app is not running.
pub fn parse_pidof(text: &str) -> Option<u32> {
    text.split_whitespace().next()?.parse().ok()
}

/// One row of `adb devices`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// Serial, as passed to `adb -s`.
    pub serial: String,
    /// `device`, `unauthorized`, `offline`, ...
    pub state: String,
}

/// Parses `adb devices` (with or without `-l`).
pub fn parse_devices(text: &str) -> Vec<Device> {
    text.lines()
        .filter(|l| !l.starts_with('*') && !l.starts_with("List of devices"))
        .filter_map(|l| {
            let mut words = l.split_whitespace();
            Some(Device {
                serial: words.next()?.to_owned(),
                state: words.next()?.to_owned(),
            })
        })
        .collect()
}

/// Picks the device to test: the requested one, or the only attached one.
/// The error text is shown to the user as is.
pub fn choose_device(devices: &[Device], requested: Option<&str>) -> Result<String, String> {
    let usable = |d: &Device| match d.state.as_str() {
        "device" => Ok(d.serial.clone()),
        "unauthorized" => Err(format!(
            "device {} is unauthorized: unlock it and accept the USB debugging prompt",
            d.serial
        )),
        other => Err(format!(
            "device {} is {other}; reconnect it or restart adb (`adb kill-server`)",
            d.serial
        )),
    };
    let list = || {
        devices
            .iter()
            .map(|d| format!("{} ({})", d.serial, d.state))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match requested {
        Some(serial) => match devices.iter().find(|d| d.serial == serial) {
            Some(d) => usable(d),
            None if devices.is_empty() => Err(format!(
                "device {serial} is not attached: `adb devices` lists no devices"
            )),
            None => Err(format!("device {serial} is not attached; attached: {}", list())),
        },
        None => match devices {
            [] => Err(
                "no Android device attached: `adb devices` lists none. Connect a device with USB \
                       debugging enabled or start an emulator (`emulator -avd <name>`)"
                    .into(),
            ),
            [only] => usable(only),
            _ => Err(format!(
                "several devices attached ({}); choose one with --device <serial>",
                list()
            )),
        },
    }
}

/// Where to look for `adb`, in order: `$ANDROID_HOME`, `$ANDROID_SDK_ROOT`, the default SDK
/// location under the home directory (`~/Android/Sdk`, `%LOCALAPPDATA%\Android\Sdk`), then
/// every `PATH` entry. The SDK copy comes before `PATH` so the tool talks to the same adb
/// server as the rest of the Android tooling.
pub fn adb_candidates(env: &AdbEnv, windows: bool) -> Vec<PathBuf> {
    let exe = if windows { "adb.exe" } else { "adb" };
    let mut sdks: Vec<PathBuf> = Vec::new();
    sdks.extend(env.android_home.iter().cloned());
    sdks.extend(env.sdk_root.iter().cloned());
    sdks.extend(env.home.iter().map(|h| h.join("Android").join("Sdk")));
    sdks.extend(env.local_app_data.iter().map(|d| d.join("Android").join("Sdk")));
    let mut out: Vec<PathBuf> = sdks
        .into_iter()
        .map(|s| s.join("platform-tools").join(exe))
        .collect();
    out.extend(env.path.iter().map(|d| d.join(exe)));
    out
}

/// The environment `adb_candidates` reads, captured so it can be tested.
#[derive(Debug, Default)]
pub struct AdbEnv {
    /// `$ANDROID_HOME`.
    pub android_home: Option<PathBuf>,
    /// `$ANDROID_SDK_ROOT`.
    pub sdk_root: Option<PathBuf>,
    /// `$HOME` (or `%USERPROFILE%`).
    pub home: Option<PathBuf>,
    /// `%LOCALAPPDATA%` (Windows).
    pub local_app_data: Option<PathBuf>,
    /// `$PATH`, split.
    pub path: Vec<PathBuf>,
}

/// `am start -n` component for `package` and `activity` (`.Foo`, `com.x.Foo` or `pkg/.Foo`).
pub fn component(package: &str, activity: &str) -> String {
    if activity.contains('/') {
        activity.to_owned()
    } else {
        format!("{package}/{activity}")
    }
}

/// The value printed by `settings get`, or `None` for an unset key (`null`).
pub fn parse_setting(text: &str) -> Option<String> {
    let v = text.trim();
    (!v.is_empty() && v != "null").then(|| v.to_owned())
}

/// Whether `dumpsys power` says the device is awake (`mWakefulness=Awake`): after a
/// `KEYCODE_SLEEP` that means the system woke itself back up and swallowed the key (a Pixel 6 on
/// Android 17 did so within 11 ms, `WAKE_REASON_DOZE_STOPPED`, ADR-3210), so the app was right not
/// to suspend and the key must be sent again. `None` when the dump has no such line.
pub fn wakefulness_awake(dumpsys_power: &str) -> Option<bool> {
    dumpsys_power.lines().find_map(|line| {
        let value = line.trim().strip_prefix("mWakefulness=")?;
        Some(value.trim() == "Awake")
    })
}

/// Median of `values` (lower middle for an even count); `None` when empty.
pub fn median(values: &[u64]) -> Option<u64> {
    let mut v = values.to_vec();
    v.sort_unstable();
    v.get(v.len().saturating_sub(1) / 2).copied()
}

/// Change from `baseline` to `final_kb` in percent, and whether it exceeds `tolerance`
/// (a fraction: 0.05 = 5 %). Only growth counts as a leak.
pub fn leak_verdict(baseline: u64, final_kb: u64, tolerance: f64) -> (f64, bool) {
    if baseline == 0 {
        return (0.0, final_kb > 0);
    }
    let delta = (final_kb as f64 - baseline as f64) / baseline as f64;
    (
        delta * 100.0,
        final_kb as f64 > baseline as f64 * (1.0 + tolerance),
    )
}

/// `user_rotation` values for `count` rotations, alternating landscape (1) and reverse
/// landscape (3) and starting with whichever differs from the current value, so the first
/// step is a real change.
pub fn rotation_sequence(count: u32, current: Option<u32>) -> Vec<u32> {
    let first = if current == Some(1) { 3 } else { 1 };
    (0..count)
        .map(|i| if i % 2 == 0 { first } else { 4 - first })
        .collect()
}

/// The actual rotation (0-3) of display 0 in `dumpsys window displays` output: the
/// `mRotation=` line of the `Display: mDisplayId=0` section, which is the rotation the window
/// manager applied, not the requested `mUserRotation`. `None` when display 0 is missing, has no
/// such line, or lists two different values (ambiguous): the caller must then fail, not guess.
pub fn parse_display_rotation(dumpsys_window_displays: &str) -> Option<u32> {
    let mut in_display_0 = false;
    let mut found: Option<u32> = None;
    for line in dumpsys_window_displays.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Display: mDisplayId=") {
            let id: String = rest.chars().take_while(char::is_ascii_digit).collect();
            in_display_0 = id == "0";
            continue;
        }
        if !in_display_0 {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("mRotation=") else {
            continue;
        };
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let value = digits.parse::<u32>().ok().filter(|v| *v <= 3)?;
        match found {
            Some(previous) if previous != value => return None,
            _ => found = Some(value),
        }
    }
    found
}

/// A `wm fixed-to-user-rotation` mode (Android 10+): whether display rotation follows
/// `user_rotation` regardless of the activity's requested orientation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixedToUserRotation {
    /// The device default.
    Default,
    /// Always follow `user_rotation` (what the rotation phase needs).
    Enabled,
    /// Never fixed.
    Disabled,
    /// Fixed only while auto-rotate is off.
    EnabledIfNoAutoRotation,
}

impl FixedToUserRotation {
    /// The `wm fixed-to-user-rotation` argument that sets this mode.
    pub fn arg(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::EnabledIfNoAutoRotation => "enabled_if_no_auto_rotation",
        }
    }
}

/// The mode printed by `wm fixed-to-user-rotation` (no argument), or `None` for any other
/// output (an older device prints usage or an error): the caller must not guess a mode to restore.
pub fn parse_fixed_to_user_rotation(raw: &str) -> Option<FixedToUserRotation> {
    match raw.trim() {
        "default" => Some(FixedToUserRotation::Default),
        "enabled" => Some(FixedToUserRotation::Enabled),
        "disabled" => Some(FixedToUserRotation::Disabled),
        "enabled_if_no_auto_rotation" => Some(FixedToUserRotation::EnabledIfNoAutoRotation),
        _ => None,
    }
}

/// Why one rotation request did not count as a rotation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RotationMiss {
    /// The display rotation could not be read after the request.
    Unreadable,
    /// The display stayed at (or moved to) another rotation than requested: the request was
    /// ignored (a sensor-locked activity, an orientation policy), so no rotation happened.
    Ignored {
        /// The requested `user_rotation`.
        requested: u32,
        /// The rotation display 0 actually reports.
        actual: u32,
    },
}

/// The confirmation of one rotation request, or why it does not count: the display's actual
/// rotation must equal the request before the markers (`seen`) may confirm it.
pub fn rotation_outcome(
    requested: u32,
    actual: Option<u32>,
    seen: &RotationObservation,
) -> Result<RotationConfirmation, RotationMiss> {
    match actual {
        None => Err(RotationMiss::Unreadable),
        Some(actual) if actual != requested => Err(RotationMiss::Ignored { requested, actual }),
        Some(_) => Ok(seen.confirmation()),
    }
}

/// Markers seen while waiting out one rotation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RotationObservation {
    /// A `WindowResized` arrived.
    pub resized: bool,
    /// A `Suspended` arrived (the activity was torn down).
    pub suspended: bool,
    /// A `Resumed` arrived.
    pub resumed: bool,
    /// A first frame after resume arrived.
    pub first_frame: bool,
}

impl RotationObservation {
    /// Records one marker.
    pub fn see(&mut self, m: Marker) {
        match m {
            Marker::Resized => self.resized = true,
            Marker::Suspended => self.suspended = true,
            Marker::Resumed => self.resumed = true,
            Marker::FirstFrame => self.first_frame = true,
        }
    }

    /// The activity went through suspend/resume, so the run must also see it become ready.
    pub fn recreated(&self) -> bool {
        self.suspended || self.resumed
    }

    /// The rotation is over: resized in place, or recreated and ready again.
    pub fn settled(&self) -> bool {
        if self.recreated() {
            self.resumed && self.first_frame
        } else {
            self.resized
        }
    }

    /// How the rotation was confirmed.
    pub fn confirmation(&self) -> RotationConfirmation {
        if self.recreated() {
            RotationConfirmation::Recreated
        } else if self.resized {
            RotationConfirmation::Resized
        } else {
            RotationConfirmation::Continuous
        }
    }
}

/// How one rotation was confirmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RotationConfirmation {
    /// The sample logged `WindowResized`.
    Resized,
    /// No resize, and no suspend/resume: the app ran on uninterrupted (a 180° turn keeps the
    /// window size, so this is the expected result for sensor-landscape apps).
    Continuous,
    /// The activity was suspended and resumed (recreated) and became ready again.
    Recreated,
}

/// Everything the run measured; serialised as the `--report` JSON.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    /// Application id under test.
    pub package: String,
    /// adb serial of the device.
    pub device: String,
    /// Home/resume cycles requested.
    pub cycles_requested: u32,
    /// Home/resume cycles completed.
    pub cycles_done: u32,
    /// Milliseconds from `am start` to the first frame, per completed cycle.
    pub resume_ms: Vec<u64>,
    /// Rotations requested.
    pub rotations_requested: u32,
    /// How each completed rotation was confirmed.
    pub rotations: Vec<RotationConfirmation>,
    /// Allowed PSS growth as a fraction.
    pub leak_tolerance: f64,
    /// Baseline PSS samples in KB.
    pub baseline_samples_kb: Vec<u64>,
    /// Median baseline PSS in KB.
    pub baseline_pss_kb: Option<u64>,
    /// Final PSS samples in KB.
    pub final_samples_kb: Vec<u64>,
    /// Median final PSS in KB.
    pub final_pss_kb: Option<u64>,
    /// PSS change in percent.
    pub delta_percent: Option<f64>,
    /// Every pid the app ran under (more than one means it restarted).
    pub pids: Vec<u32>,
    /// Crash signal lines attributed to the app.
    pub crash_lines: Vec<String>,
    /// Crash signal lines from other processes (reported, not failed).
    pub foreign_crash_lines: Vec<String>,
    /// The last `regia surface audit:` line (`None` from engine builds before ADR-527).
    pub surface_audit: Option<SurfaceAuditLine>,
    /// Why the run stopped early, if it did.
    pub aborted: Option<String>,
    /// The verdict's reasons; empty means PASS.
    pub failures: Vec<String>,
    /// `pass` or `fail`.
    pub verdict: String,
}

impl Report {
    /// Fills `delta_percent`, `failures` and `verdict` from the measurements.
    pub fn evaluate(&mut self) {
        let mut failures = Vec::new();
        if let Some(reason) = &self.aborted {
            failures.push(format!("run aborted: {reason}"));
        }
        if !self.crash_lines.is_empty() {
            failures.push(format!("{} crash line(s) from the app", self.crash_lines.len()));
        }
        if self.pids.len() > 1 {
            failures.push(format!("app restarted: pids {:?}", self.pids));
        }
        if let Some(audit) = self.surface_audit.filter(|a| a.violations > 0) {
            failures.push(format!(
                "surface audit: {} violation(s) (created {}, dropped {}, live {}, windows {}); see `surface audit:` warnings in logcat",
                audit.violations, audit.created, audit.dropped, audit.live, audit.windows
            ));
        }
        if self.cycles_done < self.cycles_requested {
            failures.push(format!(
                "{}/{} home/resume cycles completed",
                self.cycles_done, self.cycles_requested
            ));
        }
        if (self.rotations.len() as u32) < self.rotations_requested {
            failures.push(format!(
                "{}/{} rotations completed",
                self.rotations.len(),
                self.rotations_requested
            ));
        }
        self.delta_percent = None;
        match (self.baseline_pss_kb, self.final_pss_kb) {
            (Some(base), Some(fin)) => {
                let (delta, leaked) = leak_verdict(base, fin, self.leak_tolerance);
                self.delta_percent = Some(delta);
                if leaked {
                    failures.push(format!(
                        "PSS grew {delta:+.2} % ({base} KB -> {fin} KB), over the {:.2} % tolerance",
                        self.leak_tolerance * 100.0
                    ));
                }
            }
            _ => failures.push("no PSS measurement pair (baseline and final)".into()),
        }
        self.verdict = if failures.is_empty() { "pass" } else { "fail" }.into();
        self.failures = failures;
    }

    /// Number of rotations confirmed in the given way.
    pub fn rotations_by(&self, kind: RotationConfirmation) -> usize {
        self.rotations.iter().filter(|r| **r == kind).count()
    }

    /// The human-readable summary printed at the end of a run.
    pub fn summary(&self) -> Vec<String> {
        let kb = |v: Option<u64>| v.map_or_else(|| "n/a".to_owned(), |v| format!("{v} KB"));
        let mut lines = vec![format!("android-lifecycle: {}", self.verdict.to_uppercase())];
        lines.push(format!("  device       {} ({})", self.device, self.package));
        let mut cycles = format!("  cycles       {}/{}", self.cycles_done, self.cycles_requested);
        let mut sorted = self.resume_ms.clone();
        sorted.sort_unstable();
        if let (Some(min), Some(med), Some(max)) = (sorted.first(), median(&sorted), sorted.last()) {
            cycles.push_str(&format!(
                " (start to first frame: min {min} ms, median {med} ms, max {max} ms)"
            ));
        }
        lines.push(cycles);
        lines.push(format!(
            "  rotations    {}/{} (resized {}, continuous {}, recreated {})",
            self.rotations.len(),
            self.rotations_requested,
            self.rotations_by(RotationConfirmation::Resized),
            self.rotations_by(RotationConfirmation::Continuous),
            self.rotations_by(RotationConfirmation::Recreated),
        ));
        lines.push(format!(
            "  PSS baseline {} (samples {:?})",
            kb(self.baseline_pss_kb),
            self.baseline_samples_kb
        ));
        lines.push(format!(
            "  PSS final    {} (samples {:?})",
            kb(self.final_pss_kb),
            self.final_samples_kb
        ));
        if let Some(delta) = self.delta_percent {
            lines.push(format!(
                "  PSS delta    {delta:+.2} % (tolerance {:.2} %)",
                self.leak_tolerance * 100.0
            ));
        }
        lines.push(format!(
            "  crash lines  {} from the app, {} from other processes (ignored)",
            self.crash_lines.len(),
            self.foreign_crash_lines.len()
        ));
        lines.extend(self.crash_lines.iter().map(|l| format!("    {l}")));
        lines.push(match &self.surface_audit {
            Some(a) => format!(
                "  surfaces     created {}, dropped {}, live {}/{} windows, {} violation(s) over {} resumes",
                a.created, a.dropped, a.live, a.windows, a.violations, a.resumes
            ),
            None => "  surfaces     no surface audit line (engine before ADR-527)".to_owned(),
        });
        lines.extend(self.failures.iter().map(|f| format!("  FAIL: {f}")));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wakefulness_reads_the_power_dump() {
        let awake = "POWER MANAGER (dumpsys power)\n  mWakefulness=Awake\n  mWakefulnessChanging=false\n";
        let dozing = "  mWakefulness=Dozing\n  mWakefulnessChanging=false\n";
        let asleep = "  mWakefulness=Asleep\n";
        assert_eq!(wakefulness_awake(awake), Some(true));
        assert_eq!(wakefulness_awake(dozing), Some(false));
        assert_eq!(wakefulness_awake(asleep), Some(false));
        assert_eq!(wakefulness_awake("no such line\n"), None);
        // `mWakefulnessChanging` must not be read as the state.
        assert_eq!(wakefulness_awake("  mWakefulnessChanging=true\n"), None);
    }

    const PKG: &str = "com.regiaengine.hello_window";

    // Android 10: no `TOTAL PSS:` line; summary says `TOTAL:`.
    const MEMINFO_ANDROID_10: &str = "\
Applications Memory Usage (in Kilobytes):
Uptime: 1032861 Realtime: 1032861

** MEMINFO in pid 3367 [com.regiaengine.hello_window] **
                   Pss  Private  Private  SwapPss     Heap     Heap     Heap
                 Total    Dirty    Clean    Dirty     Size    Alloc     Free
                ------   ------   ------   ------   ------   ------   ------
  Native Heap     6012     5960        0        0    13312     9254     4057
  Dalvik Heap     2464     2428        0        0     5262     2631     2631
 Dalvik Other     1006     1004        0        0
        Stack       44       44        0        0
    Other dev       12        0       12        0
     .so mmap     2963      164      296        0
    GL mtrack     3072     3072        0        0
      Unknown      460      456        0        0
        TOTAL    37858    20068    11676        0    18574    11885     6688

 App Summary
                       Pss(KB)
                        ------
           Java Heap:     7956
         Native Heap:     5960
                Code:    11812
               Stack:       44
            Graphics:     4500
       Private Other:     1472
              System:     6114

               TOTAL:    37858       TOTAL SWAP PSS:        0

 Objects
               Views:        0         ViewRootImpl:        0
         AppContexts:        3           Activities:        1
";

    // Android 13-15: Rss columns, and a `TOTAL PSS:` summary line.
    const MEMINFO_ANDROID_14: &str = "\
Applications Memory Usage (in Kilobytes):
Uptime: 5120934 Realtime: 5120934

** MEMINFO in pid 8012 [com.regiaengine.hello_window] **
                   Pss  Private  Private  SwapPss      Rss     Heap     Heap     Heap
                 Total    Dirty    Clean    Dirty    Total     Size    Alloc     Free
                ------   ------   ------   ------   ------   ------   ------   ------
  Native Heap    11873    11800        0       12    13624    18944    12603     2214
  Dalvik Heap     1585     1460        0        8     8364     3496     1748     1748
        Stack      560      560        0        0      568
      Gfx dev     6236     6236        0        0     6236
    GL mtrack    19020    19020        0        0    19020
      Unknown      780      776        0        3     1208
        TOTAL    41817    39852      844       23   124424    22440    14351     3962

 App Summary
                       Pss(KB)                        Rss(KB)
                        ------                         ------
           Java Heap:     2652                          21184
         Native Heap:    11800                          13624
                Code:      876                          46608
               Stack:      560                            568
            Graphics:    25256                          25256
       Private Other:      552
              System:      121
             Unknown:                                    17184

           TOTAL PSS:    41817            TOTAL RSS:   124424       TOTAL SWAP PSS:       23

 Objects
               Views:        0         ViewRootImpl:        0
";

    #[test]
    fn meminfo_android_10_format() {
        assert_eq!(parse_meminfo_total_pss(MEMINFO_ANDROID_10), Some(37858));
    }

    #[test]
    fn meminfo_android_14_format() {
        assert_eq!(parse_meminfo_total_pss(MEMINFO_ANDROID_14), Some(41817));
    }

    #[test]
    fn meminfo_summary_line_wins_over_table() {
        let text = MEMINFO_ANDROID_14.replace("TOTAL PSS:    41817", "TOTAL PSS:    41000");
        assert_eq!(parse_meminfo_total_pss(&text), Some(41000));
    }

    #[test]
    fn meminfo_table_only() {
        let table_only = "        TOTAL    5000    4000    0\n";
        assert_eq!(parse_meminfo_total_pss(table_only), Some(5000));
    }

    #[test]
    fn meminfo_sums_processes() {
        let two = format!("{MEMINFO_ANDROID_14}\n{MEMINFO_ANDROID_14}");
        assert_eq!(parse_meminfo_total_pss(&two), Some(2 * 41817));
    }

    #[test]
    fn meminfo_no_process() {
        let text =
            "Applications Memory Usage (in Kilobytes):\nUptime: 1 Realtime: 1\nNo process found for: com.x\n";
        assert_eq!(parse_meminfo_total_pss(text), None);
        assert_eq!(parse_meminfo_total_pss(""), None);
    }

    #[test]
    fn markers_match_any_tag_and_format() {
        let tt = "10-04 12:34:56.789  8012  8040 I regia : regia lifecycle: Resumed";
        assert_eq!(marker(tt), Some(Marker::Resumed));
        assert_eq!(
            marker("I/test    ( 4242): regia lifecycle: Suspended"),
            Some(Marker::Suspended)
        );
        assert_eq!(
            marker(
                "10-04 12:34:56.900  8012  8041 D render  : regia render: first frame presented after resume"
            ),
            Some(Marker::FirstFrame)
        );
        assert_eq!(
            marker("I/hello_window( 1): event: WindowResized { width: 2400, height: 1080 }"),
            Some(Marker::Resized)
        );
        assert_eq!(
            marker("10-04 12:34:56.789  8012  8040 I regia : regia lifecycle: Started"),
            None
        );
    }

    #[test]
    fn parses_threadtime_and_brief_lines() {
        let tt = parse_log_line("10-04 12:34:56.789  8012  8040 F DEBUG   : pid: 8012, tid: 8040");
        assert_eq!(
            tt,
            LogLine {
                pid: Some(8012),
                priority: Some('F'),
                tag: "DEBUG",
                message: "pid: 8012, tid: 8040"
            }
        );
        let brief = parse_log_line("E/RustPanic( 8012): panicked at src/lib.rs:1:1");
        assert_eq!(
            brief,
            LogLine {
                pid: Some(8012),
                priority: Some('E'),
                tag: "RustPanic",
                message: "panicked at src/lib.rs:1:1"
            }
        );
        let other = parse_log_line("--------- beginning of main");
        assert_eq!(
            other,
            LogLine {
                pid: None,
                priority: None,
                tag: "",
                message: "--------- beginning of main"
            }
        );
    }

    #[test]
    fn crash_signals_from_the_app_fail() {
        let mut t = CrashTracker::new(PKG);
        t.add_app_pid(8012);
        assert_eq!(
            t.check("10-04 12:00:00.000  8012  8012 E AndroidRuntime: FATAL EXCEPTION: main"),
            Some(Crash::App)
        );
        assert_eq!(
            t.check("10-04 12:00:00.000  8012  8040 F libc    : Fatal signal 11 (SIGSEGV), code 1, fault addr 0x0 in tid 8040 (GameThread), pid 8012 (o.hello_window)"),
            Some(Crash::App)
        );
        assert_eq!(
            t.check("10-04 12:00:00.000  8012  8040 E RustPanic: panicked at 'boom'"),
            Some(Crash::App)
        );
        assert_eq!(
            t.check("E/RustPanic( 9999): panicked at 'boom'"),
            Some(Crash::App)
        );
        assert_eq!(
            t.check(&format!(
                "10-04 12:00:00.000   545   600 E ActivityManager: ANR in {PKG} ({PKG}/.RegiaActivity)"
            )),
            Some(Crash::App)
        );
    }

    #[test]
    fn tombstone_header_claims_the_rest_of_the_tombstone() {
        let mut t = CrashTracker::new(PKG);
        t.add_app_pid(8012);
        assert_eq!(
            t.check(&format!("10-04 12:00:01.000  8100  8100 F DEBUG   : pid: 8012, tid: 8040, name: GameThread  >>> {PKG} <<<")),
            Some(Crash::App)
        );
        assert_eq!(
            t.check("10-04 12:00:01.001  8100  8100 F DEBUG   :       #00 pc 0000000000012345  /data/app/lib/arm64/libmain.so"),
            Some(Crash::App)
        );
        assert_eq!(
            t.check("F/DEBUG   ( 8100):       #01 pc 0000000000012399"),
            Some(Crash::App)
        );
    }

    #[test]
    fn crash_signals_from_other_processes_are_foreign() {
        let mut t = CrashTracker::new(PKG);
        t.add_app_pid(8012);
        assert_eq!(
            t.check("10-04 12:00:00.000  1234  1234 E AndroidRuntime: FATAL EXCEPTION: main"),
            Some(Crash::Foreign)
        );
        assert_eq!(
            t.check("10-04 12:00:00.000  7000  7000 F DEBUG   :       #00 pc 0000000000000001"),
            Some(Crash::Foreign)
        );
        assert_eq!(
            t.check("10-04 12:00:00.000   545   600 E ActivityManager: ANR in com.android.systemui"),
            None
        );
        assert_eq!(
            t.check(&format!(
                "10-04 12:00:00.000  8012  8012 I regia : regia lifecycle: Resumed ({PKG})"
            )),
            None
        );
    }

    #[test]
    fn pidof_output() {
        assert_eq!(parse_pidof("8012\n"), Some(8012));
        assert_eq!(parse_pidof("8012 8013\n"), Some(8012));
        assert_eq!(parse_pidof(""), None);
        assert_eq!(parse_pidof("pidof: not found"), None);
    }

    #[test]
    fn devices_and_choice() {
        let text = "* daemon not running; starting now at tcp:5037\n* daemon started successfully\n\
                    List of devices attached\nemulator-5554\tdevice\nR58M123\tunauthorized usb:1-1 transport_id:3\n\n";
        let devices = parse_devices(text);
        assert_eq!(
            devices,
            vec![
                Device {
                    serial: "emulator-5554".into(),
                    state: "device".into()
                },
                Device {
                    serial: "R58M123".into(),
                    state: "unauthorized".into()
                },
            ]
        );
        assert_eq!(
            choose_device(&devices, Some("emulator-5554")),
            Ok("emulator-5554".into())
        );
        assert!(choose_device(&devices, Some("R58M123")).is_err_and(|e| e.contains("unauthorized")));
        assert!(choose_device(&devices, None).is_err_and(|e| e.contains("--device")));
        assert!(choose_device(&devices, Some("nope")).is_err_and(|e| e.contains("not attached")));
        assert_eq!(choose_device(&devices[..1], None), Ok("emulator-5554".into()));
    }

    #[test]
    fn no_device_message() {
        let devices = parse_devices("List of devices attached\n\n");
        assert!(devices.is_empty());
        assert!(choose_device(&devices, None).is_err_and(|e| e.starts_with("no Android device attached")));
    }

    #[test]
    fn adb_search_order() {
        let env = AdbEnv {
            android_home: Some("/sdk".into()),
            sdk_root: None,
            home: Some("/home/u".into()),
            local_app_data: None,
            path: vec!["/usr/bin".into()],
        };
        let c = adb_candidates(&env, false);
        assert_eq!(
            c,
            vec![
                PathBuf::from("/sdk/platform-tools/adb"),
                PathBuf::from("/home/u/Android/Sdk/platform-tools/adb"),
                PathBuf::from("/usr/bin/adb"),
            ]
        );
        assert!(adb_candidates(&env, true).iter().all(|p| p.ends_with("adb.exe")));
    }

    #[test]
    fn components_and_settings() {
        assert_eq!(component(PKG, ".RegiaActivity"), format!("{PKG}/.RegiaActivity"));
        assert_eq!(component(PKG, "a.b/.C"), "a.b/.C");
        assert_eq!(parse_setting("1\n"), Some("1".into()));
        assert_eq!(parse_setting("null\n"), None);
    }

    #[test]
    fn medians() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[5]), Some(5));
        assert_eq!(median(&[30, 10, 20]), Some(20));
        assert_eq!(median(&[4, 1, 3, 2]), Some(2));
    }

    #[test]
    fn leak_decision() {
        assert_eq!(leak_verdict(1000, 1050, 0.05), (5.0, false));
        assert!(leak_verdict(1000, 1051, 0.05).1);
        assert_eq!(leak_verdict(1000, 900, 0.05), (-10.0, false));
    }

    #[test]
    fn rotation_sequences_alternate_and_start_with_a_change() {
        assert_eq!(rotation_sequence(4, None), vec![1, 3, 1, 3]);
        assert_eq!(rotation_sequence(3, Some(0)), vec![1, 3, 1]);
        assert_eq!(rotation_sequence(3, Some(1)), vec![3, 1, 3]);
        assert!(rotation_sequence(0, None).is_empty());
    }

    #[test]
    fn rotation_confirmation() {
        let mut o = RotationObservation::default();
        assert!(!o.settled());
        assert_eq!(o.confirmation(), RotationConfirmation::Continuous);
        o.see(Marker::Resized);
        assert!(o.settled());
        assert_eq!(o.confirmation(), RotationConfirmation::Resized);

        let mut r = RotationObservation::default();
        r.see(Marker::Suspended);
        r.see(Marker::Resumed);
        assert!(!r.settled(), "recreated but not yet drawn");
        r.see(Marker::FirstFrame);
        assert!(r.settled());
        assert_eq!(r.confirmation(), RotationConfirmation::Recreated);
    }

    /// Shape of `dumpsys window displays` (Android 17, TEST Pixel 9 Pro): one
    /// section per display, each with its own `mRotation=` line beside `mUserRotation`.
    fn displays(rotation_0: &str, user_0: &str, extra: &str) -> String {
        format!(
            "WINDOW MANAGER DISPLAY CONTENTS (dumpsys window displays)\n  \
             Display: mDisplayId=0 (organized) rootTasks=[1]\n    \
             init=1080x2424 420dpi cur=2424x1080 app=2424x1080\n    \
             mCurrentAppOrientation=SCREEN_ORIENTATION_SENSOR_LANDSCAPE\n    \
             {rotation_0}\n    \
             mUserRotationMode=USER_ROTATION_LOCKED mUserRotation={user_0} mCameraRotationMode=0\n\
             {extra}"
        )
    }

    #[test]
    fn display_rotation_reads_display_0_actual_not_user_rotation() {
        let raw = displays("mRotation=1 mDeferredRotationPauseCount=0", "ROTATION_270", "");
        assert_eq!(parse_display_rotation(&raw), Some(1));
        let raw = displays("mRotation=3 mDeferredRotationPauseCount=0", "ROTATION_270", "");
        assert_eq!(parse_display_rotation(&raw), Some(3));
    }

    #[test]
    fn display_rotation_ignores_secondary_displays_and_rejects_missing_or_ambiguous() {
        let secondary =
            "  Display: mDisplayId=2 rootTasks=[]\n    mRotation=0 mDeferredRotationPauseCount=0\n";
        let raw = displays(
            "mRotation=1 mDeferredRotationPauseCount=0",
            "ROTATION_90",
            secondary,
        );
        assert_eq!(
            parse_display_rotation(&raw),
            Some(1),
            "display 2 must not override display 0"
        );
        // Display 10 must not be read as display 1 or 0.
        let raw = "  Display: mDisplayId=10\n    mRotation=2\n";
        assert_eq!(parse_display_rotation(raw), None, "no display 0 section");
        let raw = displays("mDeferredRotationPauseCount=0", "ROTATION_90", "");
        assert_eq!(parse_display_rotation(&raw), None, "display 0 without mRotation");
        let raw = displays("mRotation=1\n    mRotation=3", "ROTATION_90", "");
        assert_eq!(parse_display_rotation(&raw), None, "two different values");
        let raw = displays("mRotation=7", "ROTATION_90", "");
        assert_eq!(parse_display_rotation(&raw), None, "not a rotation");
        assert_eq!(parse_display_rotation(""), None);
    }

    #[test]
    fn ignored_rotation_request_fails_instead_of_counting_as_continuous() {
        // The 2026-11-08 TEST Pixel case: sensorLandscape activity, user rotation 3 requested,
        // display stays at 1, no marker. The old harness recorded `Continuous`.
        let seen = RotationObservation::default();
        assert_eq!(
            rotation_outcome(3, Some(1), &seen),
            Err(RotationMiss::Ignored {
                requested: 3,
                actual: 1
            })
        );
        assert_eq!(rotation_outcome(3, None, &seen), Err(RotationMiss::Unreadable));
    }

    #[test]
    fn observed_rotations_keep_their_marker_confirmation() {
        // A real 180-degree turn keeps the window size: no marker, but the display moved.
        let seen = RotationObservation::default();
        assert_eq!(
            rotation_outcome(3, Some(3), &seen),
            Ok(RotationConfirmation::Continuous)
        );
        let mut resized = RotationObservation::default();
        resized.see(Marker::Resized);
        assert_eq!(
            rotation_outcome(1, Some(1), &resized),
            Ok(RotationConfirmation::Resized)
        );
        // A recreation counts only with the display at the requested rotation, too.
        let mut recreated = RotationObservation::default();
        recreated.see(Marker::Suspended);
        recreated.see(Marker::Resumed);
        recreated.see(Marker::FirstFrame);
        assert_eq!(
            rotation_outcome(1, Some(1), &recreated),
            Ok(RotationConfirmation::Recreated)
        );
        assert_eq!(
            rotation_outcome(1, Some(3), &recreated),
            Err(RotationMiss::Ignored {
                requested: 1,
                actual: 3
            })
        );
    }

    #[test]
    fn fixed_to_user_rotation_round_trips_and_rejects_unknown_output() {
        for mode in [
            FixedToUserRotation::Default,
            FixedToUserRotation::Enabled,
            FixedToUserRotation::Disabled,
            FixedToUserRotation::EnabledIfNoAutoRotation,
        ] {
            assert_eq!(
                parse_fixed_to_user_rotation(&format!("{}\r\n", mode.arg())),
                Some(mode)
            );
        }
        // TEST Pixel 9 Pro, Android 17, 2026-11-09: an untouched device prints `default`.
        assert_eq!(
            parse_fixed_to_user_rotation("default\n"),
            Some(FixedToUserRotation::Default)
        );
        assert_eq!(parse_fixed_to_user_rotation(""), None);
        assert_eq!(
            parse_fixed_to_user_rotation("Unknown command: fixed-to-user-rotation"),
            None
        );
        assert_eq!(parse_fixed_to_user_rotation("enabled disabled"), None);
    }

    fn passing_report() -> Report {
        Report {
            package: PKG.into(),
            device: "emulator-5554".into(),
            cycles_requested: 2,
            cycles_done: 2,
            resume_ms: vec![200, 100],
            rotations_requested: 2,
            rotations: vec![RotationConfirmation::Resized, RotationConfirmation::Continuous],
            leak_tolerance: 0.05,
            baseline_samples_kb: vec![1000, 1001, 999],
            baseline_pss_kb: Some(1000),
            final_samples_kb: vec![1020],
            final_pss_kb: Some(1020),
            pids: vec![8012],
            ..Report::default()
        }
    }

    #[test]
    fn verdict_pass() {
        let mut r = passing_report();
        r.evaluate();
        assert_eq!(r.verdict, "pass");
        assert!(r.failures.is_empty());
        assert_eq!(r.delta_percent, Some(2.0));
        let text = r.summary().join("\n");
        assert!(text.contains("PASS"));
        assert!(text.contains("2/2 (start to first frame: min 100 ms, median 100 ms, max 200 ms)"));
        assert!(text.contains("resized 1, continuous 1, recreated 0"));
    }

    #[test]
    fn verdict_fail_reasons() {
        let mut leak = passing_report();
        leak.final_pss_kb = Some(1100);
        leak.evaluate();
        assert_eq!(leak.verdict, "fail");
        assert!(leak.failures[0].contains("PSS grew +10.00 %"));

        let mut crash = passing_report();
        crash.crash_lines.push("FATAL EXCEPTION".into());
        crash.pids.push(9000);
        crash.cycles_done = 1;
        crash.rotations.clear();
        crash.final_pss_kb = None;
        crash.aborted = Some("timed out".into());
        crash.evaluate();
        assert_eq!(crash.failures.len(), 6, "{:?}", crash.failures);

        let mut foreign_only = passing_report();
        foreign_only.foreign_crash_lines.push("FATAL EXCEPTION".into());
        foreign_only.evaluate();
        assert_eq!(foreign_only.verdict, "pass");
    }

    #[test]
    fn surface_audit_lines_parse_with_any_prefix() {
        let line = "10-21 13:30:01.123  8012  8040 I regia_platform: regia surface audit: suspends=3 \
                    resumes=4 created=4 dropped=3 live=1 windows=1 violations=0";
        assert_eq!(
            parse_surface_audit(line),
            Some(SurfaceAuditLine {
                suspends: 3,
                resumes: 4,
                created: 4,
                dropped: 3,
                live: 1,
                windows: 1,
                violations: 0,
            })
        );
        assert_eq!(
            parse_surface_audit("regia surface audit: suspends=1"),
            None,
            "incomplete"
        );
        assert_eq!(parse_surface_audit("regia lifecycle: Resumed"), None);
        assert_eq!(
            parse_surface_audit(
                "regia surface audit: suspends=x resumes=1 created=1 dropped=0 live=1 windows=1 violations=0"
            ),
            None,
            "not a number"
        );
    }

    #[test]
    fn surface_audit_violations_fail_the_run() {
        let mut clean = passing_report();
        clean.surface_audit = Some(SurfaceAuditLine {
            resumes: 3,
            created: 3,
            dropped: 2,
            live: 1,
            windows: 1,
            ..SurfaceAuditLine::default()
        });
        clean.evaluate();
        assert_eq!(clean.verdict, "pass");
        assert!(
            clean
                .summary()
                .join("\n")
                .contains("created 3, dropped 2, live 1/1 windows, 0 violation(s)")
        );

        let mut broken = clean;
        broken.surface_audit = broken
            .surface_audit
            .map(|a| SurfaceAuditLine { violations: 2, ..a });
        broken.evaluate();
        assert_eq!(broken.verdict, "fail");
        assert!(
            broken.failures[0].starts_with("surface audit: 2 violation(s)"),
            "{:?}",
            broken.failures
        );

        let mut old_engine = passing_report();
        old_engine.evaluate();
        assert_eq!(
            old_engine.verdict, "pass",
            "a missing audit line is not a failure"
        );
        assert!(old_engine.summary().join("\n").contains("no surface audit line"));
    }
}
