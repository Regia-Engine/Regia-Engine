// SPDX-License-Identifier: MPL-2.0
//! Android (GameActivity through winit's `android-game-activity` backend).
//!
//! JNI calls live in `jvm.rs`, the APK asset mount in `apk.rs`. JNI failures are logged and fall
//! back to defaults; nothing here panics.

mod apk;
mod jvm;
mod perf_hint;
mod sched;
mod vsync;

use std::sync::{PoisonError, RwLock};
use std::time::Duration;

use regia_app::prelude::*;
use winit::event_loop::EventLoop;
use winit::platform::android::EventLoopBuilderExtAndroid;

use super::EventLoopSetupError;
use crate::services::parse::{
    android_amplitude, android_duration_ms, gamepad_rumble_request, meminfo_total_bytes,
    normalise_language_tag,
};
use crate::services::{OsInfo, SafeArea, ServiceError};
use crate::vfs::{DirMount, MountPoint, Vfs};

pub use apk::ApkAssetMount;
pub(crate) use perf_hint::{PerfHintSession, current_thread_id};
pub(crate) use sched::thread_sched_sample;
pub(crate) use vsync::{VsyncThread, monotonic_ns, set_frame_rate_hint, start_vsync_thread};
/// The Android activity handle passed to `android_main`.
pub use winit::platform::android::activity::AndroidApp;

/// [`AndroidApp`] as a resource, for JNI and asset access (specs/11 § 3).
#[derive(Resource, Clone)]
pub struct AndroidAppHandle(pub AndroidApp);

/// The current activity, for services that are free functions (`open_url`, `system_locale`). Set by
/// [`init`] each time `PlatformPlugin` is built (`android_main` may run more than once per process).
static APP: RwLock<Option<AndroidApp>> = RwLock::new(None);

fn app() -> Option<AndroidApp> {
    APP.read().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The app's internal data path (the host directory of `user://`), once [`init`] has run.
pub(crate) fn internal_data_path() -> Option<std::path::PathBuf> {
    app().and_then(|a| a.internal_data_path())
}

/// `regiaPackState(pack)` on the generated activity (`crate::asset_packs::pack_state`).
pub(crate) fn asset_pack_state(pack: &str) -> Result<Option<crate::asset_packs::PackState>, ServiceError> {
    jvm::pack_state(pack)
}

/// `regiaPackFetch` / `regiaPackCancel` / `regiaPackRemove` / `regiaPackConfirm`.
pub(crate) fn asset_pack_call(pack: &str, what: crate::asset_packs::PackCall) -> Result<(), ServiceError> {
    jvm::pack_call(pack, what)
}

/// `regiaPackLocation(pack)`.
pub(crate) fn asset_pack_location(pack: &str) -> Result<Option<std::path::PathBuf>, ServiceError> {
    jvm::pack_location(pack)
}

/// `ConnectivityManager.isActiveNetworkMetered()` (`crate::network::active_network_metered`).
pub(crate) fn active_network_metered() -> Result<Option<bool>, ServiceError> {
    jvm::active_network_metered()
}

/// Builds the event loop around the activity. `regia::run_android` inserts [`AndroidAppHandle`].
pub(crate) fn build_event_loop(world: &mut World) -> Result<EventLoop<()>, EventLoopSetupError> {
    let handle = world
        .get_resource::<AndroidAppHandle>()
        .ok_or(EventLoopSetupError::MissingAndroidApp)?;
    Ok(EventLoop::builder().with_android_app(handle.0.clone()).build()?)
}

/// Remembers the activity for the free-function services.
pub(crate) fn init(world: &World) {
    match world.get_resource::<AndroidAppHandle>() {
        Some(handle) => *APP.write().unwrap_or_else(PoisonError::into_inner) = Some(handle.0.clone()),
        None => tracing::warn!(
            target: "regia_platform",
            "AndroidAppHandle missing at PlatformPlugin build; VFS and services are unavailable"
        ),
    }
}

/// `Context.getCacheDir()`, with the `<internal data>/cache` fallback from the default VFS
/// mounts. Shared by the default VFS and `regia_platform::paths::cache_dir` (audit-g-agent6-platform).
pub(crate) fn cache_dir_for(app: &AndroidApp) -> Option<std::path::PathBuf> {
    match jvm::cache_dir() {
        Ok(dir) => Some(dir),
        Err(err) => {
            tracing::warn!(target: "regia_platform", "vfs: getCacheDir failed ({err}); using internal/cache");
            app.internal_data_path().map(|p| p.join("cache"))
        }
    }
}

/// `assets://` and `config://` from the APK, `user://` = internal storage, `cache://` = the cache
/// directory, `logs://` = `user://logs`.
pub(crate) fn default_vfs(_world: &World) -> Vfs {
    let mut vfs = Vfs::new();
    let Some(app) = app() else {
        tracing::warn!(target: "regia_platform", "vfs: no AndroidApp; nothing mounted");
        return vfs;
    };
    vfs.mount(MountPoint::Assets, ApkAssetMount::new(app.asset_manager(), ""));
    vfs.mount(
        MountPoint::Config,
        ApkAssetMount::new(app.asset_manager(), "config"),
    );
    let internal = app.internal_data_path();
    match &internal {
        Some(user) => {
            vfs.mount(
                MountPoint::Logs,
                DirMount::read_only(user.join("logs")).without_symlinks(),
            );
            vfs.mount(MountPoint::User, DirMount::writable(user.clone()));
        }
        None => tracing::warn!(target: "regia_platform", "vfs: no internal data path; user:// unmounted"),
    }
    let cache = cache_dir_for(&app);
    if let Some(cache) = cache {
        vfs.mount(MountPoint::Cache, DirMount::writable(cache));
    }
    vfs
}

/// The string extra the activity's launching intent carried under `key` (`am start --es key value`);
/// `None` without one or when the JNI call fails (logged). The editor's dev launch hands its
/// options over this way (ADR-8472).
pub fn launch_intent_string_extra(key: &str) -> Option<String> {
    match jvm::intent_string_extra(key) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(target: "regia_platform", "intent extra {key}: {err}");
            None
        }
    }
}

