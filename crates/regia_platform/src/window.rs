// SPDX-License-Identifier: MPL-2.0
//! Windows (`docs/specs/11-platform-and-input.md` § 1): the data half, which a headless build
//! keeps. The OS half is `window_os`.

use regia_app::prelude::*;
use regia_ecs::reflect::Reflect;
use regia_ecs::reflect::std_traits::ReflectDefault;

/// Settings for the primary window, created on the first resume.
#[derive(Resource, Debug, Clone, Default)]
pub struct WindowSettings {
    /// Initial window description.
    pub window: Window,
}

/// Fullscreen mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[reflect(Default)]
pub enum WindowMode {
    /// A normal window.
    #[default]
    Windowed,
    /// Borderless fullscreen on the current monitor.
    BorderlessFullscreen,
}

/// Presentation mode, read by the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[reflect(Default)]
pub enum PresentMode {
    /// Wait for vertical blank (no tearing).
    #[default]
    Vsync,
    /// Vsync, but late frames may tear.
    VsyncRelaxed,
    /// No waiting (may tear).
    Immediate,
}

/// Cursor grab mode.
///
/// winit 0.30 does not implement every mode on every backend. The window layer rewrites the
/// request before it reaches the OS: [`Locked`](Self::Locked) becomes [`Confined`](Self::Confined)
/// on Windows and on X11 (including Xwayland), and [`Confined`](Self::Confined) becomes
/// [`Locked`](Self::Locked) on macOS. A grab the OS rejects stays pending and is retried on a
/// later frame (`docs/specs/11-platform-and-input.md` § 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[reflect(Default)]
pub enum CursorGrab {
    /// Free cursor.
    #[default]
    None,
    /// Kept inside the window. On macOS this is requested as [`Locked`](Self::Locked).
    Confined,
    /// Locked in place (mouse-look). Falls back to [`Confined`](Self::Confined) on Windows and X11.
    Locked,
}

/// While `true`, every OS window gets a visible, free cursor, whatever its [`Window`] component
/// asks for (ADR-7349). The components keep the game's own `cursor_grab` and `cursor_visible`
/// and the game may keep writing them; the window layer only stops passing them to the OS. When it
/// turns `false`, the components' current values are applied again on the next `PostUpdate`.
///
/// A system overlay that draws inside the game's window and needs the mouse sets it, so the game
/// cannot take the pointer back while the overlay is open: the Steam plugin sets it from
/// `OverlayOpened` to `OverlayClosed`.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CursorRelease(pub bool);

impl CursorRelease {
    /// The grab the OS gets for a window whose component asks for `grab`.
    #[must_use]
    pub fn grab(self, grab: CursorGrab) -> CursorGrab {
        if self.0 { CursorGrab::None } else { grab }
    }

    /// Whether the OS cursor is shown for a window whose component asks for `visible`.
    #[must_use]
    pub fn visible(self, visible: bool) -> bool {
        self.0 || visible
    }
}

/// What the window layer has asked the OS for, counted (ADR-7349): each `set_cursor_grab` and
/// `set_cursor_visible` call the window layer made, and the last value of each. A trace or a test
/// reads it to see whether the game re-grabbed the pointer (for example while the Steam overlay is
/// open). Stays at zero in a build without the window feature.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CursorCalls {
    /// `set_cursor_grab` calls made (refused ones included).
    pub grab_calls: u64,
    /// The grab the last call asked for (before the per-backend rewrite of `Locked`/`Confined`).
    pub last_grab: CursorGrab,
    /// `set_cursor_visible` calls made.
    pub visible_calls: u64,
    /// The value the last `set_cursor_visible` call passed (`false` before the first call).
    pub last_visible: bool,
}

/// A window. Lives on an entity; changes are applied to the OS window in `PostUpdate`.
#[derive(Component, Debug, Clone, Reflect)]
#[reflect(Component, Default)]
pub struct Window {
    /// Title bar text.
    pub title: String,
    /// Requested size in logical pixels.
    pub logical_size: (f64, f64),
    /// Fullscreen mode.
    pub mode: WindowMode,
    /// Presentation mode.
    pub present_mode: PresentMode,
    /// User can resize.
    pub resizable: bool,
    /// OS decorations.
    pub decorations: bool,
    /// Cursor visible over the window.
    pub cursor_visible: bool,
    /// Cursor grab.
    pub cursor_grab: CursorGrab,
    /// IME input enabled.
    pub ime_enabled: bool,
    /// Application id the desktop uses to group, match and place windows (Wayland `app_id`, X11
    /// `WM_CLASS`). `None` = `regia.<executable name>`. Set at window creation only.
    pub app_id: Option<String>,
    /// Current surface size in physical pixels (kept up to date by the platform layer).
    pub physical_size: (u32, u32),
    /// Current scale factor (kept up to date by the platform layer).
    pub scale_factor: f64,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            title: "Regia Engine".into(),
            logical_size: (1280.0, 720.0),
            mode: WindowMode::Windowed,
            present_mode: PresentMode::Vsync,
            resizable: true,
            decorations: true,
            cursor_visible: true,
            cursor_grab: CursorGrab::None,
            ime_enabled: false,
            app_id: None,
            physical_size: (0, 0),
            scale_factor: 1.0,
        }
    }
}

