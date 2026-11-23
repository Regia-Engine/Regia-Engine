// SPDX-License-Identifier: MPL-2.0
//! Platform services (`docs/specs/11-platform-and-input.md` §§ 3 and 5; M1 subset per ADR-012 § 4):
//! [`open_url`], [`system_locale`], [`OsInfo`], [`Clipboard`], and the Android-specific [`SafeArea`],
//! [`Haptics`] and [`KeepScreenOn`] (no-ops or zeros on desktop).
//!
//! Android thermal state is [`ThermalState`]; clipboard is Tier 2 and not implemented. [`PowerState`] reads the host supply.

pub(crate) mod parse;
pub(crate) mod power;
pub(crate) mod thermal;

pub use power::{
    LINUX_POWER_SUPPLY_CLASS, LOW_POWER_BELOW_PERCENT, PowerSource, PowerState, power_state,
    read_power_supply_dir,
};

pub use parse::locale_from_env;
pub use thermal::ActivityState;
pub use thermal::{ThermalOverride, ThermalState, ThermalStatus};
pub(crate) use thermal::{poll_activity, poll_thermal};

use std::time::Duration;

use regia_app::prelude::*;

use crate::{AppLifecycle, LifecycleEvent, PrimaryWindow, Window, WindowFocused, WindowResized};

/// Platform service failures.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// Not available on this platform (or not yet).
    #[error("unsupported on this platform: {0}")]
    Unsupported(&'static str),
    /// [`open_url`] refused the URL.
    #[error("cannot open URL {url:?}: {reason}")]
    InvalidUrl {
        /// The URL.
        url: String,
        /// Why it was refused.
        reason: String,
    },
    /// An OS call failed.
    #[error("{operation} failed: {message}")]
    Os {
        /// What was being done, e.g. `"open_url"`.
        operation: &'static str,
        /// The OS or JNI error text.
        message: String,
    },
}

/// Opens `url` in the user's browser or mail client. Only `http`, `https` and `mailto` URLs are
/// accepted. Desktop: `xdg-open` on Linux, `/usr/bin/open` on macOS, `ShellExecuteW` on Windows; Android: an
/// `Intent.ACTION_VIEW` activity. Returns once the request has been handed to the OS.
pub fn open_url(url: &str) -> Result<(), ServiceError> {
    parse::validate_url(url)?;
    tracing::info!(target: "regia_platform", "open_url {url}");
    crate::platform::open_url(url)
}

/// Shows `path` (an existing file or directory) in the OS file manager: `xdg-open` on Linux,
/// `/usr/bin/open` on macOS, `explorer` on Windows. Returns once the request has been handed to
/// the OS; the file manager's exit status is not reported (`explorer` exits non-zero even when it
/// worked). Not available on Android.
#[cfg(not(target_os = "android"))]
pub fn open_path(path: &std::path::Path) -> Result<(), ServiceError> {
    use std::process::{Command, Stdio};

    if !path.exists() {
        return Err(ServiceError::Os {
            operation: "open_path",
            message: format!("{} does not exist", path.display()),
        });
    }
    tracing::info!(target: "regia_platform", "open_path {}", path.display());
    let mut child = Command::new(file_manager_program())
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| ServiceError::Os {
            operation: "open_path",
            message: e.to_string(),
        })?;
    // Reap the child in the background so it doesn't linger as a zombie.
    let spawned = std::thread::Builder::new()
        .name("regia-open-path".into())
        .spawn(move || {
            let _ = child.wait();
        });
    if let Err(err) = spawned {
        tracing::debug!(target: "regia_platform", "cannot spawn open_path reaper thread: {err}");
    }
    Ok(())
}

/// The program [`open_path`] hands a path to.
#[cfg(not(target_os = "android"))]
fn file_manager_program() -> &'static str {
    if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "/usr/bin/open"
    } else {
        "xdg-open"
    }
}

/// The user's preferred locale as a BCP-47 tag such as `en-US`, or `None` when the OS reports none
/// (e.g. the `C` locale).
pub fn system_locale() -> Option<String> {
    crate::platform::system_locale()
}

