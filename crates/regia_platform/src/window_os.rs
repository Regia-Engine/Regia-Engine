// SPDX-License-Identifier: MPL-2.0
//! The OS half of the window layer: winit handles and the system that applies [`Window`] changes
//! (feature `window`, spec 76 § 2.1).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use regia_app::prelude::*;

use crate::window::{
    CursorCalls, CursorGrab, CursorRelease, ImeCursorArea, MonitorExtent, PrimaryWindow, Window, WindowMode,
    WindowRefusal, WindowStatus, default_app_id, plan_window_mode, plan_window_size,
};

/// OS window handles by entity, for the renderer (specs/11 § 1 `RawHandleWrapper`).
#[derive(Resource, Default, Clone)]
pub struct WindowHandles {
    handles: HashMap<Entity, Arc<winit::window::Window>>,
}

impl WindowHandles {
    /// The OS window of `entity`.
    pub fn get(&self, entity: Entity) -> Option<&Arc<winit::window::Window>> {
        self.handles.get(&entity)
    }
    /// All windows.
    pub fn iter(&self) -> impl Iterator<Item = (Entity, &Arc<winit::window::Window>)> {
        self.handles.iter().map(|(e, w)| (*e, w))
    }
    /// Asks the OS to draw the user's attention to the window of `entity` (taskbar flash, urgency
    /// hint) until it is focused. Returns false if `entity` has no OS window. No effect on Android.
    pub fn request_user_attention(&self, entity: Entity) -> bool {
        match self.handles.get(&entity) {
            Some(window) => {
                window.request_user_attention(Some(winit::window::UserAttentionType::Informational));
                true
            }
            None => false,
        }
    }
    pub(crate) fn insert(&mut self, entity: Entity, window: Arc<winit::window::Window>) {
        self.handles.insert(entity, window);
    }
    pub(crate) fn remove(&mut self, entity: Entity) {
        self.handles.remove(&entity);
    }
}

/// The OS display connection, for creating the GPU instance (wgpu needs it for GLES/Wayland).
#[derive(Resource, Clone)]
pub struct DisplayHandle(pub winit::event_loop::OwnedDisplayHandle);

pub(crate) fn window_attributes(window: &Window) -> winit::window::WindowAttributes {
    let attrs = winit::window::Window::default_attributes()
        .with_title(window.title.clone())
        .with_inner_size(winit::dpi::LogicalSize::new(
            window.logical_size.0,
            window.logical_size.1,
        ))
        .with_resizable(window.resizable)
        .with_decorations(window.decorations);
    let app_id = window.app_id.clone().unwrap_or_else(default_app_id);
    let attrs = crate::platform::with_app_id(attrs, &app_id);
    match window.mode {
        WindowMode::Windowed => attrs,
        WindowMode::BorderlessFullscreen => {
            attrs.with_fullscreen(Some(winit::window::Fullscreen::Borderless(None)))
        }
    }
}

/// `PostUpdate`: hands a changed [`ImeCursorArea`] to its OS window (the primary window when it
/// names none), so the input method's candidate window opens next to the focused field's caret
/// (ADR-7208). The same rect is not sent twice; a window without an OS handle yet is retried.
pub(crate) fn apply_ime_cursor_area(
    area: Res<ImeCursorArea>,
    handles: Option<Res<WindowHandles>>,
    primary: Query<Entity, With<PrimaryWindow>>,
    mut applied: Local<Option<(Entity, [f32; 4])>>,
) {
    let Some(rect) = area.rect else {
        return;
    };
    let Some(entity) = area.window.or_else(|| primary.iter().next()) else {
        return;
    };
    if *applied == Some((entity, rect)) {
        return;
    }
    let Some(os) = handles.as_ref().and_then(|h| h.get(entity)) else {
        return;
    };
    crate::platform::set_ime_cursor_area(os, rect);
    *applied = Some((entity, rect));
}