/// `Intent.ACTION_VIEW` for the URL.
pub(crate) fn open_url(url: &str) -> Result<(), ServiceError> {
    jvm::open_url(url)
}

/// The night-mode bit of the activity's configuration (`Configuration.uiMode`): `Some(true)` for
/// dark, `Some(false)` for light, `None` when undefined or there is no activity yet.
pub(crate) fn system_dark_appearance() -> Option<bool> {
    match app()?.config().ui_mode_night() {
        ndk::configuration::UiModeNight::Yes => Some(true),
        ndk::configuration::UiModeNight::No => Some(false),
        _ => None,
    }
}

/// `Locale.getDefault().toLanguageTag()`.
pub(crate) fn system_locale() -> Option<String> {
    match jvm::default_locale_tag() {
        Ok(tag) => normalise_language_tag(&tag),
        Err(err) => {
            tracing::warn!(target: "regia_platform", "system_locale: {err}");
            None
        }
    }
}

/// The Settings font size (`Configuration.fontScale`) through JNI; `None` on failure or a value
/// that is not a positive finite number.
pub(crate) fn system_font_scale() -> Option<f32> {
    match jvm::font_scale() {
        Ok(scale) if scale.is_finite() && scale > 0.0 => Some(scale),
        Ok(_) => None,
        Err(err) => {
            tracing::warn!(target: "regia_platform", "system_font_scale: {err}");
            None
        }
    }
}

/// The display scale factor (`Configuration.densityDpi / 160`, winit's formula) through JNI;
/// `None` on failure or a density that is not positive (ADR-5379).
pub(crate) fn system_display_scale() -> Option<f64> {
    match jvm::density_dpi() {
        Ok(dpi) => crate::runner::scale_from_density_dpi(dpi),
        Err(err) => {
            tracing::warn!(target: "regia_platform", "system_display_scale: {err}");
            None
        }
    }
}

/// `CaptioningManager.isEnabled()` through JNI.
pub(crate) fn system_captions() -> Option<bool> {
    match jvm::captioning_enabled() {
        Ok(enabled) => Some(enabled),
        Err(err) => {
            tracing::warn!(target: "regia_platform", "system_captions: {err}");
            None
        }
    }
}

/// `Display.getHdrCapabilities()` through JNI; `None` fields when the display reports none.
pub(crate) fn display_luminance() -> crate::services::DisplayLuminance {
    match jvm::display_luminance() {
        Ok(luminance) => luminance,
        Err(err) => {
            tracing::warn!(target: "regia_platform", "display_luminance: {err}");
            crate::services::DisplayLuminance::default()
        }
    }
}

/// `Build.*` through JNI and `/proc/meminfo`.
pub(crate) fn os_info() -> OsInfo {
    let mut info = OsInfo::unknown();
    match jvm::build_info() {
        Ok(build) => {
            info.version = format!("Android {} (API {})", build.release, build.sdk_int);
            let manufacturer = build.manufacturer.trim();
            let model = build.model.trim();
            info.device_model = if manufacturer.is_empty()
                || model.to_lowercase().starts_with(&manufacturer.to_lowercase())
            {
                model.to_owned()
            } else {
                format!("{manufacturer} {model}")
            };
        }
        Err(err) => tracing::warn!(target: "regia_platform", "os_info: {err}"),
    }
    info.total_ram_bytes = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| meminfo_total_bytes(&m))
        .unwrap_or(0);
    info
}