/// The host's local UTC offset in seconds (east positive) at `unix_secs`, for showing wall-clock
/// times: the C library's zone rules on Linux, macOS and Android, the registry's current bias on
/// Windows; `None` when the OS cannot tell (show UTC then). ADR-8586.
#[must_use]
pub fn local_utc_offset_seconds(unix_secs: i64) -> Option<i32> {
    crate::platform::local_utc_offset_seconds(unix_secs)
}

/// The OS text size multiplier: Android's Settings font size (`Configuration.fontScale`), 1 on
/// platforms that have none. The runner keeps it current on Android (window creation, resume, every
/// configuration change); `regia_ui` multiplies it into the text scale (ADR-4932).
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct SystemTextScale(pub f32);

impl Default for SystemTextScale {
    fn default() -> Self {
        Self(1.0)
    }
}

/// Whether the user turned captions on in the OS (Android: the captioning setting; Windows: the
/// Show Sounds accessibility flag). `None` where the platform has no such setting.
pub fn system_captions() -> Option<bool> {
    crate::platform::system_captions()
}

/// What the OS reports about the display's luminance range (Android
/// `Display.HdrCapabilities`, API 24+). `None` where the platform does not say: desktop winit
/// exposes no display luminance today (Windows DXGI `IDXGIOutput6::GetDesc1` is the follow-up).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default)]
pub struct DisplayLuminance {
    /// Peak luminance, cd/m² (`getDesiredMaxLuminance`).
    pub peak_nits: Option<f32>,
    /// Minimum luminance, cd/m² (`getDesiredMinLuminance`).
    pub min_nits: Option<f32>,
}

/// The processor model: the registry on Windows, `/proc/cpuinfo` elsewhere. Empty when unknown.
fn cpu_model() -> String {
    #[cfg(windows)]
    {
        crate::platform::cpu_model()
    }
    #[cfg(not(windows))]
    {
        std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|text| cpuinfo_model(&text))
            .unwrap_or_default()
    }
}

/// The model name in `/proc/cpuinfo` text: x86's `model name`, else Arm's `Hardware` or `Model`,
/// else the first core's `CPU implementer` / `CPU part` ids (`Arm implementer 0x41 part 0xd0b`).
#[cfg_attr(windows, allow(dead_code))]
fn cpuinfo_model(text: &str) -> Option<String> {
    let field = |key: &str| {
        text.lines().find_map(|line| {
            let (k, v) = line.split_once(':')?;
            (k.trim() == key)
                .then(|| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        })
    };
    ["model name", "Hardware", "Model"]
        .iter()
        .find_map(|key| field(key))
        .or_else(|| {
            let implementer = field("CPU implementer")?;
            let part = field("CPU part")?;
            Some(format!("Arm implementer {implementer} part {part}"))
        })
}

/// Queries [`DisplayLuminance`] from the OS (never fails: unknown fields stay `None`).
pub fn display_luminance() -> DisplayLuminance {
    crate::platform::display_luminance()
}

/// Facts about the device, filled when [`crate::PlatformPlugin`] is built.
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct OsInfo {
    /// `"linux"`, `"windows"`, `"macos"` or `"android"` (Rust's `std::env::consts::OS`).
    pub os: String,
    /// Human-readable OS version: the distribution on Linux (`Arch Linux`), `Windows 11 Pro 24H2
    /// (build 26100)`, or `Android 14 (API 34)`. Empty when unknown.
    pub version: String,
    /// Device or computer model (`Pixel 8`, the DMI product name on Linux, the BIOS product name on
    /// Windows). Empty when unknown.
    pub device_model: String,
    /// Physical RAM in bytes (0 when unknown).
    pub total_ram_bytes: u64,
    /// Logical CPUs available to the process.
    pub cpu_count: usize,
    /// Processor model name (`AMD Ryzen 7 7800X3D 8-Core Processor`). Empty when unknown.
    pub cpu_model: String,
}

impl OsInfo {
    /// Queries the OS. Cheap (a few small file reads or OS calls); never fails.
    pub fn detect() -> Self {
        let mut info = crate::platform::os_info();
        info.cpu_count = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        // A platform's own `os_info` may already know the model (macOS reads it from sysctl).
        let model = cpu_model();
        if !model.is_empty() {
            info.cpu_model = model;
        }
        info
    }

    pub(crate) fn unknown() -> Self {
        Self {
            os: std::env::consts::OS.to_owned(),
            version: String::new(),
            device_model: String::new(),
            total_ram_bytes: 0,
            cpu_count: 1,
            cpu_model: String::new(),
        }
    }
}

/// Text clipboard. Desktop uses the system clipboard; on Android it is Tier 2 and every call returns
/// [`ServiceError::Unsupported`].
#[derive(Resource, Default)]
pub struct Clipboard {
    backend: crate::platform::ClipboardBackend,
}

impl Clipboard {
    /// The clipboard's text.
    pub fn get_text(&self) -> Result<String, ServiceError> {
        self.backend.get_text()
    }

    /// Replaces the clipboard's content with `text`.
    pub fn set_text(&self, text: &str) -> Result<(), ServiceError> {
        self.backend.set_text(text)
    }
}

/// Insets of the area that is safe for UI, in physical pixels from each window edge: display
/// cutouts and visible system bars on Android, all zeros on desktop. Refreshed on every resume,
/// resize and focus change.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SafeArea {
    /// Pixels from the left edge.
    pub left: u32,
    /// Pixels from the top edge.
    pub top: u32,
    /// Pixels from the right edge.
    pub right: u32,
    /// Pixels from the bottom edge.
    pub bottom: u32,
}