/// The state a freshly created OS window is in: [`window_attributes`] carries title, size,
/// resizable, decorations, app id and mode; the cursor is visible and free and the IME is off.
fn creation_baseline(window: &Window) -> Window {
    Window {
        cursor_visible: true,
        cursor_grab: CursorGrab::None,
        ime_enabled: false,
        ..window.clone()
    }
}

/// Which window backend the grab is applied on.
///
/// winit 0.30.13 does not implement every [`winit::window::CursorGrabMode`]: `Locked` returns
/// `NotSupported` on X11 (and Xwayland), and `Confined` returns `NotSupported` on macOS. Windows
/// keeps the spec's `Locked` → `Confined` fallback. Wayland implements both, so it stays `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrabBackend {
    /// Win32. `Locked` is requested as `Confined`.
    Windows,
    /// X11 or Xwayland. `Locked` is requested as `Confined`.
    X11,
    /// AppKit. `Confined` is requested as `Locked`.
    MacOs,
    /// Wayland, Android, and every backend that is asked for the mode unchanged.
    Other,
}

/// Once-per-process notes for a grab mode this backend cannot do.
///
/// `pub(crate)` because it is a parameter of `apply_window_changes`, and a `pub(crate)` system
/// cannot name a module-private type.
#[derive(Default)]
pub(crate) struct GrabFallbackLog {
    /// `Locked` was rewritten to `Confined` (Windows, X11).
    locked_as_confined: bool,
    /// `Confined` was rewritten to `Locked` (macOS).
    confined_as_locked: bool,
}

/// The winit cursor-grab mode for `grab` on `backend`.
///
/// A fallback is logged once, through `logged`. winit 0.30.13 returns `NotSupported` for `Locked`
/// on X11 and for `Confined` on macOS, so those requests use the mode that backend implements.
fn cursor_grab_mode(
    grab: CursorGrab,
    backend: GrabBackend,
    logged: &mut GrabFallbackLog,
) -> winit::window::CursorGrabMode {
    match (grab, backend) {
        (CursorGrab::None, _) => winit::window::CursorGrabMode::None,
        (CursorGrab::Confined, GrabBackend::MacOs) => {
            if !logged.confined_as_locked {
                logged.confined_as_locked = true;
                tracing::debug!(
                    target: "regia_platform",
                    "CursorGrab::Confined is not supported on macOS; falling back to Locked"
                );
            }
            winit::window::CursorGrabMode::Locked
        }
        (CursorGrab::Confined, _) => winit::window::CursorGrabMode::Confined,
        (CursorGrab::Locked, GrabBackend::Windows) => locked_as_confined("Windows", logged),
        (CursorGrab::Locked, GrabBackend::X11) => locked_as_confined("X11", logged),
        (CursorGrab::Locked, _) => winit::window::CursorGrabMode::Locked,
    }
}

/// `Locked` requested as `Confined`, logged once per process.
fn locked_as_confined(host: &str, logged: &mut GrabFallbackLog) -> winit::window::CursorGrabMode {
    if !logged.locked_as_confined {
        logged.locked_as_confined = true;
        tracing::debug!(
            target: "regia_platform",
            "CursorGrab::Locked is not supported on {host}; falling back to Confined"
        );
    }
    winit::window::CursorGrabMode::Confined
}

/// True when the display connection is X11 or Xwayland (winit's Xlib or Xcb handle).
fn x11_display(window: &winit::window::Window) -> bool {
    use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
    matches!(
        window.display_handle().map(|handle| handle.as_raw()),
        Ok(RawDisplayHandle::Xlib(_) | RawDisplayHandle::Xcb(_))
    )
}

/// Backend for `window`, from the compile target and, on Linux, the live display handle.
fn grab_backend(window: &winit::window::Window) -> GrabBackend {
    classify_grab_backend(
        cfg!(target_os = "windows"),
        cfg!(target_os = "macos"),
        x11_display(window),
    )
}