/// Where the focused text field's caret (or IME composition) is, in the window's physical
/// pixels, so the OS can open the input method's candidate window next to it (ADR-7208). The
/// runtime UI writes it every frame a text field is focused; the platform layer hands changes to
/// the OS window (`set_ime_cursor_area` on desktop; Android has no candidate window to place, so
/// it is ignored there).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default)]
pub struct ImeCursorArea {
    /// The window the rect belongs to (`None`: the primary window).
    pub window: Option<Entity>,
    /// `[x, y, w, h]` in physical px from the window's top-left; `None` while no field is focused.
    pub rect: Option<[f32; 4]>,
}

/// Marks the primary window.
#[derive(Component, Debug, Clone, Copy, Default, Reflect)]
#[reflect(Component, Default)]
pub struct PrimaryWindow;

/// `regia.<executable name>`, the default [`Window::app_id`].
pub fn default_app_id() -> String {
    format!("regia.{}", crate::paths::exe_stem())
}

/// The smallest window the window layer asks the OS for, in logical pixels (ADR-7880). A smaller
/// [`Window::logical_size`] is refused with [`WINDOW_SIZE_TOO_SMALL`].
pub const MIN_WINDOW_LOGICAL_SIZE: (f64, f64) = (320.0, 180.0);

/// Refusal code: a [`Window::logical_size`] that is not finite or not positive.
pub const WINDOW_SIZE_INVALID: &str = "window_size_invalid";
/// Refusal code: a [`Window::logical_size`] below [`MIN_WINDOW_LOGICAL_SIZE`] on either axis.
pub const WINDOW_SIZE_TOO_SMALL: &str = "window_size_too_small";
/// Refusal code: the platform's window is the whole screen and cannot be resized (Android).
pub const WINDOW_SIZE_FIXED: &str = "window_size_fixed";
/// Refusal code: the platform's window is always fullscreen, so [`WindowMode::Windowed`] cannot
/// be applied (Android).
pub const WINDOW_MODE_FIXED: &str = "window_mode_fixed";

/// The monitor a window is on, as the OS reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonitorExtent {
    /// Monitor size in physical pixels.
    pub physical_size: (u32, u32),
    /// The monitor's scale factor (physical pixels per logical pixel).
    pub scale_factor: f64,
}

impl MonitorExtent {
    /// The monitor size in logical pixels (physical size over the scale factor; a scale factor
    /// that is not finite and positive counts as 1).
    #[must_use]
    pub fn logical_size(&self) -> (f64, f64) {
        let scale = if self.scale_factor.is_finite() && self.scale_factor > 0.0 {
            self.scale_factor
        } else {
            1.0
        };
        (
            f64::from(self.physical_size.0) / scale,
            f64::from(self.physical_size.1) / scale,
        )
    }
}

/// A size or mode request the window layer did not pass to the OS, and why (ADR-7880). The
/// window keeps the size and mode it had.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowRefusal {
    /// Stable lowercase code: [`WINDOW_SIZE_INVALID`], [`WINDOW_SIZE_TOO_SMALL`],
    /// [`WINDOW_SIZE_FIXED`] or [`WINDOW_MODE_FIXED`].
    pub code: &'static str,
    /// The requested logical size.
    pub requested_size: (f64, f64),
    /// The requested mode.
    pub requested_mode: WindowMode,
}

