// SPDX-License-Identifier: MPL-2.0
//! Linux, Windows and macOS. OS-specific services live in `linux.rs`, `windows.rs` and
//! `macos.rs`; this module holds what the desktop platforms share.

use std::time::Duration;

use regia_app::prelude::World;
#[cfg(has_window)]
use winit::event_loop::EventLoop;

#[cfg(has_window)]
use super::EventLoopSetupError;
use crate::services::{DisplayLuminance, SafeArea, ServiceError};
use crate::vfs::{DirMount, MountPoint, Vfs, resolve_assets_root};

#[cfg(all(unix, not(any(target_os = "android", target_os = "macos"))))]
pub(crate) use super::linux::{open_url, os_info, system_locale};
#[cfg(target_os = "macos")]
pub(crate) use super::macos::{open_url, os_info, system_locale};
#[cfg(windows)]
pub(crate) use super::windows::{open_url, os_info, system_captions, system_locale};

/// Desktop Linux and macOS have no setting for it.
#[cfg(not(windows))]
pub(crate) fn system_captions() -> Option<bool> {
    None
}

#[cfg(has_window)]
pub(crate) fn build_event_loop(_world: &mut World) -> Result<EventLoop<()>, EventLoopSetupError> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `pthread_main_np` takes no arguments and only reads thread state.
        if unsafe { libc::pthread_main_np() } == 0 {
            return Err(EventLoopSetupError::NotMainThread);
        }
    }
    let mut builder = EventLoop::builder();
    if regia_core::brand::env_var_os("REGIA_WINIT_ANY_THREAD").is_some() {
        apply_winit_any_thread(&mut builder);
    }
    Ok(builder.build()?)
}

#[cfg(all(has_window, target_os = "linux"))]
fn apply_winit_any_thread<T>(builder: &mut winit::event_loop::EventLoopBuilder<T>) {
    use winit::platform::wayland::EventLoopBuilderExtWayland;
    EventLoopBuilderExtWayland::with_any_thread(builder, true);
}

#[cfg(all(has_window, windows))]
fn apply_winit_any_thread<T>(builder: &mut winit::event_loop::EventLoopBuilder<T>) {
    use winit::platform::windows::EventLoopBuilderExtWindows;
    builder.with_any_thread(true);
}

#[cfg(all(has_window, not(any(target_os = "linux", windows))))]
fn apply_winit_any_thread<T>(_builder: &mut winit::event_loop::EventLoopBuilder<T>) {}

/// Nothing to set up on desktop.
pub(crate) fn init(_world: &World) {}

/// Desktop development mounts (see the table in `crate::vfs`).
pub(crate) fn default_vfs(_world: &World) -> Vfs {
    let mut vfs = Vfs::new();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let assets = resolve_assets_root(
        regia_core::brand::env_var_os("REGIA_ASSETS").map(Into::into),
        exe_dir,
        regia_core::brand::env_var_os("CARGO_MANIFEST_DIR").map(Into::into),
        |p| p.is_dir(),
    );
    match assets {
        Some(assets) => {
            vfs.mount(MountPoint::Config, DirMount::read_only(assets.join("config")));
            vfs.mount(MountPoint::Assets, DirMount::read_only(assets));
        }
        None => tracing::warn!(target: "regia_platform", "vfs: no assets directory; assets:// unmounted"),
    }
    match crate::paths::user_dir() {
        Some(user) => {
            vfs.mount(
                MountPoint::Logs,
                DirMount::read_only(user.join("logs")).without_symlinks(),
            );
            vfs.mount(MountPoint::User, DirMount::writable(user));
        }
        None => tracing::warn!(target: "regia_platform", "vfs: no user data directory; user:// unmounted"),
    }
    match crate::paths::cache_dir() {
        Some(cache) => vfs.mount(MountPoint::Cache, DirMount::writable(cache)),
        None => tracing::warn!(target: "regia_platform", "vfs: no cache directory; cache:// unmounted"),
    }
    vfs
}

#[cfg(has_window)]
pub(crate) use clipboard::ClipboardBackend;

/// No clipboard without the `window` feature (a headless server).
#[cfg(not(has_window))]
#[derive(Default)]
pub(crate) struct ClipboardBackend;

#[cfg(not(has_window))]
impl ClipboardBackend {
    pub(crate) fn get_text(&self) -> Result<String, ServiceError> {
        Err(ServiceError::Unsupported("clipboard unavailable"))
    }

    pub(crate) fn set_text(&self, _text: &str) -> Result<(), ServiceError> {
        Err(ServiceError::Unsupported("clipboard unavailable"))
    }
}

#[cfg(has_window)]
mod clipboard {
    use std::sync::{Mutex, PoisonError};

    use crate::services::ServiceError;

    /// The system clipboard through `arboard`, opened on first use (so headless runs never touch it).
    #[derive(Default)]
    pub(crate) struct ClipboardBackend {
        clipboard: Mutex<Option<arboard::Clipboard>>,
    }

    impl ClipboardBackend {
        fn with<T>(
            &self,
            operation: &'static str,
            f: impl FnOnce(&mut arboard::Clipboard) -> Result<T, arboard::Error>,
        ) -> Result<T, ServiceError> {
            let os_err = |e: arboard::Error| ServiceError::Os {
                operation,
                message: e.to_string(),
            };
            let mut guard = self.clipboard.lock().unwrap_or_else(PoisonError::into_inner);
            if guard.is_none() {
                *guard = Some(arboard::Clipboard::new().map_err(os_err)?);
            }
            match guard.as_mut() {
                Some(clipboard) => f(clipboard).map_err(os_err),
                None => Err(ServiceError::Unsupported("clipboard unavailable")),
            }
        }

