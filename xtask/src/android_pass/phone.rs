// SPDX-License-Identifier: MPL-2.0
//! The phone checks of `android-pass` (ADR-1749): cold and warm start times, touch, memory trim,
//! the ASTC texture path, the back button and the crash scan.
//!
//! Everything here reads text or JSON the device or the app produced, so it is tested on Linux;
//! `android_pass.rs` drives the device and writes the files.

use std::fmt::Write as _;

use serde_json::{Value, json};

use crate::android_lifecycle::logic::{Crash, CrashTracker, Marker};

/// One `am start -W` result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartTiming {
    /// `LaunchState:` (`COLD`, `WARM`, `HOT`; older releases print none).
    pub launch_state: Option<String>,
    /// `TotalTime:` in ms: from the intent to the activity's first drawn window.
    pub total_ms: Option<u64>,
    /// `WaitTime:` in ms: what `am` itself waited.
    pub wait_ms: Option<u64>,
    /// `Status:` is `ok`.
    pub ok: bool,
}

/// Parses `am start -W` output.
#[must_use]
pub fn parse_am_start(text: &str) -> StartTiming {
    let mut timing = StartTiming::default();
    for line in text.lines().map(str::trim) {
        let num = |rest: &str| rest.trim().parse::<u64>().ok();
        if let Some(rest) = line.strip_prefix("LaunchState:") {
            timing.launch_state = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("TotalTime:") {
            timing.total_ms = num(rest);
        } else if let Some(rest) = line.strip_prefix("WaitTime:") {
            timing.wait_ms = num(rest);
        } else if let Some(rest) = line.strip_prefix("Status:") {
            timing.ok = rest.trim() == "ok";
        }
    }
    timing
}

/// Milliseconds since midnight of a `logcat -v threadtime` line (`10-29 19:50:01.234 ...`).
#[must_use]
pub fn log_time_ms(line: &str) -> Option<u64> {
    let mut fields = line.split_whitespace();
    let date = fields.next()?;
    let time = fields.next()?;
    if date.len() != 5 || date.as_bytes().get(2) != Some(&b'-') {
        return None;
    }
    let (hms, ms) = time.split_once('.')?;
    let mut parts = hms.split(':').map(|p| p.parse::<u64>().ok());
    let (h, m, s) = (parts.next()??, parts.next()??, parts.next()??);
    if parts.next().is_some() || h > 23 || m > 59 || s > 60 || ms.len() != 3 {
        return None;
    }
    Some(((h * 60 + m) * 60 + s) * 1000 + ms.parse::<u64>().ok()?)
}

/// The `MM-DD hh:mm:ss.mmm` stamp of the last complete `logcat -v threadtime` line of `log`: where
/// a restarted `adb logcat -T` picks up after the stream died (an adb transport reset).
#[must_use]
pub fn last_log_stamp(log: &str) -> Option<String> {
    let complete = log.rfind('\n').map_or("", |end| &log[..end]);
    complete.lines().rev().find_map(|line| {
        log_time_ms(line)?;
        let mut fields = line.split_whitespace();
        Some(format!("{} {}", fields.next()?, fields.next()?))
    })
}

/// The process id of a `logcat -v threadtime` line (`10-29 19:50:01.234  4000  4010 I tag: msg`).
fn log_pid(line: &str) -> Option<u32> {
    line.split_whitespace().nth(2)?.parse().ok()
}

/// From the system's `START u0 ... <package>/...` line to the engine's first presented frame,
/// in ms of device time, from one launch's `logcat -v threadtime` dump. A midnight between the
/// two lines is handled; `None` when either line is missing.
///
/// The frame is the `regia render: first frame presented after resume` marker line. When logcat
/// lost that line (the v0.5.0 rc4 device pass, run 1), the same process's `boot: first_present at`
/// line stands in for it: the engine logs both in one call. "The same process" is the pid of the
/// first `boot:` line after START, so a dying previous process's lines do not count.
#[must_use]
pub fn start_to_first_frame_ms(log: &str, package: &str) -> Option<u64> {
    const DAY_MS: u64 = 24 * 3600 * 1000;
    const BOOT_PRESENT: &str = "boot: first_present at";
    let component = format!("{package}/");
    let mut lines = log.lines();
    let start = lines
        .by_ref()
        .find(|l| l.contains("START u0") && l.contains(&component))
        .and_then(log_time_ms)?;
    let after: Vec<&str> = lines.collect();
    let frame = after
        .iter()
        .find(|l| l.contains(Marker::FirstFrame.text()))
        .copied()
        .or_else(|| {
            let pid = after
                .iter()
                .find(|l| l.contains("boot: "))
                .and_then(|l| log_pid(l))?;
            after
                .iter()
                .find(|l| l.contains(BOOT_PRESENT) && log_pid(l) == Some(pid))
                .copied()
        })
        .and_then(log_time_ms)?;
    Some((frame + DAY_MS - start) % DAY_MS)
}

/// Median of the values that are present (the lower middle for an even count).
#[must_use]
pub fn median(values: &[Option<u64>]) -> Option<u64> {
    let mut v: Vec<u64> = values.iter().flatten().copied().collect();
    v.sort_unstable();
    v.get(v.len().checked_sub(1)? / 2).copied()
}

/// The pacing numbers of one window, from the `pacing.get` answers before and after it (the
/// counters run from the app's start). `late_pct` is `null` when no frame was paced on the display.
#[must_use]
pub fn pacing_window(before: &Value, after: &Value) -> Value {
    let grown = |key: &str| {
        after[key]
            .as_u64()
            .map(|now| now.saturating_sub(before[key].as_u64().unwrap_or(0)))
    };
    let (frames, late, timeouts) = (grown("frames"), grown("late"), grown("timeouts"));
    let late_pct = match (frames, late) {
        (Some(frames), Some(late)) if frames > 0 => Some(late as f64 * 100.0 / frames as f64),
        _ => None,
    };
    json!({
        "source": after["source"],
        "refresh_hz": after["refresh_hz"],
        "swap_interval": after["swap_interval"],
        "frames": frames,
        "late": late,
        "late_pct": late_pct,
        "timeouts": timeouts,
    })
}

/// One start-time row of `startup.tsv`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartRow {
    /// `cold` or `warm`.
    pub kind: &'static str,
    /// What `am start -W` reported.
    pub timing: StartTiming,
    /// The engine's first frame after the system's START line (device clock).
    pub first_frame_ms: Option<u64>,
}