/// What the window layer last did with a [`Window`]'s size and mode (ADR-7880). The window layer
/// keeps it on every OS window entity; a build without the window feature never writes it.
///
/// A size request is clamped to the monitor the window is on (when the OS reports one) before it
/// reaches the OS, and a size or mode the platform cannot take is refused: the window keeps the
/// old one and [`Self::refused`] says why. [`Window::logical_size`] keeps the request as written.
#[derive(Component, Debug, Clone, Default, PartialEq)]
pub struct WindowStatus {
    /// The monitor the window was on at the last size or mode request (or its creation); `None`
    /// when the OS reports none.
    pub monitor: Option<MonitorExtent>,
    /// The logical size last passed to the OS (after the clamp); `None` until a request.
    pub applied_logical_size: Option<(f64, f64)>,
    /// The mode last passed to the OS; `None` until a mode change.
    pub applied_mode: Option<WindowMode>,
    /// The last refused request; cleared by the next request that is applied.
    pub refused: Option<WindowRefusal>,
    /// Size requests passed to the OS so far.
    pub size_requests: u64,
    /// Mode changes passed to the OS so far.
    pub mode_requests: u64,
}

/// The logical size the window layer asks the OS for when a window requests `requested`
/// (ADR-7880): each axis clamped to `monitor` (the monitor's logical size, when known). `fixed`
/// is a platform whose window is the screen. The error is a refusal code ([`WINDOW_SIZE_FIXED`],
/// [`WINDOW_SIZE_INVALID`], [`WINDOW_SIZE_TOO_SMALL`]); a refused size leaves the window as it is.
pub fn plan_window_size(
    requested: (f64, f64),
    monitor: Option<(f64, f64)>,
    fixed: bool,
) -> Result<(f64, f64), &'static str> {
    if fixed {
        return Err(WINDOW_SIZE_FIXED);
    }
    let (w, h) = requested;
    if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
        return Err(WINDOW_SIZE_INVALID);
    }
    if w < MIN_WINDOW_LOGICAL_SIZE.0 || h < MIN_WINDOW_LOGICAL_SIZE.1 {
        return Err(WINDOW_SIZE_TOO_SMALL);
    }
    let Some((mw, mh)) =
        monitor.filter(|(mw, mh)| mw.is_finite() && mh.is_finite() && *mw >= 1.0 && *mh >= 1.0)
    else {
        return Ok((w, h));
    };
    // A monitor smaller than the minimum still gets a window that fits it.
    Ok((w.min(mw), h.min(mh)))
}

/// The monitor extent a size request is clamped to, given the window's current logical size
/// (ADR-7880). Some platforms report a rotated panel unrotated: the Steam Deck's 800x1280 panel,
/// shown landscape, reads as 800x1280 through winit on Wayland. When the current window does not fit
/// the report but fits it rotated, the rotated extent is the screen; when it fits neither, the
/// report is not trusted and `None` (no clamp) is returned. A window size of zero (not reported
/// yet) cannot tell the orientation either, so it is `None` too: the window layer applies the
/// request unclamped and applies it again once the OS reports a size (ADR-7979), instead of
/// locking a portrait clamp (a Deck's saved 1280x800 cut to width 800) on the first apply.
#[must_use]
pub fn orient_monitor(monitor: (f64, f64), current: (f64, f64)) -> Option<(f64, f64)> {
    if current.0 <= 0.0 || current.1 <= 0.0 {
        return None;
    }
    let fits = |extent: (f64, f64)| current.0 <= extent.0 + 0.5 && current.1 <= extent.1 + 0.5;
    if fits(monitor) {
        Some(monitor)
    } else if fits((monitor.1, monitor.0)) {
        Some((monitor.1, monitor.0))
    } else {
        None
    }
}

/// Whether the window layer passes `mode` to the OS: `fixed` is a platform whose window is always
/// fullscreen, where [`WindowMode::Windowed`] is refused with [`WINDOW_MODE_FIXED`].
pub fn plan_window_mode(mode: WindowMode, fixed: bool) -> Result<WindowMode, &'static str> {
    match mode {
        WindowMode::Windowed if fixed => Err(WINDOW_MODE_FIXED),
        mode => Ok(mode),
    }
}

