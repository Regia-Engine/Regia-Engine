// SPDX-License-Identifier: MPL-2.0
//! Per-OS code behind one seam (`docs/04-conventions.md` § 8).
//!
//! Every platform module provides the same `pub(crate)` items: `build_event_loop`, `init`,
//! `default_vfs`, `display_luminance`, `open_url`, `system_locale`, `system_captions`, `os_info`, `ClipboardBackend`, `query_safe_area`,
//! `thermal_state`, `activity_state`, `vibrate`, `set_keep_screen_on`, `set_soft_input_visible`,
//! `set_ime_cursor_area` (ADR-7208),
//! `system_dark_appearance`, `exit_immediately`, `FIXED_WINDOW` (ADR-7880), and the frame pacing tick source: `VsyncThread`,
//! `start_vsync_thread`, `monotonic_ns` and `set_frame_rate_hint` (ADR-1942); `instance_lock_dir`,
//! `process_alive`, `raise_instance_window` and `hide_window_now` (ADR-8833).

#[cfg(target_os = "android")]
mod android;
#[cfg(not(target_os = "android"))]
mod desktop;
#[cfg(all(unix, not(any(target_os = "android", target_os = "macos"))))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

/// Whether the OS window is the whole screen (ADR-7880): read by the window layer's size and mode
/// planning.
#[cfg(all(target_os = "android", has_window))]
pub(crate) use android::FIXED_WINDOW;
/// The metered-network query (`crate::network`); Android only.
#[cfg(target_os = "android")]
pub(crate) use android::active_network_metered;
#[cfg(target_os = "android")]
pub(crate) use android::battery_power;
/// Android app cache dir (`Context.getCacheDir()` with the internal fallback); Android only.
#[cfg(target_os = "android")]
pub(crate) use android::cache_dir_for;
#[cfg(target_os = "android")]
pub(crate) use android::exit_process as exit_immediately;
/// The host directory of `user://` (`crate::paths::user_dir`); Android only.
#[cfg(target_os = "android")]
pub(crate) use android::internal_data_path;
/// The launching intent's string extras (ADR-8472).
#[cfg(target_os = "android")]
pub use android::launch_intent_string_extra;
/// The display density as a scale factor (ADR-5379); Android only, where winit misses live changes.
#[cfg(target_os = "android")]
pub(crate) use android::system_display_scale;
/// The Settings font size (ADR-4932); Android only, other platforms keep `SystemTextScale` at 1.
#[cfg(target_os = "android")]
pub(crate) use android::system_font_scale;
/// The frame loop's scheduler counters for the pacing trace (ADR-5946).
#[cfg(target_os = "android")]
pub(crate) use android::thread_sched_sample;
#[cfg(target_os = "android")]
pub use android::{AndroidApp, AndroidAppHandle, ApkAssetMount, exit_process};
#[cfg(target_os = "android")]
pub(crate) use android::{
    ClipboardBackend, VsyncThread, activity_state, build_event_loop, default_vfs, display_luminance, init,
    log_previous_exits, microphone_permission, monotonic_ns, open_url, os_info, query_safe_area,
    request_microphone_permission, set_frame_rate_hint, set_ime_cursor_area, set_keep_screen_on,
    set_soft_input_visible, start_vsync_thread, system_captions, system_dark_appearance, system_locale,
    thermal_state, vibrate,
};
#[cfg(target_os = "android")]
pub use android::{GamepadDeviceInfo, enumerate_gamepads, gamepad_rumble};
/// The frame loop's performance hint session (ADR-5461).
#[cfg(target_os = "android")]
pub(crate) use android::{PerfHintSession, current_thread_id};
/// Store asset packs (`crate::asset_packs`); Android only.
#[cfg(target_os = "android")]
pub(crate) use android::{asset_pack_call, asset_pack_location, asset_pack_state};
#[cfg(all(not(target_os = "android"), has_window))]
pub(crate) use desktop::FIXED_WINDOW;
#[cfg(not(target_os = "android"))]
pub(crate) use desktop::{
    ClipboardBackend, activity_state, default_vfs, display_luminance, init, log_previous_exits,
    microphone_permission, open_url, os_info, query_safe_area, request_microphone_permission,
    set_keep_screen_on, system_captions, system_locale, thermal_state, vibrate,
};
#[cfg(not(target_os = "android"))]
pub use desktop::{GamepadDeviceInfo, enumerate_gamepads, gamepad_rumble};
#[cfg(all(not(target_os = "android"), has_window))]
pub(crate) use desktop::{
    PerfHintSession, VsyncThread, build_event_loop, current_thread_id, monotonic_ns, set_frame_rate_hint,
    set_ime_cursor_area, set_soft_input_visible, start_vsync_thread, system_dark_appearance,
    thread_sched_sample,
};
#[cfg(all(unix, not(any(target_os = "android", target_os = "macos"))))]
pub(crate) use linux::exit_immediately;
#[cfg(target_os = "macos")]
pub(crate) use macos::exit_immediately;
#[cfg(windows)]
pub(crate) use windows::{cpu_model, exit_immediately, saved_games_dir};