/// The header of `startup.tsv`.
pub const STARTUP_HEADER: &str = "run\tkind\tlaunch_state\ttotal_ms\twait_ms\tfirst_frame_ms";

/// `startup.tsv` (header and one line per row) and the summary line.
#[must_use]
pub fn startup_table(rows: &[StartRow]) -> (String, String) {
    let cell = |v: Option<u64>| v.map_or_else(|| "-".to_owned(), |v| v.to_string());
    let mut tsv = format!("{STARTUP_HEADER}\n");
    for (i, r) in rows.iter().enumerate() {
        let _ = writeln!(
            tsv,
            "{}\t{}\t{}\t{}\t{}\t{}",
            i + 1,
            r.kind,
            r.timing.launch_state.as_deref().unwrap_or("-"),
            cell(r.timing.total_ms),
            cell(r.timing.wait_ms),
            cell(r.first_frame_ms)
        );
    }
    let mut summary = String::from("startup:");
    for kind in ["cold", "warm"] {
        let of: Vec<&StartRow> = rows.iter().filter(|r| r.kind == kind).collect();
        if of.is_empty() {
            continue;
        }
        let total: Vec<Option<u64>> = of.iter().map(|r| r.timing.total_ms).collect();
        let frame: Vec<Option<u64>> = of.iter().map(|r| r.first_frame_ms).collect();
        let _ = write!(
            summary,
            " {kind} x{} median TotalTime {} ms, first engine frame {} ms;",
            of.len(),
            cell(median(&total)),
            cell(median(&frame))
        );
    }
    (tsv, summary.trim_end_matches(';').to_owned())
}

/// The crash lines of a whole-run `logcat` dump (main and crash buffers), split into the app's
/// own and other processes' (same attribution as `android-lifecycle`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrashScan {
    /// Lines attributed to the app: any one fails the pass.
    pub app: Vec<String>,
    /// Crash lines of other processes (reported, never a failure).
    pub foreign: Vec<String>,
}

