// SPDX-License-Identifier: MPL-2.0
//! Windowing, event loop, surface lifecycle, VFS, Android glue.
//!
//! Specification: `docs/specs/11-platform-and-input.md` §§ 1–5. [`PlatformPlugin`] installs the winit
//! runner, which creates the primary window on the first resume, turns OS callbacks into
//! [`LifecycleEvent`]s (running the `Lifecycle` schedule synchronously, ADR-012 § 2) and window/input
//! messages, and runs one frame per redraw. It also provides the [`Vfs`] and the platform services
//! ([`OsInfo`], [`Clipboard`], [`SafeArea`], [`Haptics`], [`KeepScreenOn`], [`PowerState`],
//! [`open_url`], [`system_locale`]).
//!
//! The default feature `window` owns winit, the runner and the clipboard backend. Without it (the
//! `server` build, spec 76 § 2.1) [`PlatformPlugin`] installs the [`Vfs`], the services and crash
//! handling and leaves the app's runner alone; Android always has the window half.
//!
//! # Getting started
//!
//! A game reads and writes files through [`VfsPath`]s such as `user://saves/slot1.sav`, never host
//! paths. [`PlatformPlugin`] mounts the platform's directories; a test or tool mounts its own. This
//! example mounts a temporary directory as `user://` and saves a file atomically:
//!
//! ```
//! use regia_platform::{DirMount, MountPoint, Vfs, VfsPath};
//!
//! let dir = std::env::temp_dir().join(format!("regia-platform-doc-{}", std::process::id()));
//! let mut vfs = Vfs::new();
//! vfs.mount(MountPoint::User, DirMount::writable(&dir));
//!
//! let save = VfsPath::parse("user://saves/slot1.sav")?;
//! assert!(!vfs.exists(&save));
//! vfs.write_atomic(&save, b"level=3")?;
//! assert_eq!(&*vfs.read_sync(&save)?, b"level=3");
//!
//! // Packaged content is read-only: writing there is refused.
//! assert!(!MountPoint::Assets.is_writable());
//! # std::fs::remove_dir_all(&dir)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! With a window, the game adds the plugin to an `App` (`app.add_plugins(PlatformPlugin)`) and the
//! runner owns the event loop; that path needs a display, so it is not run here.

// Listed in docs/04-conventions.md § 5: Android JNI / AAssetManager and Win32 calls. Every `unsafe`
// block carries a `// SAFETY:` comment.
#![allow(unsafe_code)]

pub mod asset_packs;
pub mod background;
// Child processes that end with their parent (play-in-editor separate processes, ADR-2682).
pub mod child_bind;
pub mod console_ctrl;
pub mod crash;
mod deliberate_exit;
mod events;
#[cfg(has_window)]
mod exit;
pub mod frame_pacing;
mod gamepad;
mod gpu_worker_registry;
#[cfg(not(target_os = "android"))]
pub mod hid;
mod lifecycle;
pub mod lifecycle_drill;
mod locale_hints;
pub mod native_crash;
pub mod paths;
mod platform;
#[cfg(has_window)]
mod runner;
mod services;
pub mod sleep;
mod vfs;
#[cfg(any(windows, test))]
mod wide;
mod window;
#[cfg(has_window)]
mod window_os;
pub mod xr;
// The screen-reader OS bridge, platform half (ADR-1998); documented by its inner docs.
#[cfg(feature = "accesskit-adapters")]
pub mod accessibility;
// Resident memory from the OS, Windows included (n150-minspec-pass, ADR-3288).
mod process_memory;
// Whether the active network is metered (spec 40 § 2.5, ADR-7236).
pub mod network;

pub mod alt_menu;
pub mod stack_sampling;
// One running instance per user and app (ADR-8833).
mod single_instance;
// Hide, bounded flush, `_exit` (ADR-8833).
#[cfg(has_window)]
mod fast_exit;

pub use process_memory::{ProcessMemory, process_memory};

use regia_app::prelude::*;