impl SafeArea {
    /// Builds a safe area from OS insets, treating negative values as zero.
    pub fn from_insets(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        let c = |v: i32| u32::try_from(v).unwrap_or(0);
        Self {
            left: c(left),
            top: c(top),
            right: c(right),
            bottom: c(bottom),
        }
    }

    /// The per-edge maximum of two inset sets (e.g. system bars and display cutout).
    pub fn union(self, other: Self) -> Self {
        Self {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }

    /// Limits the insets so they never cover more than the window (`width` × `height` physical
    /// pixels). A zero size leaves the insets unchanged (size not known yet).
    pub fn clamped_to(self, width: u32, height: u32) -> Self {
        if width == 0 || height == 0 {
            return self;
        }
        let left = self.left.min(width);
        let top = self.top.min(height);
        Self {
            left,
            top,
            right: self.right.min(width - left),
            bottom: self.bottom.min(height - top),
        }
    }

    /// The safe rectangle `(x, y, width, height)` inside a `width` × `height` window.
    pub fn rect(self, width: u32, height: u32) -> (u32, u32, u32, u32) {
        let s = self.clamped_to(width, height);
        (
            s.left,
            s.top,
            width.saturating_sub(s.left + s.right),
            height.saturating_sub(s.top + s.bottom),
        )
    }
}

/// Vibration (Android `VibratorManager`, or `Vibrator` before API 31). A no-op on desktop.
/// Android apps need the `android.permission.VIBRATE` permission in their manifest.
#[derive(Resource, Debug, Default)]
pub struct Haptics {
    _private: (),
}

impl Haptics {
    /// Vibrates for `duration` at `amplitude` (`0.0..=1.0`; 0 does nothing, and a zero `duration` does nothing). Devices without
    /// amplitude control vibrate at their default strength.
    pub fn vibrate(&self, duration: Duration, amplitude: f32) -> Result<(), ServiceError> {
        if duration.is_zero() {
            return Ok(());
        }
        crate::platform::vibrate(duration, amplitude)
    }
}

/// The runtime microphone permission (Android `RECORD_AUDIO`, specs/59 § 2.5). Desktop platforms
/// have no prompt to drive: they answer [`Granted`](Self::Granted) and a privacy block surfaces as
/// an open error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophonePermission {
    /// The app may record.
    Granted,
    /// Never asked in this launch; [`request_microphone_permission`] shows the prompt.
    NotAsked,
    /// The system prompt is on screen.
    Pending,
    /// Refused. `can_prompt` is false after "don't ask again": only the system settings can change
    /// it.
    Denied {
        /// Asking again can still show a prompt.
        can_prompt: bool,
    },
}