/// Android clipboard is Tier 2 (specs/11 § 5).
#[derive(Default)]
pub(crate) struct ClipboardBackend;

impl ClipboardBackend {
    pub(crate) fn get_text(&self) -> Result<String, ServiceError> {
        Err(ServiceError::Unsupported("Android clipboard is Tier 2"))
    }

    pub(crate) fn set_text(&self, _text: &str) -> Result<(), ServiceError> {
        Err(ServiceError::Unsupported("Android clipboard is Tier 2"))
    }
}

/// System bars and display cutout from the decor view's root `WindowInsets`.
pub(crate) fn query_safe_area() -> Option<SafeArea> {
    match jvm::safe_area() {
        Ok(area) => area,
        Err(err) => {
            tracing::warn!(target: "regia_platform", "safe area query failed: {err}");
            None
        }
    }
}

/// One-shot vibration.
pub(crate) fn vibrate(duration: Duration, amplitude: f32) -> Result<(), ServiceError> {
    let Some(amplitude) = android_amplitude(amplitude) else {
        return Ok(());
    };
    jvm::vibrate(android_duration_ms(duration), amplitude)
}

/// `FLAG_KEEP_SCREEN_ON` on the activity window, through the generated activity's UI-thread helper.
/// The Android window is the whole screen: the window layer refuses size requests and windowed
/// mode (ADR-7880).
pub(crate) const FIXED_WINDOW: bool = true;

pub(crate) fn set_keep_screen_on(on: bool) {
    match jvm::set_keep_screen_on(on) {
        Ok(()) => tracing::debug!(target: "regia_platform", "keep screen on: {on}"),
        Err(err) => tracing::warn!(target: "regia_platform", "keep screen on ({on}) failed: {err}"),
    }
}

/// `RECORD_AUDIO` through the generated activity. A failed JVM call reads as "denied, no prompt":
/// the microphone stays closed and `voice_chat.status` shows why.
pub(crate) fn microphone_permission() -> crate::services::MicrophonePermission {
    match jvm::microphone_permission() {
        Ok((granted, prompt, can_prompt)) => {
            crate::services::resolve_microphone_permission(granted, prompt, can_prompt)
        }
        Err(err) => {
            tracing::warn!(target: "regia_platform", "microphone permission query failed: {err}");
            crate::services::MicrophonePermission::Denied { can_prompt: false }
        }
    }
}

/// Shows the system prompt through the activity's UI-thread helper.
pub(crate) fn request_microphone_permission() {
    if let Err(err) = jvm::request_microphone_permission() {
        tracing::warn!(target: "regia_platform", "microphone permission request failed: {err}");
    }
}

/// Android input methods draw their own candidate strip on the soft keyboard: there is no
/// candidate window to place, so the caret rect is not passed on (ADR-7208).
pub(crate) fn set_ime_cursor_area(_window: &winit::window::Window, _rect: [f32; 4]) {}

/// Explicit soft keyboard requests (winit only makes implicit ones).
pub(crate) fn set_soft_input_visible(visible: bool) {
    let Some(app) = app() else { return };
    if visible {
        app.show_soft_input(false);
    } else {
        app.hide_soft_input(false);
    }
}

/// A connected gamepad/joystick device found through `InputManager` (see `jvm::gamepad_devices`).
pub struct GamepadDeviceInfo {
    /// Android input device id (stable while the device is connected).
    pub id: i32,
    /// Device name as reported by the OS.
    pub name: String,
}

/// Every connected device reporting gamepad/joystick sources (specs 11 § 6, Android backend).
pub fn enumerate_gamepads() -> Vec<GamepadDeviceInfo> {
    match jvm::gamepad_devices() {
        Ok(devices) => devices
            .into_iter()
            .map(|d| GamepadDeviceInfo {
                id: d.id,
                name: d.name,
            })
            .collect(),
        Err(err) => {
            tracing::warn!(target: "regia_platform", "gamepad device query failed: {err}");
            Vec::new()
        }
    }
}

/// Rumble one input device via `InputDevice.getVibrator()` (specs 11 § 6). `strength` is 0..1.
pub fn gamepad_rumble(device_id: i32, strength: f32, duration: Duration) -> Result<(), ServiceError> {
    // Zero (or NaN) strength means "stop", same as `Haptics::vibrate`, and
    // `VibrationEffect.createOneShot` throws on a zero duration: nothing to vibrate either way.
    let Some((ms, amplitude)) = gamepad_rumble_request(strength, duration) else {
        return Ok(());
    };
    match jvm::gamepad_rumble(device_id, ms, amplitude) {
        Ok(true) => Ok(()),
        Ok(false) => Err(ServiceError::Unsupported("device has no vibrator")),
        Err(err) => Err(err),
    }
}