/// Scans `log` for crash signals of `package` (whose processes ran as `pids`).
#[must_use]
pub fn crash_scan(log: &str, package: &str, pids: &[u32]) -> CrashScan {
    let mut tracker = CrashTracker::new(package);
    for &pid in pids {
        tracker.add_app_pid(pid);
    }
    let mut scan = CrashScan::default();
    for line in log.lines() {
        match tracker.check(line) {
            Some(Crash::App) => scan.app.push(line.to_owned()),
            Some(Crash::Foreign) => scan.foreign.push(line.to_owned()),
            None => {}
        }
    }
    scan
}

/// The pids of `package` named by `ActivityManager: Start proc <pid>:<package>/...` lines, in
/// order (every process the app ran as during the dump).
#[must_use]
pub fn started_pids(log: &str, package: &str) -> Vec<u32> {
    let needle = format!(":{package}/");
    let mut pids = Vec::new();
    for line in log.lines() {
        let Some(at) = line.find("Start proc ") else {
            continue;
        };
        let rest = &line[at + "Start proc ".len()..];
        if let Some((pid, tail)) = rest.split_once(':')
            && format!(":{tail}").starts_with(&needle)
            && let Ok(pid) = pid.trim().parse::<u32>()
            && !pids.contains(&pid)
        {
            pids.push(pid);
        }
    }
    pids
}

/// What `input.touch_controls` showed while a finger was held on the screen.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TouchCheck {
    /// Fingers the engine reported down (a finger in phase `Ended` or `Cancelled` is not).
    pub fingers: usize,
    /// The first finger's current position (window pixels).
    pub position: Option<[f64; 2]>,
    /// The app installs `TouchControls`.
    pub controls_installed: bool,
    /// Actions held at that moment.
    pub held_actions: Vec<String>,
}

impl TouchCheck {
    /// The pass condition: the engine saw the finger.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.fingers > 0
    }
}