/// The current microphone permission state. Cheap enough to poll once per voice frame.
#[must_use]
pub fn microphone_permission() -> MicrophonePermission {
    crate::platform::microphone_permission()
}

/// Shows the system microphone prompt (a no-op on desktop). Call it from an explicit user action
/// only (specs/59 § 2.5); the app's manifest must list `android.permission.RECORD_AUDIO`.
pub fn request_microphone_permission() {
    crate::platform::request_microphone_permission();
}

/// Maps the activity's prompt state and the permission check to [`MicrophonePermission`]: the pure
/// half of the Android path. `prompt` is 0 before any prompt, 1 while it shows, 2 once answered.
#[must_use]
pub fn resolve_microphone_permission(granted: bool, prompt: i32, can_prompt: bool) -> MicrophonePermission {
    if granted {
        MicrophonePermission::Granted
    } else {
        match prompt {
            0 => MicrophonePermission::NotAsked,
            1 => MicrophonePermission::Pending,
            _ => MicrophonePermission::Denied { can_prompt },
        }
    }
}

/// Keep the screen on while the app runs (Android `FLAG_KEEP_SCREEN_ON`; no effect on desktop).
/// Applied whenever the resource changes. Off by default; projects turn it on from their settings.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeepScreenOn(pub bool);

/// Pause gameplay [`Time`] while the primary window is out of focus (`docs/specs/11` § 2, the
/// `pause_on_focus_loss` key of `Regia.toml`'s `[runtime]` table, ADR-7070). Off by default: focus loss
/// then only releases held input (`regia_input`).
///
/// The pause sets [`Time::paused`] on focus loss and clears it on focus gain, but only a pause it set
/// itself: a game that paused before the window lost focus stays paused, and a game that unpaused
/// while unfocused keeps running.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PauseOnFocusLoss(pub bool);

/// `PreUpdate`: applies [`PauseOnFocusLoss`] to this frame's focus messages for the primary window
/// (any window when none is marked primary).
pub(crate) fn pause_on_focus_loss(
    setting: Res<PauseOnFocusLoss>,
    mut focus: MessageReader<WindowFocused>,
    primary: Query<(), With<PrimaryWindow>>,
    time: Option<ResMut<Time>>,
    mut ours: Local<bool>,
) {
    let mut lost = None;
    for event in focus.read() {
        if primary.is_empty() || primary.contains(event.window) {
            lost = Some(!event.focused);
        }
    }
    let Some(mut time) = time else {
        return;
    };
    if *ours && !time.paused {
        // The game unpaused while the window was unfocused: the pause is no longer ours.
        *ours = false;
    }
    match lost {
        Some(true) if setting.0 && !time.paused => {
            time.paused = true;
            *ours = true;
        }
        Some(false) if *ours => {
            time.paused = false;
            *ours = false;
        }
        _ if *ours && !setting.0 => {
            // The setting turned off while our pause held.
            time.paused = false;
            *ours = false;
        }
        _ => {}
    }
}

/// `Lifecycle`: re-query the safe area on every resume.
pub(crate) fn refresh_safe_area_on_resume(
    lifecycle: Res<AppLifecycle>,
    area: ResMut<SafeArea>,
    windows: Query<&Window, With<PrimaryWindow>>,
) {
    if lifecycle.last_event == Some(LifecycleEvent::Resumed) {
        refresh_safe_area(area, windows);
    }
}

/// `PreUpdate`: re-query the safe area after a resize (rotation) or focus change.
pub(crate) fn refresh_safe_area_on_change(
    mut resized: MessageReader<WindowResized>,
    mut focused: MessageReader<WindowFocused>,
    area: ResMut<SafeArea>,
    windows: Query<&Window, With<PrimaryWindow>>,
) {
    let changed = resized.read().count() + focused.read().count() > 0;
    if changed {
        refresh_safe_area(area, windows);
    }
}