/// Ends the process immediately with `code`, without running exit handlers.
///
/// winit allows one event loop per process and Android keeps the process alive after the activity
/// finishes, so a game that has exited must take its process with it or the next launch fails
/// ("EventLoop can't be recreated"). `std::process::exit` runs C/C++ exit handlers, and GameActivity's
/// make JNI calls from the wrong thread, which makes ART abort; `_exit` skips them (ADR-013). Flush logs
/// before calling.
pub fn exit_process(code: i32) -> ! {
    // SAFETY: `_exit` takes no pointers and never returns; skipping Rust and C exit handlers is the
    // point (the caller has flushed everything that matters).
    unsafe { libc::_exit(code) }
}

/// Sticky `ACTION_BATTERY_CHANGED` extras as a [`crate::services::PowerState`].
pub(crate) fn battery_power() -> Result<crate::services::PowerState, ServiceError> {
    jvm::battery_power()
}

/// `PowerManager` thermal status and headroom through JNI; `None` on a JNI failure (logged once
/// per failure by the caller's change detection staying put).
pub(crate) fn thermal_state() -> Option<crate::services::ThermalState> {
    match jvm::thermal() {
        Ok(state) => Some(state),
        Err(err) => {
            tracing::debug!(target: "regia_platform", "thermal_state: {err}");
            None
        }
    }
}

/// `(paused, destroyed)` from the generated activity; `None` when the call fails (an older
/// activity without `regiaState`, or JNI trouble).
pub(crate) fn activity_state() -> Option<(bool, bool)> {
    match jvm::activity_flags() {
        Ok(bits) => Some((bits & 1 != 0, bits & 2 != 0)),
        Err(err) => {
            tracing::debug!(target: "regia_platform", "activity_state: {err}");
            None
        }
    }
}

/// Logs why the app's recent earlier processes ended (`ApplicationExitInfo`, API 30+), so a
/// native crash, an ANR or a low-memory kill on the previous run leaves a line in this run's log
/// (`logs://`) even though the process died without a bundle. Crash-class reasons warn.
///
/// Then the native crash record (ADR-328): the previous run's record in `<internal>/crash` is
/// collected (a log line and a `logs://` report), and the handler is armed for this run.
pub(crate) fn log_previous_exits() {
    if let Some(internal) = app().and_then(|a| a.internal_data_path()) {
        crate::native_crash::start_records(&internal);
    }
    match jvm::past_exits(5) {
        Ok(exits) => {
            for e in exits {
                let name = crate::services::parse::android_exit_reason(e.reason);
                let line = format!(
                    "previous run ended: {name} (status {}, {} ms since epoch) {}",
                    e.status, e.timestamp_ms, e.description
                );
                if crate::services::parse::android_exit_is_failure(e.reason) {
                    tracing::warn!(target: "regia_platform", "{line}");
                } else {
                    tracing::info!(target: "regia_platform", "{line}");
                }
            }
        }
        Err(err) => tracing::debug!(target: "regia_platform", "past_exits: {err}"),
    }
}

/// TalkBack adapter (`accesskit` feature, ADR-3454). `platform/mod.rs` re-exports it as
/// `accesskit_adapter` on Android. The module is `pub(crate)` because a private module cannot be
/// re-exported.
#[cfg(feature = "accesskit-adapters")]
pub(crate) mod accessibility;
#[cfg(feature = "accesskit-adapters")]
pub(crate) use accessibility as accesskit_adapter;

/// Android keeps one task per app (the activity's launch mode): no single-instance lock file
/// (ADR-8833).
pub(crate) fn instance_lock_dir() -> Option<(std::path::PathBuf, String)> {
    None
}

/// Whether process `pid` is alive (`kill(pid, 0)`; EPERM: alive under another user).
pub(crate) fn process_alive(pid: u32) -> Option<bool> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    // SAFETY: signal 0 only checks that the pid exists; no signal is sent.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Some(true);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Some(false),
        Some(libc::EPERM) => Some(true),
        _ => None,
    }
}

/// The OS brings the app's task forward itself.
pub(crate) fn raise_instance_window(_pid: u32) -> bool {
    false
}

/// The activity's window is the OS's: the fast exit (ADR-8833) leaves it to the process end.
#[cfg(has_window)]
pub(crate) fn hide_window_now(_window: &winit::window::Window) -> &'static str {
    "android: left to the process end"
}