pub use deliberate_exit::{
    DeliberateExit, begin_deliberate_exit, deliberate_exit_code, finish_deliberate_exit, finish_exit,
};
pub use events::{
    BackPressed, ButtonState, ClosePolicy, CursorMoved, FileDropped, ImeEvent, InputTimestamp, InputTiming,
    InputTimingPlugin, KeyboardInput, MouseButton, MouseButtonInput, MouseMotion, MouseWheel, QuitRequest,
    SystemThemeChanged, TouchInput, TouchPhase, WheelUnit, WindowCloseRequested, WindowFocused,
    WindowResized, WindowScaleFactorChanged, input_epoch, request_quit,
};
#[cfg(has_window)]
pub use exit::{
    CleanExitReport, CleanExitSettings, DoneHook, ExitHook, ExitHooks, ExitStep, ExitStepTiming,
    ExitWatchdog, FullscreenWindow, arm_exit_watchdog, clean_exit_progress, leave_fullscreen,
    request_clean_exit, run_clean_exit,
};
#[cfg(has_window)]
pub use fast_exit::{
    ExitMode, FAST_EXIT_DEADLINE_CODE, FastExitReport, FastExitRequest, FastExitSettings, FastHideHook,
    FlushHook, FlushJob, FlushOutcome, HideableWindow, exit_mode, fast_exit_due, fast_exit_requested,
    finish_fast_exit, hide_windows, keep_file_on_fast_exit, poll_fast_exit, request_fast_exit, run_bounded,
    run_fast_exit_flush,
};
pub use single_instance::{
    ALLOW_MULTI_INSTANCE_ENV, EXIT_ALREADY_RUNNING, SingleInstanceError, SingleInstanceLock,
    instance_lock_path, multi_instance_allowed,
};
/// Present while [`run_clean_exit`] runs (until the world is cleared): the lifecycle systems can
/// tell an exit sequence's `WillTerminate` from a bare one and leave the GPU teardown to the
/// sequence's later steps.
///
/// Defined outside the `has_window` gate (the marker has no window dependency) so headless
/// consumers of this crate — a dedicated server reading its client's exit state — see it too
/// (corr-low, ADR-5789).
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct CleanExitInProgress;
pub use gamepad::{
    GamepadAxis, GamepadAxisChanged, GamepadButton, GamepadButtonChanged, GamepadConnected,
    GamepadDisconnected, RumbleRequest,
};
// GPU worker threads the end of the process waits for (ADR-287).
pub use gpu_worker_registry::{
    DriverCall, EXIT_WORKER_WAIT, EXIT_WORKER_WAIT_ENV, GpuWorker, GpuWorkerBuilder, GpuWorkers, LeftWorker,
    gpu_workers, note_exit_started, parse_exit_worker_wait,
};
pub use lifecycle::{AppLifecycle, LifecycleEvent, LifecycleState, LifecycleTracker, apply_transition};
pub use lifecycle_drill::{LifecycleDrill, SurfaceAudit, SurfaceObservation, record_surface_observation};
pub use locale_hints::{LOCALE_ENV, LOCALE_FLAG, LocaleHints, flag_from_args};
/// The launching intent's string extras (the editor's dev launch options, ADR-8472).
#[cfg(target_os = "android")]
pub use platform::launch_intent_string_extra;
/// Windows volume serial and file index for an open handle.
#[cfg(windows)]
pub use platform::open_file_identity;
/// Moves a file to a name only when that name is free (`MoveFileExW` without replace): the
/// Windows way to publish a new file atomically without a hard link (ADR-8504).
#[cfg(windows)]
pub use platform::rename_no_replace;
#[cfg(target_os = "android")]
pub use platform::{AndroidApp, AndroidAppHandle, ApkAssetMount, exit_process};
/// Gamepad device discovery and rumble through the platform seam (Android: `InputManager` JNI;
/// desktop: handled by gilrs in `regia_input`).
pub use platform::{GamepadDeviceInfo, enumerate_gamepads, gamepad_rumble};
#[cfg(has_window)]
pub use runner::{FocusedWindow, RunnerSettings, SoftwareRenderer, SurfacesHidden, SystemSleepStats};
#[cfg(has_window)]
pub use runner::{OCCLUDED_ACQUIRES_HIDE, OccludedAcquires};
pub use services::local_utc_offset_seconds;
#[cfg(not(target_os = "android"))]
pub use services::open_path;
pub use services::{
    ActivityState, Clipboard, DisplayLuminance, Haptics, KeepScreenOn, LINUX_POWER_SUPPLY_CLASS,
    LOW_POWER_BELOW_PERCENT, OsInfo, PauseOnFocusLoss, PowerSource, PowerState, SafeArea, ServiceError,
    SystemTextScale, ThermalOverride, ThermalState, ThermalStatus, display_luminance, locale_from_env,
    open_url, power_state, read_power_supply_dir, system_captions, system_locale,
};
pub use services::{
    MicrophonePermission, microphone_permission, request_microphone_permission, resolve_microphone_permission,
};
pub use vfs::{
    DirMount, MountPoint, Vfs, VfsDirEntry, VfsError, VfsMount, VfsPath, VfsPathError, VfsReader,
    is_reserved_device_name,
};
// `.fpak` mounts: the opener `regia_asset` installs (ADR-7232).
pub use vfs::{PakMountOpener, install_pak_mount_opener, pak_mounts_available};
// File work through the VFS, read by `app.startup_work` (ADR-2987).
pub use vfs::{VfsStats, stats::stats as vfs_stats};
pub use window::{
    CursorGrab, ImeCursorArea, PresentMode, PrimaryWindow, Window, WindowMode, WindowSettings, default_app_id,
};
// The cursor hand-over to a system overlay and its call counts (ADR-7349).
pub use window::{CursorCalls, CursorRelease};
#[cfg(has_window)]
pub use window::{DisplayHandle, WindowHandles};
/// The OS window behind a [`Window`] entity (see [`WindowHandles`]).
#[cfg(has_window)]
pub use winit::window::Window as OsWindow;