/// Reads an `input.touch_controls` result.
#[must_use]
pub fn touch_check(report: &Value) -> TouchCheck {
    // A lifted finger stays one frame in phase `Ended` (or `Cancelled`): not down any more.
    let all = report["fingers"].as_array().map_or(&[][..], Vec::as_slice);
    let fingers: Vec<&Value> = all
        .iter()
        .filter(|f| !matches!(f["phase"].as_str(), Some("Ended" | "Cancelled")))
        .collect();
    let position = fingers.first().and_then(|f| {
        let p = f["current"].as_array()?;
        Some([p.first()?.as_f64()?, p.get(1)?.as_f64()?])
    });
    TouchCheck {
        fingers: fingers.len(),
        position,
        controls_installed: report["installed"].as_bool().unwrap_or(false),
        held_actions: report["held_actions"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default(),
    }
}

/// Where the touch check holds its finger on a display whose `wm size` is `w`x`h`, in the
/// orientation the app runs in (`landscape`: the long side is x): the middle of the width, four
/// fifths down. That stays clear of the system's one-time "Viewing full screen" hint, which
/// covers the upper half of the screen until someone confirms it (the pass never does: it
/// would write a system setting), and of a HUD's corner sticks and buttons.
#[must_use]
pub fn touch_point(w: u32, h: u32, landscape: bool) -> (u32, u32) {
    let (long, short) = (w.max(h), w.min(h));
    let (width, height) = if landscape { (long, short) } else { (short, long) };
    (width / 2, height * 4 / 5)
}

/// Whether the built-in display is rotated a quarter turn, from the first
/// `Viewport INTERNAL: ... orientation=N` line of `dumpsys input` (1 and 3 are landscape on a
/// portrait phone).
#[must_use]
pub fn is_landscape(dumpsys_input: &str) -> Option<bool> {
    let line = dumpsys_input.lines().find(|l| l.contains("Viewport INTERNAL:"))?;
    let rest = &line[line.find("orientation=")? + "orientation=".len()..];
    let n: u32 = rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
    Some(!n.is_multiple_of(2))
}

/// Whether the keyguard is showing, from the `isKeyguardShowing=` line of `dumpsys window`
/// (`None` when the build prints no such line).
#[must_use]
pub fn keyguard_showing(dumpsys_window: &str) -> Option<bool> {
    let rest = dumpsys_window
        .lines()
        .find_map(|l| l.trim().strip_prefix("isKeyguardShowing="))?;
    match rest.split_whitespace().next()? {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// The window with input focus in `dumpsys window` (`mCurrentFocus=Window{8e3cfb8 u0 <name>}`):
/// `<name>` is `com.x/com.x.RegiaActivity` for an app or a system window such as
/// `ImmersiveModeConfirmation`. `None` without the line or for `mCurrentFocus=null`.
#[must_use]
pub fn focused_window(dumpsys_window: &str) -> Option<String> {
    let rest = dumpsys_window
        .lines()
        .find_map(|l| l.trim().strip_prefix("mCurrentFocus="))?;
    let inner = rest.trim().strip_prefix("Window{")?;
    let inner = inner.split('}').next()?;
    inner.split_whitespace().last().map(str::to_owned)
}

/// Whether a [`focused_window`] name is one of `package`'s activities.
#[must_use]
pub fn window_is_package(window: &str, package: &str) -> bool {
    window
        .strip_prefix(package)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// `Physical size: 1080x2400` (an `Override size:` line wins) from `wm size`.
#[must_use]
pub fn parse_wm_size(text: &str) -> Option<(u32, u32)> {
    let pick = |prefix: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(prefix))
            .and_then(|s| s.trim().split_once('x'))
            .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
    };
    pick("Override size:").or_else(|| pick("Physical size:"))
}

/// The ASTC texture path verdict from `render.get_capabilities` and `assets.cooked_pack`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AstcCheck {
    /// The device exposes ASTC LDR (wgpu `TEXTURE_COMPRESSION_ASTC`).
    pub device_astc: bool,
    /// The device exposes BC (phones normally do not).
    pub device_bc: bool,
    /// Mounted paks.
    pub paks: usize,
    /// ASTC textures across the paks (`texture_families.astc`).
    pub astc_textures: u64,
    /// Textures of any other family across the paks.
    pub other_textures: u64,
    /// Textures the asset server had to decode on the CPU so far.
    pub cpu_decoded: u64,
    /// Textures the server loaded so far.
    pub loaded: u64,
}

impl AstcCheck {
    /// The pass condition: the device has ASTC, no texture was decoded on the CPU, and the paks
    /// (when there are any) carry no non-ASTC texture.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.device_astc && self.cpu_decoded == 0 && self.other_textures == 0
    }

    /// One summary line.
    #[must_use]
    pub fn line(&self) -> String {
        let verdict = if self.passed() { "PASS" } else { "FAIL" };
        let content = if self.paks == 0 {
            "no paks mounted (this APK ships no cooked textures)".to_owned()
        } else {
            format!(
                "{} pak(s), {} ASTC and {} other textures",
                self.paks, self.astc_textures, self.other_textures
            )
        };
        format!(
            "astc: {verdict} (device ASTC {}, BC {}; {content}; {} loaded, {} decoded on the CPU)",
            self.device_astc, self.device_bc, self.loaded, self.cpu_decoded
        )
    }
}

/// Reads the two agent results into an [`AstcCheck`].
#[must_use]
pub fn astc_check(caps: &Value, cooked: &Value) -> AstcCheck {
    let feature = |name: &str| caps["features"][name].as_bool().unwrap_or(false);
    let paks = cooked["paks"].as_array().map_or(&[][..], Vec::as_slice);
    let mut astc = 0;
    let mut other = 0;
    for pak in paks {
        if let Some(families) = pak["texture_families"].as_object() {
            for (family, count) in families {
                let n = count.as_u64().unwrap_or(0);
                if family.eq_ignore_ascii_case("astc") {
                    astc += n;
                } else {
                    other += n;
                }
            }
        }
    }
    AstcCheck {
        device_astc: feature("texture_compression_astc"),
        device_bc: feature("texture_compression_bc"),
        paks: paks.len(),
        astc_textures: astc,
        other_textures: other,
        cpu_decoded: cooked["loads"]["textures_cpu_decoded"].as_u64().unwrap_or(0),
        loaded: cooked["loads"]["textures"].as_u64().unwrap_or(0),
    }
}

/// The engine's marker for a memory warning (`onTrimMemory` / `onLowMemory`).
pub const LOW_MEMORY_LINE: &str = "regia lifecycle: LowMemory";

/// The engine's marker for the clean exit (`onDestroy` -> `AppExit`, or the OS ending the app).
pub const EXITING_LINE: &str = "regia lifecycle: WillTerminate";