fn refresh_safe_area(mut area: ResMut<SafeArea>, windows: Query<&Window, With<PrimaryWindow>>) {
    let Some(insets) = crate::platform::query_safe_area() else {
        return;
    };
    let (w, h) = windows.iter().next().map(|w| w.physical_size).unwrap_or((0, 0));
    let new = insets.clamped_to(w, h);
    if *area != new {
        tracing::info!(target: "regia_platform", "safe area {new:?} (window {w}x{h})");
        *area = new;
    }
}

/// `PostUpdate`: applies [`KeepScreenOn`] when its value differs from the last one applied (the OS
/// default is off, so nothing happens until a game turns it on).
pub(crate) fn apply_keep_screen_on(keep: Res<KeepScreenOn>, mut applied: Local<bool>) {
    if keep.0 != *applied {
        *applied = keep.0;
        crate::platform::set_keep_screen_on(keep.0);
    }
}

/// `PostUpdate`: shows or hides the Android soft keyboard when a window's `ime_enabled` changes.
/// (winit's `set_ime_allowed`, called by `apply_window_changes`, only makes *implicit* requests,
/// which some input methods ignore; this follows up with explicit ones.)
#[cfg(has_window)]
pub(crate) fn sync_soft_input(
    windows: Query<(Entity, Ref<Window>)>,
    mut previous: Local<std::collections::HashMap<Entity, bool>>,
) {
    for (entity, window) in &windows {
        let old = previous.insert(entity, window.ime_enabled);
        if window.is_changed()
            && let Some(visible) = soft_input_request(old, window.ime_enabled)
        {
            crate::platform::set_soft_input_visible(visible);
        }
    }
    // Forget windows that are gone.
    previous.retain(|entity, _| windows.contains(*entity));
}