// Window size and mode requests: the monitor clamp, refusals and their codes (ADR-7880).
pub use window::{
    MIN_WINDOW_LOGICAL_SIZE, MonitorExtent, WINDOW_MODE_FIXED, WINDOW_SIZE_FIXED, WINDOW_SIZE_INVALID,
    WINDOW_SIZE_TOO_SMALL, WindowRefusal, WindowStatus, orient_monitor, plan_window_mode, plan_window_size,
};

/// Installs the winit runner, lifecycle handling, window/input messages, the [`Vfs`] and the
/// platform service resources.
///
/// A [`Vfs`] inserted before this plugin is kept; otherwise the platform's default mounts are used.
#[derive(Default)]
pub struct PlatformPlugin;

impl Plugin for PlatformPlugin {
    fn build(&self, app: &mut App) {
        platform::init(app.world());
        if !app.world().contains_resource::<Vfs>() {
            let vfs = Vfs::with_default_mounts(app.world());
            tracing::info!(target: "regia_platform", "vfs mounts: {vfs:?}");
            app.insert_resource(vfs);
        }
        let os_info = OsInfo::detect();
        let locale = system_locale().unwrap_or_else(|| "unknown".into());
        tracing::info!(
            target: "regia_platform",
            "os: {} {} | device: {} | ram: {} MiB | cpus: {} | locale: {}",
            os_info.os,
            os_info.version,
            os_info.device_model,
            os_info.total_ram_bytes / (1024 * 1024),
            os_info.cpu_count,
            locale,
        );

        // Crash reporting (specs/21 § 5): point the panic hook at `logs://` and record the OS
        // facts, then keep the frame count fresh for any later report.
        crash::configure(
            app.world()
                .get_resource::<Vfs>()
                .and_then(|vfs| vfs.host_path(&VfsPath::root(MountPoint::Logs))),
            crash::BuildInfo::detect(),
            os_info.clone(),
        );
        crash::install_panic_hook();
        platform::log_previous_exits();

        events::register(app);
        gamepad::register(app);
        app.init_resource::<AppTypeRegistry>();
        app.init_resource::<frame_pacing::FramePacingSettings>();
        app.init_resource::<frame_pacing::FramePacingReport>();
        app.init_resource::<frame_pacing::FramePacingTrace>();
        app.init_resource::<frame_pacing::FramePerfHint>();
        app.init_resource::<frame_pacing::FrameWakeAhead>();
        app.init_resource::<frame_pacing::FrameSteadyStart>();
        {
            let registry = app.world().resource::<AppTypeRegistry>().clone();
            let mut registry = registry.write();
            registry.register::<Window>();
            registry.register::<PrimaryWindow>();
            registry.register::<Name>();
        }
        app.add_message::<LifecycleEvent>()
            .insert_resource(os_info)
            .init_resource::<AppLifecycle>()
            .init_resource::<LifecycleDrill>()
            .init_resource::<SurfaceAudit>()
            .init_resource::<WindowSettings>()
            .init_resource::<CursorRelease>()
            .init_resource::<CursorCalls>()
            .init_resource::<Clipboard>()
            .init_resource::<SafeArea>()
            .init_resource::<Haptics>()
            .init_resource::<KeepScreenOn>()
            .init_resource::<PauseOnFocusLoss>()
            .init_resource::<ImeCursorArea>()
            .init_resource::<ThermalState>()
            .init_resource::<ThermalOverride>()
            .init_resource::<ActivityState>()
            .add_systems(Lifecycle, services::refresh_safe_area_on_resume)
            .add_systems(
                PreUpdate,
                (
                    services::refresh_safe_area_on_change,
                    services::poll_thermal,
                    services::poll_activity,
                    // `pause_on_focus_loss` (spec 11 § 2, ADR-7070).
                    services::pause_on_focus_loss,
                ),
            )
            .add_systems(
                PostUpdate,
                (services::apply_keep_screen_on, crash::update_frame_count),
            )
            .add_systems(Last, events::handle_close_requests);
        // The window half: without it a headless host drives the app itself.
        #[cfg(has_window)]
        app.init_resource::<WindowHandles>()
            .init_resource::<RunnerSettings>()
            .init_resource::<SurfacesHidden>()
            .init_resource::<OccludedAcquires>()
            .init_resource::<FocusedWindow>()
            .add_systems(
                PostUpdate,
                (
                    window::apply_window_changes,
                    services::sync_soft_input.after(window::apply_window_changes),
                    window::apply_ime_cursor_area.after(window::apply_window_changes),
                ),
            )
            .set_runner(runner::run);
        // The screen-reader bridge's OS adapter (ADR-1998); nothing runs without a bridge resource.
        #[cfg(feature = "accesskit")]
        app.add_systems(Last, accessibility::drive_accesskit_adapter);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_registers_vfs_and_services() {
        let mut app = App::new();
        app.add_plugins(PlatformPlugin);
        let world = app.world();
        for resource in [
            world.contains_resource::<Vfs>(),
            world.contains_resource::<OsInfo>(),
            world.contains_resource::<Clipboard>(),
            world.contains_resource::<SafeArea>(),
            world.contains_resource::<Haptics>(),
            world.contains_resource::<KeepScreenOn>(),
        ] {
            assert!(resource);
        }
        // The runner writes these on window events (`resource_mut`), so the plugin must add them.
        #[cfg(has_window)]
        for resource in [
            world.contains_resource::<WindowHandles>(),
            world.contains_resource::<RunnerSettings>(),
            world.contains_resource::<SurfacesHidden>(),
            world.contains_resource::<OccludedAcquires>(),
            world.contains_resource::<FocusedWindow>(),
        ] {
            assert!(resource);
        }
        let vfs = world.resource::<Vfs>();
        assert!(vfs.is_mounted(MountPoint::User) && vfs.is_mounted(MountPoint::Cache));
        assert!(vfs.is_mounted(MountPoint::Logs));
        assert_eq!(world.resource::<OsInfo>().os, std::env::consts::OS);
        // Systems run headless: keep-screen-on, soft input and safe-area refresh are inert on desktop.
        app.world_mut().spawn((Window::default(), PrimaryWindow));
        app.world_mut().resource_mut::<KeepScreenOn>().0 = true;
        app.update();
        app.update();
        assert_eq!(*app.world().resource::<SafeArea>(), SafeArea::default());
    }

    #[test]
    fn plugin_keeps_a_preinserted_vfs() {
        let mut app = App::new();
        app.insert_resource(Vfs::new());
        app.add_plugins(PlatformPlugin);
        assert!(!app.world().resource::<Vfs>().is_mounted(MountPoint::User));
    }
}