/// Lines of `log` that contain `needle`.
#[must_use]
pub fn count_lines(log: &str, needle: &str) -> usize {
    log.lines().filter(|l| l.contains(needle)).count()
}

/// One line of the gamepad check from `dumpsys input`: the input devices whose sources include
/// a gamepad or joystick (`Sources: 0x...` with `GAMEPAD` / `JOYSTICK` in the decoded list).
#[must_use]
pub fn gamepads_in_dumpsys(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut name: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        // `  7: Xbox Wireless Controller` opens a device block in the EventHub section.
        if let Some((idx, rest)) = t.split_once(": ")
            && idx.parse::<i32>().is_ok()
        {
            name = Some(rest.trim().to_owned());
        }
        if t.starts_with("Classes:")
            && (t.contains("GAMEPAD") || t.contains("JOYSTICK"))
            && let Some(n) = name.take()
            && !found.contains(&n)
        {
            found.push(n);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pacing_window_counts_only_its_own_frames() {
        let before = json!({ "frames": 1000, "late": 40, "timeouts": 1, "source": "choreographer" });
        let after = json!({
            "frames": 1200, "late": 45, "timeouts": 1, "source": "choreographer",
            "refresh_hz": 90.0, "swap_interval": 2,
        });
        let window = pacing_window(&before, &after);
        assert_eq!(window["frames"], 200);
        assert_eq!(window["late"], 5);
        assert_eq!(window["late_pct"], 2.5);
        assert_eq!(window["timeouts"], 0);
        assert_eq!(window["swap_interval"], 2);
    }

    #[test]
    fn a_window_without_paced_frames_has_no_late_share() {
        let software = json!({ "frames": 0, "late": 0, "timeouts": 0, "source": "software" });
        assert_eq!(pacing_window(&software, &software)["late_pct"], Value::Null);
        assert_eq!(pacing_window(&Value::Null, &Value::Null)["late_pct"], Value::Null);
    }

    #[test]
    fn the_last_complete_logcat_line_gives_the_restart_stamp() {
        let log = "--------- beginning of main\n\
                   10-30 04:58:23.530 12124 12150 I regia_platform: thermal None\n\
                   10-30 04:58:23.544 14771 14819 I DisplayDeviceRepository: changed\n\
                   10-30 04:58:2";
        assert_eq!(last_log_stamp(log).as_deref(), Some("10-30 04:58:23.544"));
        assert_eq!(last_log_stamp("--------- beginning of main\n"), None);
        assert_eq!(last_log_stamp(""), None);
    }

    #[test]
    fn keyguard_state_is_read_from_dumpsys_window() {
        // Shape of the Fairphone 5's `dumpsys window` (Android 14) while PIN-locked.
        let locked = "  mCurrentFocus=Window{de1eb7a u0 NotificationShade}\n    mIsImeShowing=false\n\
                      KeyguardServiceDelegate\n      showing=true\n    isKeyguardShowing=true\n";
        assert_eq!(keyguard_showing(locked), Some(true));
        assert_eq!(keyguard_showing("    isKeyguardShowing=false\n"), Some(false));
        assert_eq!(
            keyguard_showing("mCurrentFocus=Window{1 u0 com.x/.RegiaActivity}\n"),
            None
        );
        assert_eq!(keyguard_showing("    isKeyguardShowing=maybe\n"), None);
    }

    #[test]
    fn the_focused_window_is_read_from_dumpsys_window() {
        // The test Pixel 9 Pro (Android 17) with the system's one-time full-screen notice over the
        // arena: the back key went to that window, not the app (ADR-3210).
        let notice = "  mCurrentFocus=Window{8e3cfb8 u0 ImmersiveModeConfirmation}\n  mFocusedApp=ActivityRecord{25945931 u0 com.regiaengine.samples.arena/.RegiaActivity t25}\n";
        assert_eq!(
            focused_window(notice).as_deref(),
            Some("ImmersiveModeConfirmation")
        );
        let app = "  mCurrentFocus=Window{16d6ac0 u0 com.regiaengine.samples.arena/com.regiaengine.samples.arena.RegiaActivity}\n";
        let window = focused_window(app).unwrap_or_default();
        assert!(window_is_package(&window, "com.regiaengine.samples.arena"));
        assert!(!window_is_package(&window, "com.regiaengine.samples"));
        assert!(!window_is_package(
            "ImmersiveModeConfirmation",
            "com.regiaengine.samples.arena"
        ));
        assert_eq!(focused_window("  mCurrentFocus=null\n"), None);
        assert_eq!(focused_window("    isKeyguardShowing=false\n"), None);
    }

    #[test]
    fn am_start_output_is_parsed() {
        let text = "Starting: Intent { act=android.intent.action.MAIN cmp=com.x/.RegiaActivity }\n\
                    Status: ok\nLaunchState: COLD\nActivity: com.x/.RegiaActivity\nTotalTime: 812\n\
                    WaitTime: 815\nComplete\n";
        let t = parse_am_start(text);
        assert_eq!(
            t,
            StartTiming {
                launch_state: Some("COLD".into()),
                total_ms: Some(812),
                wait_ms: Some(815),
                ok: true
            }
        );
        let failed = parse_am_start("Status: timeout\nWaitTime: 10003\n");
        assert!(!failed.ok);
        assert_eq!(failed.total_ms, None);
        assert_eq!(parse_am_start(""), StartTiming::default());
    }

    /// The marker line can be lost from logcat (v0.5.0 rc4 device pass, run 1): the same pid's
    /// `boot: first_present at` line then gives the first frame.
    #[test]
    fn start_to_first_frame_falls_back_to_the_boot_mark() {
        let start =
            "10-29 22:56:13.991  1100  1200 I ActivityTaskManager: START u0 {cmp=com.x/.A} from uid 2000\n";
        let boot_resumed = "10-29 22:56:14.936  4000  4010 I regia_render: boot: resumed at 940.1 ms\n";
        let boot_present =
            "10-29 22:56:19.495  4000  4010 I regia_render: boot: first_present at 5486.7 ms\n";
        let marker = "10-29 22:56:19.496  4000  4010 I regia_render: regia render: first frame presented after resume\n";
        // Marker line lost: 22:56:19.495 - 22:56:13.991 = 5504 ms.
        let lost = format!("{start}{boot_resumed}{boot_present}");
        assert_eq!(start_to_first_frame_ms(&lost, "com.x"), Some(5504));
        // Both lines present: the marker line still wins.
        let both = format!("{start}{boot_resumed}{boot_present}{marker}");
        assert_eq!(start_to_first_frame_ms(&both, "com.x"), Some(5505));
        // A `first_present` line of a different pid (the previous process dying) is not this start's.
        let other_pid = "10-29 22:56:15.000  3999  4001 I regia_render: boot: first_present at 99.0 ms\n";
        let mixed = format!("{start}{boot_resumed}{other_pid}{boot_present}");
        assert_eq!(start_to_first_frame_ms(&mixed, "com.x"), Some(5504));
        // Neither a marker nor a boot mark: nothing to report.
        let none = format!("{start}{boot_resumed}");
        assert_eq!(start_to_first_frame_ms(&none, "com.x"), None);
        // A boot mark that precedes the START line is not this launch's.
        let before = format!("{boot_present}{start}");
        assert_eq!(start_to_first_frame_ms(&before, "com.x"), None);
    }

    #[test]
    fn log_times_and_start_to_first_frame() {
        assert_eq!(log_time_ms("10-29 19:50:01.234  1  2 I Tag: x"), Some(71_401_234));
        assert_eq!(log_time_ms("--------- beginning of main"), None);
        assert_eq!(log_time_ms("10-29 25:00:00.000 1 2 I T: x"), None);
        let log = "10-29 19:50:00.900  1100  1200 I ActivityTaskManager: START u0 {act=android.intent.action.MAIN cmp=com.x/.RegiaActivity} from uid 2000\n\
                   10-29 19:50:01.000  4000  4010 I regia_platform: regia lifecycle: Resumed\n\
                   10-29 19:50:02.150  4000  4010 I regia_render: regia render: first frame presented after resume\n";
        assert_eq!(start_to_first_frame_ms(log, "com.x"), Some(1250));
        assert_eq!(start_to_first_frame_ms(log, "com.other"), None);
        let midnight = "10-29 23:59:59.900  1  2 I ActivityTaskManager: START u0 {cmp=com.x/.A}\n\
                        10-30 00:00:00.400  3  4 I regia_render: regia render: first frame presented after resume\n";
        assert_eq!(start_to_first_frame_ms(midnight, "com.x"), Some(500));
        let frame_before_start = "10-29 10:00:00.000  3  4 I r: regia render: first frame presented after resume\n\
                                  10-29 10:00:01.000  1  2 I A: START u0 {cmp=com.x/.A}\n";
        assert_eq!(start_to_first_frame_ms(frame_before_start, "com.x"), None);
    }

    #[test]
    fn start_to_first_frame_survives_a_dropped_marker_line() {
        // ADR-7687: the rc4 pass lost the marker line of one cold start in logcat while the
        // engine's `boot: first_present` line, logged in the same present call, arrived.
        let dropped = "10-29 19:50:00.900  1100  1200 I ActivityTaskManager: START u0 {cmp=com.x/.RegiaActivity}\n\
                       10-29 19:50:01.000  4000  4010 I regia_platform: regia lifecycle: Resumed\n\
                       10-29 19:50:05.400  4000  4010 I regia_render: boot: first_present at 5486.7 ms\n";
        assert_eq!(start_to_first_frame_ms(dropped, "com.x"), Some(4500));
        // Both lines present: the marker line counts (the boot line, logged in the same call,
        // only stands in when logcat lost the marker; integ-v051e kept the base's marker-first rule).
        let both = "10-29 19:50:00.900  1  2 I A: START u0 {cmp=com.x/.A}\n\
                    10-29 19:50:02.149  4000  4010 I regia_render: boot: first_present at 2100.0 ms\n\
                    10-29 19:50:02.150  4000  4010 I regia_render: regia render: first frame presented after resume\n";
        assert_eq!(start_to_first_frame_ms(both, "com.x"), Some(1250));
        // A boot line from before this start (an earlier process) does not count.
        let stale = "10-29 10:00:00.000  3  4 I r: boot: first_present at 900.0 ms\n\
                     10-29 10:00:01.000  1  2 I A: START u0 {cmp=com.x/.A}\n";
        assert_eq!(start_to_first_frame_ms(stale, "com.x"), None);
        // Other boot marks are not a first frame.
        let other = "10-29 10:00:01.000  1  2 I A: START u0 {cmp=com.x/.A}\n\
                     10-29 10:00:01.500  3  4 I r: boot: surface at 600.0 ms\n";
        assert_eq!(start_to_first_frame_ms(other, "com.x"), None);
    }

    #[test]
    fn startup_table_has_medians_and_a_cell_per_column() {
        let row = |kind, total, frame| StartRow {
            kind,
            timing: StartTiming {
                launch_state: Some(kind.to_uppercase()),
                total_ms: total,
                wait_ms: total,
                ok: true,
            },
            first_frame_ms: frame,
        };
        let rows = [
            row("cold", Some(900), Some(1500)),
            row("cold", Some(700), Some(1300)),
            row("cold", Some(800), None),
            row("warm", Some(120), Some(200)),
        ];
        let (tsv, summary) = startup_table(&rows);
        let columns = STARTUP_HEADER.split('\t').count();
        assert!(tsv.lines().all(|l| l.split('\t').count() == columns), "{tsv}");
        assert_eq!(tsv.lines().count(), 5);
        assert_eq!(
            summary,
            "startup: cold x3 median TotalTime 800 ms, first engine frame 1300 ms; \
             warm x1 median TotalTime 120 ms, first engine frame 200 ms"
        );
        assert_eq!(median(&[None, None]), None);
        assert_eq!(median(&[Some(3), None, Some(1)]), Some(1));
    }

    #[test]
    fn the_crash_scan_attributes_lines() {
        let log = "10-29 19:00:00.000  500  501 F libc    : Fatal signal 11 (SIGSEGV), code 1 in tid 501 (com.x)\n\
                   10-29 19:00:00.000  777  778 F libc    : Fatal signal 6 (SIGABRT) in tid 778 (other.app)\n\
                   10-29 19:00:01.000  900  900 I ActivityManager: Start proc 4242:com.x/u0a200 for top-activity\n\
                   10-29 19:00:02.000 4242 4250 E RustPanic: panicked at x.rs:1\n\
                   10-29 19:00:03.000 4242 4250 I regia_platform: regia lifecycle: Resumed\n";
        let pids = started_pids(log, "com.x");
        assert_eq!(pids, vec![4242]);
        let scan = crash_scan(log, "com.x", &pids);
        assert_eq!(scan.app.len(), 2, "{scan:?}");
        assert_eq!(scan.foreign.len(), 1);
        assert!(crash_scan("", "com.x", &[]).app.is_empty());
        assert!(
            started_pids(log, "com.xy").is_empty(),
            "a longer package name is another app"
        );
    }

    #[test]
    fn touch_reports_are_read() {
        let held = json!({ "installed": true, "held_actions": ["fire"],
            "fingers": [{ "id": 0, "phase": "Moved", "start": [10.0, 20.0], "current": [1200.0, 540.0] }] });
        let t = touch_check(&held);
        assert!(t.passed());
        assert_eq!(t.position, Some([1200.0, 540.0]));
        assert_eq!(t.held_actions, vec!["fire".to_owned()]);
        assert!(t.controls_installed);
        let lifted = json!({ "installed": false,
            "fingers": [{ "id": 0, "phase": "Ended", "start": [1.0, 2.0], "current": [1.0, 2.0] }] });
        assert!(!touch_check(&lifted).passed(), "an Ended finger is released");
        let none = touch_check(&json!({ "installed": false, "fingers": [] }));
        assert!(!none.passed());
        assert!(!touch_check(&json!(null)).passed());
    }

    #[test]
    fn screen_geometry() {
        assert_eq!(parse_wm_size("Physical size: 1080x2400\n"), Some((1080, 2400)));
        assert_eq!(
            parse_wm_size("Physical size: 1080x2400\nOverride size: 720x1600\n"),
            Some((720, 1600))
        );
        assert_eq!(parse_wm_size("garbage"), None);
        assert_eq!(touch_point(1080, 2400, true), (1200, 864));
        assert_eq!(touch_point(1080, 2400, false), (540, 1920));
        assert_eq!(
            touch_point(2700, 1224, true),
            (1350, 979),
            "rotation-agnostic input"
        );
        let viewport = "  Viewport INTERNAL: displayId=0, port=0, orientation=1, densityDpi=420\n  Viewport INTERNAL: orientation=0\n";
        assert_eq!(is_landscape(viewport), Some(true));
        assert_eq!(
            is_landscape(&viewport.replace("orientation=1", "orientation=2")),
            Some(false)
        );
        assert_eq!(is_landscape("no viewport"), None);
    }

    #[test]
    fn astc_verdicts() {
        let caps =
            json!({ "features": { "texture_compression_astc": true, "texture_compression_bc": false } });
        let cooked = json!({ "paks": [{ "texture_families": { "astc": 12 } }],
            "loads": { "textures": 12, "textures_cpu_decoded": 0 } });
        let ok = astc_check(&caps, &cooked);
        assert!(ok.passed(), "{ok:?}");
        assert_eq!(ok.astc_textures, 12);
        assert!(ok.line().starts_with("astc: PASS"));
        let bc_pak = json!({ "paks": [{ "texture_families": { "astc": 2, "bc": 3 } }],
            "loads": { "textures": 5, "textures_cpu_decoded": 3 } });
        let bad = astc_check(&caps, &bc_pak);
        assert!(!bad.passed());
        assert_eq!((bad.other_textures, bad.cpu_decoded), (3, 3));
        let no_paks = astc_check(&caps, &json!({ "paks": [], "loads": {} }));
        assert!(no_paks.passed(), "no cooked content is not a failure");
        assert!(no_paks.line().contains("no paks"));
        assert!(
            !astc_check(&json!({}), &json!({})).passed(),
            "no ASTC on the device fails"
        );
    }

    #[test]
    fn gamepads_are_found_in_dumpsys_input() {
        let text = "Event Hub State:\n  Devices:\n    3: uinput-fpc\n      Classes: KEYBOARD\n\
                    7: Xbox Wireless Controller\n      Classes: KEYBOARD | JOYSTICK | GAMEPAD\n";
        assert_eq!(
            gamepads_in_dumpsys(text),
            vec!["Xbox Wireless Controller".to_owned()]
        );
        assert!(gamepads_in_dumpsys("").is_empty());
        assert_eq!(
            count_lines("a LowMemory\nb\nregia lifecycle: LowMemory\n", LOW_MEMORY_LINE),
            1
        );
    }
}