/// The explicit soft-keyboard request for a window whose `ime_enabled` was `old` (`None`: first
/// seen) and is now `now`. A window that starts with IME on asks too (audit v033b: the first
/// sighting was skipped, so an input method that ignores the implicit request never showed the
/// keyboard); one that starts with it off asks nothing.
#[cfg(any(has_window, test))]
fn soft_input_request(old: Option<bool>, now: bool) -> Option<bool> {
    match old {
        None => now.then_some(true),
        Some(old) => (old != now).then_some(now),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_that_starts_with_ime_on_asks_for_the_keyboard() {
        assert_eq!(soft_input_request(None, true), Some(true));
        assert_eq!(soft_input_request(None, false), None);
        assert_eq!(soft_input_request(Some(false), true), Some(true));
        assert_eq!(soft_input_request(Some(true), false), Some(false));
        assert_eq!(soft_input_request(Some(true), true), None);
    }

    #[test]
    fn microphone_permission_follows_the_activity_state() {
        let resolve = resolve_microphone_permission;
        assert_eq!(resolve(true, 0, true), MicrophonePermission::Granted);
        assert_eq!(
            resolve(true, 2, false),
            MicrophonePermission::Granted,
            "granted in the settings app"
        );
        assert_eq!(resolve(false, 0, true), MicrophonePermission::NotAsked);
        assert_eq!(resolve(false, 1, true), MicrophonePermission::Pending);
        assert_eq!(
            resolve(false, 2, true),
            MicrophonePermission::Denied { can_prompt: true }
        );
        assert_eq!(
            resolve(false, 2, false),
            MicrophonePermission::Denied { can_prompt: false }
        );
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn desktop_has_no_prompt() {
        request_microphone_permission();
        assert_eq!(microphone_permission(), MicrophonePermission::Granted);
    }

    /// The JNI calls of `platform/android/jvm.rs` name methods the generated activity must have.
    #[test]
    fn the_generated_activity_has_the_microphone_helpers() {
        let activity = include_str!("../../../regia_cli/templates/android/RegiaActivity.java");
        for needed in [
            "public boolean regiaMicGranted()",
            "public int regiaMicPrompt()",
            "public boolean regiaMicCanPrompt()",
            "public void regiaRequestMic()",
            "public void onRequestPermissionsResult(",
        ] {
            assert!(activity.contains(needed), "{needed}");
        }
    }

    #[test]
    fn safe_area_maths() {
        let bars = SafeArea::from_insets(0, 80, 0, 48);
        let cutout = SafeArea::from_insets(0, 120, 0, -5);
        let both = bars.union(cutout);
        assert_eq!(both, SafeArea::from_insets(0, 120, 0, 48));
        assert_eq!(both.rect(1080, 2400), (0, 120, 1080, 2400 - 168));
        // Landscape with a cutout on the left.
        let land = SafeArea::from_insets(120, 0, 0, 0);
        assert_eq!(land.rect(2400, 1080), (120, 0, 2280, 1080));
        // Insets bigger than the window never produce a negative or overflowing rect.
        let huge = SafeArea::from_insets(5000, 5000, 5000, 5000);
        assert_eq!(huge.clamped_to(100, 50), SafeArea::from_insets(100, 50, 0, 0));
        assert_eq!(huge.rect(100, 50), (100, 50, 0, 0));
        assert_eq!(huge.clamped_to(0, 0), huge, "unknown size leaves insets alone");
    }

    #[test]
    fn desktop_defaults_are_inert() {
        assert_eq!(SafeArea::default(), SafeArea::from_insets(0, 0, 0, 0));
        assert_eq!(KeepScreenOn::default(), KeepScreenOn(false));
        #[cfg(not(target_os = "android"))]
        {
            assert!(Haptics::default().vibrate(Duration::from_millis(30), 0.5).is_ok());
            assert_eq!(crate::platform::query_safe_area(), None);
        }
    }

    #[test]
    fn cpuinfo_model_reads_x86_and_arm_forms() {
        let x86 =
            "processor\t: 0\nvendor_id\t: AuthenticAMD\nmodel name\t: AMD Ryzen 7 7800X3D 8-Core Processor\n";
        assert_eq!(
            cpuinfo_model("processor\t: 0\nCPU implementer\t: 0x41\nCPU part\t: 0xd0b\n").as_deref(),
            Some("Arm implementer 0x41 part 0xd0b")
        );
        assert_eq!(
            cpuinfo_model(x86).as_deref(),
            Some("AMD Ryzen 7 7800X3D 8-Core Processor")
        );
        assert_eq!(
            cpuinfo_model("Hardware\t: Qualcomm SM8350\n").as_deref(),
            Some("Qualcomm SM8350")
        );
        assert_eq!(cpuinfo_model("processor : 0\n"), None);
    }

    #[test]
    fn os_info_detects_something() {
        let info = OsInfo::detect();
        assert_eq!(info.os, std::env::consts::OS);
        assert!(info.cpu_count >= 1);
        #[cfg(target_os = "linux")]
        assert!(info.total_ram_bytes > 0, "{info:?}");
    }

    #[test]
    fn open_url_rejects_before_touching_the_os() {
        assert!(matches!(
            open_url("file:///etc/passwd"),
            Err(ServiceError::InvalidUrl { .. })
        ));
    }

    #[test]
    #[cfg(not(target_os = "android"))]
    fn open_path_rejects_a_missing_path_before_spawning() {
        let missing = std::env::temp_dir().join(format!("regia-no-such-dir-{}", std::process::id()));
        assert!(matches!(
            open_path(&missing),
            Err(ServiceError::Os {
                operation: "open_path",
                ..
            })
        ));
    }

    #[test]
    #[cfg(not(target_os = "android"))]
    fn file_manager_program_is_a_bare_name_or_absolute_tool() {
        let program = file_manager_program();
        assert!(!program.is_empty() && !program.contains(' '));
        if cfg!(windows) {
            assert_eq!(program, "explorer");
        }
    }

    #[test]
    fn system_locale_is_bcp47_shaped() {
        if let Some(tag) = system_locale() {
            assert!(!tag.contains('_') && !tag.contains('.'), "{tag}");
        }
    }
}