#[cfg(has_window)]
pub use crate::window_os::{DisplayHandle, WindowHandles};
#[cfg(has_window)]
pub(crate) use crate::window_os::{apply_ime_cursor_area, apply_window_changes, window_attributes};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_size_larger_than_the_monitor_is_clamped_to_it() {
        // 2560x1440 physical at scale 1.25 is a 2048x1152 logical monitor.
        let monitor = MonitorExtent {
            physical_size: (2560, 1440),
            scale_factor: 1.25,
        };
        assert_eq!(monitor.logical_size(), (2048.0, 1152.0));
        let planned = plan_window_size((3840.0, 2160.0), Some(monitor.logical_size()), false);
        assert_eq!(planned, Ok((2048.0, 1152.0)));
        // One axis over: only that axis is clamped.
        let planned = plan_window_size((1600.0, 1400.0), Some(monitor.logical_size()), false);
        assert_eq!(planned, Ok((1600.0, 1152.0)));
        // A size that fits is passed as is.
        assert_eq!(
            plan_window_size((1280.0, 720.0), Some(monitor.logical_size()), false),
            Ok((1280.0, 720.0))
        );
    }

    #[test]
    fn without_a_monitor_report_the_size_is_passed_unclamped() {
        assert_eq!(
            plan_window_size((7680.0, 4320.0), None, false),
            Ok((7680.0, 4320.0))
        );
        // A monitor report of zero (some compositors before the first output event) is ignored.
        assert_eq!(
            plan_window_size((1920.0, 1080.0), Some((0.0, 0.0)), false),
            Ok((1920.0, 1080.0))
        );
    }

    #[test]
    fn bad_sizes_are_refused_with_their_codes() {
        for bad in [
            (0.0, 720.0),
            (1280.0, -1.0),
            (f64::NAN, 720.0),
            (f64::INFINITY, 720.0),
        ] {
            assert_eq!(
                plan_window_size(bad, None, false),
                Err(WINDOW_SIZE_INVALID),
                "{bad:?}"
            );
        }
        assert_eq!(
            plan_window_size((319.0, 720.0), None, false),
            Err(WINDOW_SIZE_TOO_SMALL)
        );
        assert_eq!(
            plan_window_size((1280.0, 179.0), None, false),
            Err(WINDOW_SIZE_TOO_SMALL)
        );
        assert_eq!(
            plan_window_size(MIN_WINDOW_LOGICAL_SIZE, None, false),
            Ok(MIN_WINDOW_LOGICAL_SIZE)
        );
    }

    #[test]
    fn a_fixed_platform_refuses_sizes_and_windowed_mode() {
        assert_eq!(
            plan_window_size((1280.0, 720.0), None, true),
            Err(WINDOW_SIZE_FIXED)
        );
        assert_eq!(
            plan_window_mode(WindowMode::Windowed, true),
            Err(WINDOW_MODE_FIXED)
        );
        assert_eq!(
            plan_window_mode(WindowMode::BorderlessFullscreen, true),
            Ok(WindowMode::BorderlessFullscreen)
        );
        assert_eq!(
            plan_window_mode(WindowMode::Windowed, false),
            Ok(WindowMode::Windowed)
        );
    }

    #[test]
    fn refusal_codes_are_protocol_safe() {
        for code in [
            WINDOW_SIZE_INVALID,
            WINDOW_SIZE_TOO_SMALL,
            WINDOW_SIZE_FIXED,
            WINDOW_MODE_FIXED,
        ] {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
        }
    }

    #[test]
    fn a_monitor_with_a_bad_scale_reads_at_scale_one() {
        let monitor = MonitorExtent {
            physical_size: (1920, 1080),
            scale_factor: 0.0,
        };
        assert_eq!(monitor.logical_size(), (1920.0, 1080.0));
    }

    /// The Steam Deck pass (ADR-7880): the 800x1280 panel reads unrotated while the 1278x720 window
    /// shows landscape, and a 1024x600 request was clamped to 800x600 before this.
    #[test]
    fn a_rotated_panel_report_is_turned_to_the_window() {
        let deck = MonitorExtent {
            physical_size: (800, 1280),
            scale_factor: 1.0,
        };
        let oriented = orient_monitor(deck.logical_size(), (1278.0, 720.0));
        assert_eq!(oriented, Some((1280.0, 800.0)));
        assert_eq!(
            plan_window_size((1024.0, 600.0), oriented, false),
            Ok((1024.0, 600.0))
        );
        assert_eq!(
            plan_window_size((4000.0, 3000.0), oriented, false),
            Ok((1280.0, 800.0))
        );
        // A landscape monitor and a window inside it: the report as is.
        assert_eq!(
            orient_monitor((1920.0, 1080.0), (1280.0, 720.0)),
            Some((1920.0, 1080.0))
        );
        // No window size yet: the orientation is unknown, so no clamp (a Deck's saved 1280x800
        // must not be cut to the unrotated report's width 800 on the first apply).
        assert_eq!(orient_monitor((800.0, 1280.0), (0.0, 0.0)), None);
        assert_eq!(orient_monitor((1920.0, 1080.0), (0.0, 720.0)), None);
        // A window larger than the monitor either way: the report is not trusted, no clamp.
        assert_eq!(orient_monitor((1280.0, 720.0), (2560.0, 1440.0)), None);
    }
}