/// Pure half of `grab_backend`. `windows` wins over `macos` and `x11`.
fn classify_grab_backend(windows: bool, macos: bool, x11: bool) -> GrabBackend {
    if windows {
        GrabBackend::Windows
    } else if macos {
        GrabBackend::MacOs
    } else if x11 {
        GrabBackend::X11
    } else {
        GrabBackend::Other
    }
}

/// Whether this frame must talk to the OS window.
///
/// A refused grab stays different from the stored snapshot, and the component is not marked
/// changed again, so `grab_pending` is what makes the next frame retry.
fn should_apply_window(first_sight: bool, component_changed: bool, grab_pending: bool) -> bool {
    first_sight || component_changed || grab_pending
}

/// A snapshot stored after this frame's OS calls: the grab stays `previous_grab` when the
/// request failed in a way worth retrying (a transient OS error), so the next change or sight
/// tries it again. A refusal that is final (the platform has no grab at all) is stored as the
/// requested grab instead, so a game that writes `Window` every frame does not retry and re-log
/// it every frame (corr-low, ADR-5789).
fn applied_window(window: &Window, previous_grab: CursorGrab, grab_final: bool) -> Window {
    let mut stored = window.clone();
    if !grab_final {
        stored.cursor_grab = previous_grab;
    }
    stored
}

/// `true` when a grab refusal will never turn into a success on this platform: winit's
/// `NotSupported` means the backend has no cursor grab at all (Android). Those are final; an
/// `Os` error may be worth one more try on a later change.
fn grab_refusal_is_final(err: &winit::error::ExternalError) -> bool {
    matches!(err, winit::error::ExternalError::NotSupported(_))
}