/// Sets the desktop application id on new windows (Wayland `app_id`, X11 `WM_CLASS`).
#[cfg(all(has_window, unix, not(any(target_os = "android", target_os = "macos"))))]
pub(crate) use linux::with_app_id;

/// Windows, macOS and Android have no per-window application id; the attributes are unchanged.
#[cfg(all(
    has_window,
    not(all(unix, not(any(target_os = "android", target_os = "macos"))))
))]
pub(crate) fn with_app_id(
    attrs: winit::window::WindowAttributes,
    _app_id: &str,
) -> winit::window::WindowAttributes {
    attrs
}

/// TalkBack (ADR-3454). rustfmt sorts this `use` ahead of the desktop adapters.
#[cfg(all(feature = "accesskit-adapters", target_os = "android"))]
pub(crate) use android::accesskit_adapter;
/// The screen-reader bridge's OS adapter (`accesskit` feature, ADR-1998): AT-SPI on Linux, UI
/// Automation on Windows, NSAccessibility on macOS.
#[cfg(all(feature = "accesskit-adapters", target_os = "linux"))]
pub(crate) use linux::accesskit_adapter;
#[cfg(all(feature = "accesskit-adapters", target_os = "macos"))]
pub(crate) use macos::accesskit_adapter;
#[cfg(all(feature = "accesskit-adapters", windows))]
pub(crate) use windows::accesskit_adapter;

/// No OS adapter here (ADR-1998 § 6; Android is ADR-3454): the bridge and its platform tree still
/// work, and the agent reports why nothing reaches the OS.
#[cfg(all(
    feature = "accesskit-adapters",
    not(any(target_os = "linux", target_os = "macos", windows, target_os = "android"))
))]
pub(crate) mod accesskit_adapter {
    use crate::accessibility::{AccessKitBridge, OsAccessKitAdapter};

    /// The adapter's name in the agent's report.
    pub(crate) const NAME: &str = "none";
    /// Nothing to attach, so the window is never hidden for it.
    pub(crate) const CREATE_HIDDEN: bool = false;

    /// Always refuses: AccessKit has no adapter wired up for this OS yet.
    pub(crate) fn create(
        _window: &winit::window::Window,
        _bridge: &AccessKitBridge,
    ) -> Result<Box<dyn OsAccessKitAdapter>, String> {
        Err("no AccessKit adapter for this OS yet (ADR-1998)".to_owned())
    }
}

/// Why the event loop could not be created.
#[cfg(has_window)]
#[derive(Debug, thiserror::Error)]
pub(crate) enum EventLoopSetupError {
    /// winit refused.
    #[error(transparent)]
    Winit(#[from] winit::error::EventLoopError),
    /// Android only: the app was not started through `regia::run_android`.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    #[error("AndroidAppHandle resource missing; start the app with regia::run_android")]
    MissingAndroidApp,
    /// macOS only: winit requires the event loop on the main thread, and `REGIA_WINIT_ANY_THREAD`
    /// cannot lift that.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    #[error(
        "on macOS the event loop must be created on the main thread (REGIA_WINIT_ANY_THREAD has no effect)"
    )]
    NotMainThread,
}

/// Stable file identity through an open Windows handle.
#[cfg(windows)]
pub use windows::open_file_identity;
/// No-replace file move (ADR-8504).
#[cfg(windows)]
pub use windows::rename_no_replace;

#[cfg(all(windows, has_window))]
pub(crate) use windows::install_alt_menu;

// The host's UTC offset for wall-clock display (ADR-8586).
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
mod local_time;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
pub(crate) use local_time::local_utc_offset_seconds;
#[cfg(windows)]
pub(crate) use windows::local_utc_offset_seconds;

/// No time zone reader on this target: wall times are shown in UTC.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "android", windows)))]
pub(crate) fn local_utc_offset_seconds(_unix_secs: i64) -> Option<i32> {
    None
}

// The single-instance lock and the fast exit's per-OS parts (ADR-8833).
#[cfg(all(target_os = "android", has_window))]
pub(crate) use android::hide_window_now;
#[cfg(target_os = "android")]
pub(crate) use android::{instance_lock_dir, process_alive, raise_instance_window};
#[cfg(all(has_window, unix, not(any(target_os = "android", target_os = "macos"))))]
pub(crate) use linux::hide_window_now;
#[cfg(all(unix, not(any(target_os = "android", target_os = "macos"))))]
pub(crate) use linux::{instance_lock_dir, process_alive, raise_instance_window};
#[cfg(all(target_os = "macos", has_window))]
pub(crate) use macos::hide_window_now;
#[cfg(target_os = "macos")]
pub(crate) use macos::{instance_lock_dir, process_alive, raise_instance_window};
#[cfg(all(windows, has_window))]
pub(crate) use windows::hide_window_now;
#[cfg(windows)]
pub(crate) use windows::{instance_lock_dir, process_alive, raise_instance_window};