        pub(crate) fn get_text(&self) -> Result<String, ServiceError> {
            self.with("clipboard get_text", |c| c.get_text())
        }

        pub(crate) fn set_text(&self, text: &str) -> Result<(), ServiceError> {
            self.with("clipboard set_text", |c| c.set_text(text))
        }
    }
}

/// Desktop windows have no cutouts or system-bar overlays: the safe area stays at zero.
pub(crate) fn query_safe_area() -> Option<SafeArea> {
    None
}

/// No vibration hardware on desktop (gamepad rumble is `regia_input`, M7).
pub(crate) fn vibrate(_duration: Duration, _amplitude: f32) -> Result<(), ServiceError> {
    Ok(())
}

/// No effect on desktop.
/// Desktop windows take the size and mode they are asked for, clamped to the monitor (ADR-7880).
#[cfg(has_window)]
pub(crate) const FIXED_WINDOW: bool = false;

pub(crate) fn set_keep_screen_on(_on: bool) {}

/// Desktop platforms have no runtime prompt; a privacy block surfaces as a stream open error.
pub(crate) fn microphone_permission() -> crate::services::MicrophonePermission {
    crate::services::MicrophonePermission::Granted
}

pub(crate) fn request_microphone_permission() {}

/// Desktop IME is handled by winit's `set_ime_allowed`; there is no soft keyboard.
#[cfg(has_window)]
pub(crate) fn set_soft_input_visible(_visible: bool) {}

/// Places the input method's candidate window next to `rect` (`[x, y, w, h]`, physical px from
/// the window's top-left; ADR-7208).
#[cfg(has_window)]
pub(crate) fn set_ime_cursor_area(window: &winit::window::Window, rect: [f32; 4]) {
    window.set_ime_cursor_area(
        winit::dpi::PhysicalPosition::new(f64::from(rect[0]), f64::from(rect[1])),
        winit::dpi::PhysicalSize::new(f64::from(rect[2].max(1.0)), f64::from(rect[3].max(1.0))),
    );
}

/// No input-device query on desktop: gamepad discovery is gilrs' job (`regia_input`, M7).
#[allow(dead_code)] // part of the platform seam `docs/04-conventions.md` § 8
pub struct GamepadDeviceInfo {
    /// Backend device id.
    pub id: i32,
    /// Device name.
    pub name: String,
}

/// Always empty on desktop (gamepads come from gilrs in `regia_input`).
pub fn enumerate_gamepads() -> Vec<GamepadDeviceInfo> {
    Vec::new()
}

/// Desktop gamepad rumble goes through gilrs in `regia_input`; nothing to do here.
pub fn gamepad_rumble(_device_id: i32, _strength: f32, _duration: Duration) -> Result<(), ServiceError> {
    Err(ServiceError::Unsupported(
        "desktop rumble is handled by regia_input (gilrs)",
    ))
}

/// Desktop appearance comes from winit (`Window::theme`, `WindowEvent::ThemeChanged`, Windows and
/// macOS); there is no second source here.
#[cfg(has_window)]
pub(crate) fn system_dark_appearance() -> Option<bool> {
    None
}

/// Desktop winit exposes no display luminance; the renderer uses its defaults.
pub(crate) fn display_luminance() -> DisplayLuminance {
    DisplayLuminance::default()
}

/// Desktop has no OS thermal API worth polling here; the governor stays idle.
pub(crate) fn thermal_state() -> Option<crate::services::ThermalState> {
    None
}

/// Desktop has no vsync tick source yet: frames run on the software cap and the swapchain.
#[cfg(has_window)]
pub(crate) struct VsyncThread;

#[cfg(has_window)]
impl VsyncThread {
    /// Never called: [`start_vsync_thread`] makes none.
    pub(crate) fn set_active(&self, _active: bool) {}

    /// Never called: [`start_vsync_thread`] makes none.
    pub(crate) fn thread_id(&self) -> Option<i32> {
        None
    }
}

/// No performance hint API on the desktop (ADR-5461): [`PerfHintSession::open`] gives `None`.
#[cfg(has_window)]
pub(crate) struct PerfHintSession;

#[cfg(has_window)]
impl PerfHintSession {
    /// Always `None` here.
    pub(crate) fn open(_threads: &[i32], _target_ns: i64) -> Option<Self> {
        None
    }

    /// Never called: [`Self::open`] makes none.
    pub(crate) fn set_target(&mut self, _target_ns: i64) -> bool {
        false
    }

    /// Never called: [`Self::open`] makes none.
    pub(crate) fn report(&mut self, _actual_ns: i64) -> bool {
        false
    }
}

/// No thread ids are needed without a performance hint API.
#[cfg(has_window)]
pub(crate) fn current_thread_id() -> i32 {
    0
}

/// No scheduler counters in the pacing trace here (ADR-5946): `None`.
#[cfg(has_window)]
pub(crate) fn thread_sched_sample() -> Option<crate::frame_pacing::SchedSample> {
    None
}

/// No tick source (`platform/mod.rs` seam).
#[cfg(has_window)]
pub(crate) fn start_vsync_thread(
    _ticks: std::sync::Arc<crate::frame_pacing::VsyncTicks>,
) -> Option<VsyncThread> {
    None
}

/// Unused without a tick source.
#[cfg(has_window)]
pub(crate) fn monotonic_ns() -> u64 {
    0
}

/// Desktop compositors have no per-window frame rate preference here.
#[cfg(has_window)]
pub(crate) fn set_frame_rate_hint(_window: &winit::window::Window, _fps: f32) {}

/// Desktop has no activity; winit's window events cover pause and close.
pub(crate) fn activity_state() -> Option<(bool, bool)> {
    None
}

/// Desktop crashes are caught by the native crash monitor (ADR-158); nothing to read back.
pub(crate) fn log_previous_exits() {}