/// `PostUpdate`: applies changed [`Window`] components to their OS windows.
///
/// Pinned to the main thread with [`NonSendMarker`]: winit's window setters post to the
/// event-loop thread and block when called off it (on Windows `set_cursor_grab` /
/// `set_cursor_visible` wait on `rx.recv()`), so running this on a task-pool thread deadlocks the
/// frame loop while the main thread is inside `App::update` waiting for this system.
pub(crate) fn apply_window_changes(
    _main: NonSendMarker,
    mut windows: Query<(Entity, Ref<Window>, Option<&mut WindowStatus>)>,
    handles: Res<WindowHandles>,
    // What each OS window was last given, and the windows whose size went out before the OS
    // reported their own size (ADR-7979).
    mut memory: Local<(HashMap<Entity, Window>, HashSet<Entity>)>,
    mut grab_fallback_logged: Local<GrabFallbackLog>,
    release: Option<Res<CursorRelease>>,
    mut calls: Option<ResMut<CursorCalls>>,
) {
    // A system overlay holds the cursor (ADR-7349): the OS gets a free, visible cursor and the
    // components' own values wait until it lets go. A change of the hold re-checks every window.
    let hold = release.as_deref().copied().unwrap_or_default();
    let (previous, unoriented) = &mut *memory;
    let hold_changed = release.is_some_and(|r| r.is_changed());
    for (entity, component, mut status) in &mut windows {
        let Some(os) = handles.get(entity) else { continue };
        // The first time an OS window is seen, diff against what it was created with: the cursor,
        // grab and IME state never go into the creation attributes, so a value set before this
        // system first ran must still reach the OS.
        let first_sight = !previous.contains_key(&entity);
        let old = previous
            .entry(entity)
            .or_insert_with(|| creation_baseline(&component));
        let Some(plan) = plan_cursor_changes(
            &component,
            old,
            hold,
            first_sight || component.is_changed() || hold_changed,
        ) else {
            continue;
        };
        let window = os_view(&component, hold);
        if window.title != old.title {
            os.set_title(&window.title);
        }
        if window.resizable != old.resizable {
            os.set_resizable(window.resizable);
        }
        if window.decorations != old.decorations {
            os.set_decorations(window.decorations);
        }
        if let Some(visible) = plan.set_visible {
            os.set_cursor_visible(visible);
            if let Some(calls) = calls.as_deref_mut() {
                calls.visible_calls += 1;
                calls.last_visible = visible;
            }
        }
        if window.ime_enabled != old.ime_enabled {
            os.set_ime_allowed(window.ime_enabled);
        }
        let previous_grab = old.cursor_grab;
        let grab_final = if plan.set_grab {
            let mode = cursor_grab_mode(window.cursor_grab, grab_backend(os), &mut grab_fallback_logged);
            if let Some(calls) = calls.as_deref_mut() {
                calls.grab_calls += 1;
                calls.last_grab = window.cursor_grab;
            }
            match os.set_cursor_grab(mode) {
                Ok(()) => true,
                Err(err) => {
                    // Retries (component unchanged) stay quiet: the first refusal is the one to log.
                    if first_sight || component.is_changed() {
                        tracing::warn!(
                            target: "regia_platform",
                            "cursor grab {:?} not supported: {err}",
                            window.cursor_grab
                        );
                    }
                    // `NotSupported` never changes: record the request as applied so it is not
                    // retried every frame.
                    grab_refusal_is_final(&err)
                }
            }
        } else {
            true
        };
        let mode_changed = window.mode != old.mode;
        let size_changed = window.logical_size != old.logical_size;
        // A size applied before the OS reported the window's size went out unclamped (the
        // monitor's orientation was unknown); apply it again once the size is known (ADR-7979).
        let reorient =
            unoriented.contains(&entity) && window.physical_size.0 > 0 && window.physical_size.1 > 0;
        if mode_changed || size_changed || first_sight || reorient {
            // The monitor the window is on now: what a size request is clamped to (ADR-7880).
            let monitor = os.current_monitor().map(|m| MonitorExtent {
                physical_size: (m.size().width, m.size().height),
                scale_factor: m.scale_factor(),
            });
            // The runner spawns every OS window with a status; one removed by the game is not
            // recorded, but its requests still go through the same planning.
            let mut next = status.as_deref().cloned().unwrap_or_default();
            next.monitor = monitor;
            if mode_changed || size_changed {
                next.refused = None;
            }
            if mode_changed {
                apply_mode(os, &window, &mut next);
            }
            if size_changed || reorient {
                if apply_size(os, &window, &mut next) {
                    unoriented.remove(&entity);
                } else {
                    unoriented.insert(entity);
                }
            }
            if let Some(status) = status.as_deref_mut()
                && *status != next
            {
                *status = next;
            }
        }
        *old = applied_window(&window, previous_grab, grab_final);
    }
    prune_previous(previous, |entity| {
        handles.get(entity).is_some() && windows.contains(entity)
    });
    unoriented.retain(|entity| previous.contains_key(entity));
}

/// Passes `window.mode` to the OS, or records why the platform refuses it (ADR-7880).
fn apply_mode(os: &winit::window::Window, window: &Window, status: &mut WindowStatus) {
    match plan_window_mode(window.mode, crate::platform::FIXED_WINDOW) {
        Ok(mode) => {
            os.set_fullscreen(match mode {
                WindowMode::Windowed => None,
                WindowMode::BorderlessFullscreen => Some(winit::window::Fullscreen::Borderless(None)),
            });
            status.applied_mode = Some(mode);
            status.mode_requests += 1;
        }
        Err(code) => refuse(window, status, code),
    }
}

/// Passes `window.logical_size`, clamped to the monitor, to the OS, or records why it is refused
/// (ADR-7880). Returns `false` when a monitor was reported but the window's own size was not yet,
/// so the clamp could not be oriented and the request went out unclamped (ADR-7979).
fn apply_size(os: &winit::window::Window, window: &Window, status: &mut WindowStatus) -> bool {
    // The window's own size orients a panel the OS reports unrotated (the Steam Deck).
    let current = if window.scale_factor.is_finite() && window.scale_factor > 0.0 {
        (
            f64::from(window.physical_size.0) / window.scale_factor,
            f64::from(window.physical_size.1) / window.scale_factor,
        )
    } else {
        (0.0, 0.0)
    };
    let oriented = status.monitor.is_none() || (current.0 > 0.0 && current.1 > 0.0);
    let monitor = status
        .monitor
        .and_then(|m| crate::window::orient_monitor(m.logical_size(), current));
    match plan_window_size(window.logical_size, monitor, crate::platform::FIXED_WINDOW) {
        Ok(size) => {
            if size != window.logical_size {
                tracing::info!(
                    target: "regia_platform",
                    "window size {:?} clamped to {size:?} (monitor {monitor:?})",
                    window.logical_size
                );
            }
            let _ = os.request_inner_size(winit::dpi::LogicalSize::new(size.0, size.1));
            status.applied_logical_size = Some(size);
            status.size_requests += 1;
        }
        Err(code) => refuse(window, status, code),
    }
    oriented
}

/// Records a refused size or mode request; the OS window keeps what it had.
fn refuse(window: &Window, status: &mut WindowStatus, code: &'static str) {
    tracing::warn!(
        target: "regia_platform",
        "window request refused ({code}): size {:?}, mode {:?}",
        window.logical_size,
        window.mode
    );
    status.refused = Some(WindowRefusal {
        code,
        requested_size: window.logical_size,
        requested_mode: window.mode,
    });
}

/// What the OS is asked for: the component, with a free and visible cursor while a system overlay
/// holds it ([`CursorRelease`]). The stored snapshot is this view, so letting go differs from it
/// and re-applies the component's own grab and visibility.
fn os_view(component: &Window, hold: CursorRelease) -> Window {
    let mut window = component.clone();
    window.cursor_grab = hold.grab(component.cursor_grab);
    window.cursor_visible = hold.visible(component.cursor_visible);
    window
}

/// The cursor calls one frame of [`apply_window_changes`] plans for one window, on its own so the
/// overlay-hold test runs the same decision code the system runs (w9-net-vehicle-steam-z 3).
///
/// `stored` is the snapshot the last applied frame left (on first sight the creation baseline);
/// `changed` is the component's change bit, a hold change, or a first sight. `None` skips the
/// window this frame. A `set_grab` plan re-asks the OS for [`os_view`]'s grab, which differs from
/// `stored` only when the hold or the component moved it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CursorPlan {
    /// `set_cursor_visible` runs, with this value.
    pub set_visible: Option<bool>,
    /// `set_cursor_grab` runs, for `os_view(component, hold).cursor_grab`.
    pub set_grab: bool,
}

/// Decides one window's cursor calls for this frame: the pure half of `apply_window_changes`.
pub(crate) fn plan_cursor_changes(
    component: &Window,
    stored: &Window,
    hold: CursorRelease,
    changed: bool,
) -> Option<CursorPlan> {
    let pending = hold.grab(component.cursor_grab) != stored.cursor_grab;
    if !should_apply_window(false, changed, pending) {
        return None;
    }
    let view = os_view(component, hold);
    Some(CursorPlan {
        set_visible: (view.cursor_visible != stored.cursor_visible).then_some(view.cursor_visible),
        set_grab: pending,
    })
}

/// Drops the last-applied state of windows that are gone (entity despawned or OS window closed),
/// so `apply_window_changes`' map does not grow with every window ever opened. A window that comes
/// back is diffed against its creation baseline again, as on first sight.
fn prune_previous<T>(previous: &mut HashMap<Entity, T>, live: impl Fn(Entity) -> bool) {
    previous.retain(|entity, _| live(*entity));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `apply_window_changes` calls winit window setters, which on Windows post to the event-loop
    /// thread and block when called off it. The `NonSendMarker` param must therefore pin the system
    /// to the main thread — otherwise a task-pool thread would deadlock against `App::update`.
    #[test]
    fn apply_window_changes_is_pinned_to_the_main_thread() {
        let mut world = World::new();
        let mut system = IntoSystem::into_system(apply_window_changes);
        system.initialize(&mut world);
        assert!(
            !system.is_send(),
            "apply_window_changes must carry NonSendMarker so it runs on the main thread"
        );
    }

    #[test]
    fn creation_baseline_differs_in_cursor_grab_and_ime_only() {
        let wanted = Window {
            cursor_visible: false,
            cursor_grab: CursorGrab::Locked,
            ime_enabled: true,
            ..Window::default()
        };
        let base = creation_baseline(&wanted);
        assert!(base.cursor_visible);
        assert_eq!(base.cursor_grab, CursorGrab::None);
        assert!(!base.ime_enabled);
        assert_eq!(base.title, wanted.title);
        assert_eq!(base.logical_size, wanted.logical_size);
        assert_eq!(base.mode, wanted.mode);
    }

    #[test]
    fn cursor_grab_mode_maps_locked_per_platform() {
        // The host OS does not matter: each backend is an argument, so one Linux run covers them.
        let mut logged = GrabFallbackLog::default();
        assert_eq!(
            cursor_grab_mode(CursorGrab::None, GrabBackend::Other, &mut logged),
            winit::window::CursorGrabMode::None
        );
        for backend in [GrabBackend::Windows, GrabBackend::X11, GrabBackend::Other] {
            let mut logged = GrabFallbackLog::default();
            assert_eq!(
                cursor_grab_mode(CursorGrab::Confined, backend, &mut logged),
                winit::window::CursorGrabMode::Confined,
                "{backend:?} implements confine"
            );
            assert!(!logged.confined_as_locked);
        }

        let mut wayland = GrabFallbackLog::default();
        assert_eq!(
            cursor_grab_mode(CursorGrab::Locked, GrabBackend::Other, &mut wayland),
            winit::window::CursorGrabMode::Locked,
            "Wayland implements a locked grab"
        );
        assert!(!wayland.locked_as_confined);

        for backend in [GrabBackend::Windows, GrabBackend::X11] {
            let mut logged = GrabFallbackLog::default();
            assert_eq!(
                cursor_grab_mode(CursorGrab::Locked, backend, &mut logged),
                winit::window::CursorGrabMode::Confined,
                "{backend:?} must fall back: winit 0.30 rejects Locked on Windows and on X11"
            );
            assert!(
                logged.locked_as_confined,
                "{backend:?} fallback must be logged once"
            );
            assert_eq!(
                cursor_grab_mode(CursorGrab::Locked, backend, &mut logged),
                winit::window::CursorGrabMode::Confined
            );
        }

        let mut macos = GrabFallbackLog::default();
        assert_eq!(
            cursor_grab_mode(CursorGrab::Confined, GrabBackend::MacOs, &mut macos),
            winit::window::CursorGrabMode::Locked,
            "macOS has no confine; winit implements Locked"
        );
        assert!(macos.confined_as_locked, "the macOS fallback must be logged once");
        assert_eq!(
            cursor_grab_mode(CursorGrab::Locked, GrabBackend::MacOs, &mut macos),
            winit::window::CursorGrabMode::Locked
        );
    }

    #[test]
    fn classify_grab_backend_prefers_the_compile_target() {
        assert_eq!(classify_grab_backend(true, false, false), GrabBackend::Windows);
        assert_eq!(classify_grab_backend(true, false, true), GrabBackend::Windows);
        assert_eq!(classify_grab_backend(false, true, true), GrabBackend::MacOs);
        assert_eq!(classify_grab_backend(false, false, true), GrabBackend::X11);
        assert_eq!(classify_grab_backend(false, false, false), GrabBackend::Other);
    }

    /// A refused grab must not be stored as applied. The component is not written again, so the
    /// next frame retries only because the stored grab is still the old one.
    #[test]
    fn failed_cursor_grab_does_not_advance_and_is_retried() {
        let requested = Window {
            title: "Game".into(),
            cursor_grab: CursorGrab::Locked,
            ..Window::default()
        };
        let previous = creation_baseline(&requested);
        assert_eq!(previous.cursor_grab, CursorGrab::None);
        assert!(should_apply_window(true, true, true));

        let stored = applied_window(&requested, previous.cursor_grab, false);
        assert_eq!(
            stored.cursor_grab,
            CursorGrab::None,
            "a failed grab must stay pending"
        );
        assert_eq!(stored.title, "Game", "other fields are still recorded");

        let pending = requested.cursor_grab != stored.cursor_grab;
        assert!(
            should_apply_window(false, false, pending),
            "the next frame must retry while the component is unchanged"
        );

        let stored = applied_window(&requested, stored.cursor_grab, true);
        assert_eq!(stored.cursor_grab, CursorGrab::Locked);
        assert!(!should_apply_window(
            false,
            false,
            requested.cursor_grab != stored.cursor_grab
        ));
    }

    /// corr-low: winit's `NotSupported` means the platform has no cursor grab at all (Android).
    /// That refusal is final: recording it as applied stops a game that writes `Window` every
    /// frame from re-calling the OS and re-logging the warning every frame. A transient `Os`
    /// failure keeps the previous grab so a later change retries it.
    #[test]
    fn a_final_grab_refusal_is_recorded_and_not_retried() {
        // `ExternalError::NotSupported`'s payload cannot be constructed outside winit, so the
        // refusal classification itself is not unit-testable; the behavior it gates is: a final
        // refusal records the request (no pending state), a transient one stays pending.
        let requested = Window {
            cursor_grab: CursorGrab::Locked,
            ..Window::default()
        };
        let stored = applied_window(&requested, CursorGrab::None, true);
        assert_eq!(
            stored.cursor_grab,
            CursorGrab::Locked,
            "a final refusal (winit NotSupported, e.g. Android) records the request: no retry"
        );
        assert!(
            !should_apply_window(false, false, requested.cursor_grab != stored.cursor_grab),
            "the next frame must not retry a final refusal"
        );
        let retried = applied_window(&requested, CursorGrab::None, false);
        assert_eq!(
            retried.cursor_grab,
            CursorGrab::None,
            "a transient failure stays pending for the next change"
        );
        assert!(
            should_apply_window(false, false, requested.cursor_grab != retried.cursor_grab),
            "a transient failure is retried while the component is unchanged"
        );
    }

    /// ADR-7349: while a system overlay holds the cursor, a game that writes `Locked` every frame
    /// (or re-grabs on a focus change) makes no grab call; the hold releases the grab once and
    /// letting go applies the game's current wish once. This drives the frame decision the system
    /// itself runs (`plan_cursor_changes` + `os_view` + `applied_window`; the system needs a winit
    /// window only to *make* the calls), so a break in that code fails here
    /// (w9-net-vehicle-steam-z 3).
    #[test]
    fn a_cursor_hold_releases_once_blocks_regrabs_and_restores_once() {
        fn frame(
            component: &Window,
            stored: &mut Window,
            hold: CursorRelease,
            changed: bool,
            hold_changed: bool,
            grabs: &mut Vec<CursorGrab>,
            shows: &mut Vec<bool>,
        ) {
            let Some(plan) = plan_cursor_changes(component, stored, hold, changed || hold_changed) else {
                return;
            };
            if let Some(visible) = plan.set_visible {
                shows.push(visible);
            }
            if plan.set_grab {
                grabs.push(os_view(component, hold).cursor_grab);
            }
            *stored = applied_window(&os_view(component, hold), stored.cursor_grab, true);
        }
        let game = Window {
            cursor_grab: CursorGrab::Locked,
            cursor_visible: false,
            ..Window::default()
        };
        // The game's grab is applied before the overlay.
        let mut stored = game.clone();
        let (mut grabs, mut shows) = (Vec::new(), Vec::new());
        let held = CursorRelease(true);
        // Overlay opens: one release.
        frame(&game, &mut stored, held, false, true, &mut grabs, &mut shows);
        assert_eq!(grabs, [CursorGrab::None]);
        assert_eq!(shows, [true]);
        // The game re-writes its grab every frame and on focus churn: nothing reaches the OS.
        for _ in 0..10 {
            frame(&game, &mut stored, held, true, false, &mut grabs, &mut shows);
        }
        assert_eq!(grabs, [CursorGrab::None], "no grab call while the hold is on");
        assert_eq!(shows, [true]);
        // The game changes its mind while held (menu: free cursor); still nothing reaches the OS.
        let menu = Window {
            cursor_grab: CursorGrab::Confined,
            ..game.clone()
        };
        frame(&menu, &mut stored, held, true, false, &mut grabs, &mut shows);
        assert_eq!(grabs, [CursorGrab::None]);
        // Overlay closes: the game's current wish, once.
        frame(
            &menu,
            &mut stored,
            CursorRelease(false),
            false,
            true,
            &mut grabs,
            &mut shows,
        );
        assert_eq!(grabs, [CursorGrab::None, CursorGrab::Confined]);
        assert_eq!(shows, [true, false]);
        frame(
            &menu,
            &mut stored,
            CursorRelease(false),
            true,
            false,
            &mut grabs,
            &mut shows,
        );
        assert_eq!(grabs.len(), 2, "no repeat after the restore");
        // A game with a free, visible cursor never sees a grab call from the hold at all.
        let free = Window::default();
        let mut stored = free.clone();
        let (mut grabs, mut shows) = (Vec::new(), Vec::new());
        frame(&free, &mut stored, held, false, true, &mut grabs, &mut shows);
        frame(
            &free,
            &mut stored,
            CursorRelease(false),
            false,
            true,
            &mut grabs,
            &mut shows,
        );
        assert!(grabs.is_empty() && shows.is_empty(), "{grabs:?} {shows:?}");
        // w9-net-vehicle-steam-z 3: a window whose cursor is hidden but not grabbed (a gamepad
        // game): the overlay still gets its visible cursor — one show call, never a grab call —
        // and letting go hides the cursor again.
        let hidden = Window {
            cursor_visible: false,
            ..Window::default()
        };
        assert_eq!(hidden.cursor_grab, CursorGrab::None);
        let mut stored = hidden.clone();
        let (mut grabs, mut shows) = (Vec::new(), Vec::new());
        frame(&hidden, &mut stored, held, false, true, &mut grabs, &mut shows);
        assert_eq!(shows, [true], "the overlay shows the hidden cursor");
        assert!(grabs.is_empty(), "an ungrabbed window makes no grab call");
        frame(
            &hidden,
            &mut stored,
            CursorRelease(false),
            false,
            true,
            &mut grabs,
            &mut shows,
        );
        assert_eq!(shows, [true, false], "letting go hides it again");
        assert!(grabs.is_empty(), "never a grab call: {grabs:?}");
    }

    #[test]
    fn closed_windows_leave_the_applied_state_map() {
        let mut world = World::new();
        let kept = world.spawn_empty().id();
        let closed = world.spawn_empty().id();
        let mut previous: HashMap<Entity, Window> = HashMap::new();
        previous.insert(kept, Window::default());
        previous.insert(closed, Window::default());
        prune_previous(&mut previous, |entity| entity == kept);
        assert_eq!(previous.len(), 1);
        assert!(previous.contains_key(&kept));
    }
}
