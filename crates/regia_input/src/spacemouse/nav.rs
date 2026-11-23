// SPDX-License-Identifier: MPL-2.0
//! The shared SpaceMouse navigation function (`docs/specs/29-spacemouse.md` §§ 2.2–2.4):
//! [`step`] turns the merged device state into deltas for one frame. Every Regia Engine UI app (the
//! editor, Tincal, Red-Rouge) calls it and applies the [`NavDelta`] to its own camera, object or
//! 2D view, so the device feels the same everywhere.
//!
//! The function is pure and deterministic: no globals, no clock, no allocation. The shaping
//! order is invert → dead zone → quadratic curve → dominant axis → rates × sensitivity × `dt`.
//! The one piece of memory a mode needs (Step's auto-repeat) is passed in
//! ([`NavContext::step_held`]) and handed back ([`NavDelta::step_held`]).
//!
//! # Sign conventions of [`NavDelta`]
//!
//! The deltas describe what happens to the **view** (the camera follows the cap):
//!
//! * `pan` — view space (x right, y up), world units (Orbit, Turntable) or the 2D view's units
//!   (View2D), added to the orbit target / view centre. Pushing the cap right moves the view
//!   right; lifting it moves the view up.
//! * `zoom` — a multiplier on the orbit distance, 2D visible extent or timeline span: `< 1`
//!   closer (cap pushed away from you), `> 1` farther (cap pulled toward you).
//! * `orbit.x` (yaw) — radians, positive turns the view counter-clockwise seen from above
//!   (right-handed about world +Y); an orbit camera moves its eye around the target by the same
//!   angle. Twisting the cap counter-clockwise is positive. Turntable: the same angle, applied
//!   to the object.
//! * `orbit.y` (pitch) — radians, positive tips the view down: an orbit camera's eye rises above
//!   the target; a fly camera looks further down. Tilting the cap away from you is positive.
//! * `roll` — radians, positive rolls the view counter-clockwise as the viewer sees it. Zero while
//!   horizon lock is on.
//! * `translate` — Fly, view space (x right, y up, z toward the viewer), world units. Pushing the
//!   cap away from you (−Z) moves the camera forward.
//! * `scrub` — Timeline, in the timeline's own units (`v.ry · visible_span · 1.0 · dt`; a
//!   fraction of the span when `visible_span` is 1): twisting counter-clockwise moves the playhead
//!   forward.
//! * `step` — Step, −1 / 0 / +1 this frame: +1 (next) when the cap is tilted right, −1 (previous)
//!   when tilted left.

use regia_core::math::{Vec2, Vec3};

use super::{NavigationDefaults as D, SpaceMouseNav, SpaceMouseState};
use crate::config::SpaceMouseConfig;

/// Which navigation model the pane uses (§ 2.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NavMode {
    /// Orbit around a target at a distance: pan, zoom, orbit (object/asset viewports; default).
    #[default]
    Orbit,
    /// Free camera: translate and turn (walk-through cameras).
    Fly,
    /// Review lightbox: the Orbit delta, with `orbit` applied to the object's rotation.
    Turntable,
    /// 2D canvas (crop windows, image and video panes): pan and zoom of the visible extent.
    View2D,
    /// A 1D time axis (players, sequencers, curve editors): twist scrubs, Z zooms the span.
    Timeline,
    /// A discrete list (scene cards, cuts, items): one step per tilt, repeating while held.
    Step,
}

/// Per-call inputs of [`step`]: the pane's navigation mode and view facts, plus the user's
/// overrides and the runtime toggles.
///
/// Build it with [`NavContext::orbit`] / [`NavContext::fly`] / [`NavContext::turntable`] /
/// [`NavContext::view2d`] / [`NavContext::timeline`] / [`NavContext::list`] and the `with_*`
/// methods; every field not set holds the spec default (horizon lock on, dominant axis off,
/// sensitivity 1.0, nothing inverted).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NavContext {
    /// The navigation mode.
    pub mode: NavMode,
    /// The orbit distance (eye to target), world units. Orbit / Turntable: pan scales with it.
    pub distance: f32,
    /// The scene's scale, world units (e.g. its bounding-sphere radius). Fly mode: full
    /// deflection moves `scene_scale` units per second.
    pub scene_scale: f32,
    /// View2D: the visible extent (e.g. the visible height) in the canvas's units; full
    /// deflection pans one extent per second.
    pub visible_extent: f32,
    /// Timeline: the visible time span; full twist scrubs one span per second.
    pub visible_span: f32,
    /// Horizon lock (default on): no roll, pitch kept within ±[`NavigationDefaults::PITCH_LIMIT_DEG`].
    ///
    /// [`NavigationDefaults::PITCH_LIMIT_DEG`]: super::NavigationDefaults::PITCH_LIMIT_DEG
    pub horizon_lock: bool,
    /// The camera's (Turntable: the object's) current pitch in the [`NavDelta`] `orbit.y` sense:
    /// radians the view looks down below the horizon (an orbit camera's eye elevation above the
    /// target). Only used to keep the pitch inside the limit while horizon lock is on; 0 when
    /// unknown.
    pub pitch: f32,
    /// Step: how long the tilt has been held past the threshold before this frame, seconds (0
    /// when released). Feed back [`NavDelta::step_held`] every frame.
    pub step_held: f32,
    /// Step: the direction (−1 / +1) of the previous frame's tilt, fed back from
    /// [`NavDelta::step_dir`]. A tilt the other way counts as a new crossing even if the cap
    /// never passed through the dead zone between two frames. 0 (the default, also what older
    /// stored contexts read as) means unknown: the tilt is taken as continuing.
    #[serde(default)]
    pub step_dir: i8,
    /// Dominant-axis mode (default off): only the largest shaped axis moves the view.
    pub dominant: bool,
    /// The user's speed multiplier ([`SpaceMouseConfig::sensitivity`], clamped to its range).
    pub sensitivity: f32,
    /// The user's per-axis sign flips, `x, y, z, rx, ry, rz` ([`SpaceMouseConfig::invert`]).
    pub invert: [bool; 6],
}

impl NavContext {
    /// An orbit-mode context at `distance` from the target, with the spec defaults.
    #[must_use]
    pub fn orbit(distance: f32) -> Self {
        Self {
            mode: NavMode::Orbit,
            distance,
            ..Self::DEFAULT
        }
    }

    /// A fly-mode context for a scene of `scene_scale` world units, with the spec defaults.
    #[must_use]
    pub fn fly(scene_scale: f32) -> Self {
        Self {
            mode: NavMode::Fly,
            scene_scale,
            ..Self::DEFAULT
        }
    }

    /// A turntable context (the object spins) at `distance`, with the spec defaults.
    #[must_use]
    pub fn turntable(distance: f32) -> Self {
        Self {
            mode: NavMode::Turntable,
            distance,
            ..Self::DEFAULT
        }
    }

    /// A 2D-canvas context with `visible_extent` canvas units in view.
    #[must_use]
    pub fn view2d(visible_extent: f32) -> Self {
        Self {
            mode: NavMode::View2D,
            visible_extent,
            ..Self::DEFAULT
        }
    }

    /// A timeline context with `visible_span` time units in view.
    #[must_use]
    pub fn timeline(visible_span: f32) -> Self {
        Self {
            mode: NavMode::Timeline,
            visible_span,
            ..Self::DEFAULT
        }
    }

    /// A list-stepping context; `step_held` is the previous frame's [`NavDelta::step_held`].
    /// The direction memory is unknown (see [`NavContext::list_dir`]).
    #[must_use]
    pub fn list(step_held: f32) -> Self {
        Self {
            mode: NavMode::Step,
            step_held,
            ..Self::DEFAULT
        }
    }

    /// A list-stepping context with both memories of the previous frame, [`NavDelta::step_held`]
    /// and [`NavDelta::step_dir`]. Prefer this over [`NavContext::list`]: it steps at once when
    /// the tilt flips from one side to the other.
    #[must_use]
    pub fn list_dir(step_held: f32, step_dir: i8) -> Self {
        Self {
            step_dir,
            ..Self::list(step_held)
        }
    }

    /// The spec defaults: orbit mode at distance 1, every scale 1.
    pub const DEFAULT: Self = Self {
        mode: NavMode::Orbit,
        distance: 1.0,
        scene_scale: 1.0,
        visible_extent: 1.0,
        visible_span: 1.0,
        horizon_lock: D::HORIZON_LOCK,
        pitch: 0.0,
        step_held: 0.0,
        step_dir: 0,
        dominant: D::DOMINANT_AXIS,
        sensitivity: 1.0,
        invert: [false; 6],
    };

    /// Sets the camera's current pitch (see [`NavContext::pitch`]).
    #[must_use]
    pub fn with_pitch(mut self, pitch: f32) -> Self {
        self.pitch = pitch;
        self
    }

    /// Applies the user's overrides (sensitivity, invert) from the app's input settings.
    #[must_use]
    pub fn with_config(mut self, config: &SpaceMouseConfig) -> Self {
        let config = config.sanitized();
        self.sensitivity = config.sensitivity;
        self.invert = config.invert;
        self
    }

    /// Applies the runtime toggles (horizon lock, dominant axis) kept by the plugin.
    #[must_use]
    pub fn with_toggles(mut self, toggles: &SpaceMouseNav) -> Self {
        self.horizon_lock = toggles.horizon_lock;
        self.dominant = toggles.dominant;
        self
    }
}

impl Default for NavContext {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One frame of motion from [`step`]. See the module docs for the sign conventions.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NavDelta {
    /// Orbit / Turntable / View2D: view-centre motion in view space (x right, y up; z is 0).
    pub pan: Vec3,
    /// Yaw (x) and pitch (y), radians.
    pub orbit: Vec2,
    /// Roll, radians (0 while horizon lock is on).
    pub roll: f32,
    /// Multiplier on the orbit distance, 2D extent or timeline span (1 = unchanged).
    pub zoom: f32,
    /// Fly mode: camera motion in view space (x right, y up, z toward the viewer), world units.
    pub translate: Vec3,
    /// Timeline: playhead motion in the timeline's units.
    pub scrub: f32,
    /// Step: −1, 0 or +1 this frame.
    pub step: i8,
    /// Step: the hold time to pass back as [`NavContext::step_held`] next frame (0 otherwise).
    pub step_held: f32,
    /// Step: the tilt direction (−1 / +1) to pass back as [`NavContext::step_dir`] next frame (0
    /// when released or in another mode).
    #[serde(default)]
    pub step_dir: i8,
}

impl NavDelta {
    /// No motion.
    pub const IDENTITY: Self = Self {
        pan: Vec3::ZERO,
        orbit: Vec2::ZERO,
        roll: 0.0,
        zoom: 1.0,
        translate: Vec3::ZERO,
        scrub: 0.0,
        step: 0,
        step_held: 0.0,
        step_dir: 0,
    };

    /// True when the delta moves nothing (a held Step tilt with no step this frame counts as
    /// no motion).
    pub fn is_identity(&self) -> bool {
        Self {
            step_held: 0.0,
            step_dir: 0,
            ..*self
        } == Self::IDENTITY
    }
}

impl Default for NavDelta {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// The dead zone and quadratic response curve of one normalised axis (§ 2.2):
/// `t = max(0, (|n| − 0.04) / (1 − 0.04))`, `v = sign(n) · t²`. The square is an explicit
/// multiply (never `powf`), which is bit-identical on every target (audit finding 7,
/// ADR-4768).
pub fn shape_axis(n: f32) -> f32 {
    if !n.is_finite() {
        return 0.0;
    }
    let n = n.clamp(-1.0, 1.0);
    let t = ((n.abs() - D::DEAD_ZONE) / (1.0 - D::DEAD_ZONE)).max(0.0);
    n.signum() * (t * t)
}

/// The six shaped axes `x, y, z, rx, ry, rz` of `state` for `ctx`: invert, dead zone, curve,
/// then dominant-axis isolation.
pub fn shaped_axes(state: &SpaceMouseState, ctx: &NavContext) -> [f32; 6] {
    let mut v = state.axes();
    for (value, invert) in v.iter_mut().zip(ctx.invert) {
        let n = if invert { -*value } else { *value };
        *value = shape_axis(n);
    }
    if ctx.dominant {
        let mut best = 0;
        for i in 1..6 {
            if v[i].abs() > v[best].abs() {
                best = i;
            }
        }
        for (i, value) in v.iter_mut().enumerate() {
            if i != best {
                *value = 0.0;
            }
        }
    }
    v
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

/// The shared navigation step (§ 2.4): deltas for one frame of `dt` seconds from the latest
/// merged device state.
///
/// Pure and deterministic — "deterministic" means bit-identical across targets: the quadratic
/// curve is an explicit multiply and the zoom uses the shared deterministic exp (multiplies
/// only, no platform libm), so an editor applying the delta writes the same camera and object
/// transforms on every OS (audit finding 7, ADR-4768). `dt` is clamped to
/// `0..=`[`NavigationDefaults::MAX_DT`] so a hitch never throws the view; with that cap the
/// motion is frame-rate independent (the same at 30 and 144 fps). Apps call it only while the
/// pane is hovered or focused and no text field or modal has focus (§ 2.3).
///
/// ```
/// use regia_input::spacemouse::{NavContext, SpaceMouseState, nav};
///
/// // The cap pushed fully right in an orbit viewport 10 m from its target, for one 60 Hz frame.
/// let state = SpaceMouseState { translation: [1.0, 0.0, 0.0], ..Default::default() };
/// let delta = nav::step(&state, 1.0 / 60.0, NavContext::orbit(10.0));
/// assert!((delta.pan.x - 10.0 / 60.0).abs() < 1e-5); // distance · 1.0 /s at full deflection
/// assert_eq!(delta.zoom, 1.0);
/// ```
///
/// [`NavigationDefaults::MAX_DT`]: super::NavigationDefaults::MAX_DT
pub fn step(state: &SpaceMouseState, dt: f32, ctx: NavContext) -> NavDelta {
    let dt = finite_or(dt, 0.0).clamp(0.0, D::MAX_DT);
    let v = shaped_axes(state, &ctx);
    let gain = finite_or(ctx.sensitivity, 1.0).clamp(
        SpaceMouseConfig::SENSITIVITY_MIN,
        SpaceMouseConfig::SENSITIVITY_MAX,
    ) * dt;
    // The zoom exponent uses the deterministic exp so the multiplier is bit-identical on every
    // target (audit finding 7, ADR-4768).
    let zoom = crate::deterministic::exp_f32(v[2] * D::ZOOM_RATE * gain);
    let mut delta = NavDelta::IDENTITY;
    match ctx.mode {
        NavMode::Orbit | NavMode::Turntable => {
            let distance = finite_or(ctx.distance, 0.0).abs();
            let pan = D::PAN_RATE * distance * gain;
            delta.pan = Vec3::new(v[0] * pan, v[1] * pan, 0.0);
            delta.zoom = zoom;
            rotate(&mut delta, &v, gain, &ctx);
        }
        NavMode::Fly => {
            let speed = D::FLY_SPEED * finite_or(ctx.scene_scale, 0.0).abs() * gain;
            delta.translate = Vec3::new(v[0], v[1], v[2]) * speed;
            rotate(&mut delta, &v, gain, &ctx);
        }
        NavMode::View2D => {
            let pan = D::VIEW2D_PAN_RATE * finite_or(ctx.visible_extent, 0.0).abs() * gain;
            delta.pan = Vec3::new(v[0] * pan, v[1] * pan, 0.0);
            delta.zoom = zoom;
        }
        NavMode::Timeline => {
            delta.scrub = v[4] * D::TIMELINE_SCRUB_RATE * finite_or(ctx.visible_span, 0.0).abs() * gain;
            delta.zoom = zoom;
        }
        NavMode::Step => {
            let (step, held, dir) = list_step(v[5], finite_or(ctx.step_held, 0.0).max(0.0), ctx.step_dir, dt);
            delta.step = step;
            delta.step_held = held;
            delta.step_dir = dir;
        }
    }
    delta
}

/// Orbit / turn: yaw from `ry`, pitch from `rx` (clamped under horizon lock), roll from `rz`
/// only without horizon lock.
fn rotate(delta: &mut NavDelta, v: &[f32; 6], gain: f32, ctx: &NavContext) {
    let turn = D::ROTATION_RATE * gain;
    let yaw = v[4] * turn;
    let mut pitch = v[3] * turn;
    if ctx.horizon_lock {
        // Keep the pitch inside the limit; never jump a camera that is already outside it.
        let limit = D::PITCH_LIMIT_DEG.to_radians();
        let current = finite_or(ctx.pitch, 0.0);
        let next = current + pitch;
        if pitch > 0.0 && next > limit {
            pitch = (limit - current).max(0.0);
        } else if pitch < 0.0 && next < -limit {
            pitch = (-limit - current).min(0.0);
        }
    }
    delta.orbit = Vec2::new(yaw, pitch);
    delta.roll = if ctx.horizon_lock { 0.0 } else { v[5] * turn };
}

/// Step mode (§ 2.3): one step when `|v.rz|` crosses the threshold, then one more every
/// [`NavigationDefaults::STEP_REPEAT_S`] while it stays past it. Tilting right (`rz` < 0) is +1.
/// A tilt the other way from `dir_before` (the previous frame's direction, 0 = unknown) is a new
/// crossing: it steps at once and restarts the hold time. Returns the step, the new hold time and
/// the direction to remember.
///
/// [`NavigationDefaults::STEP_REPEAT_S`]: super::NavigationDefaults::STEP_REPEAT_S
fn list_step(rz: f32, held_before: f32, dir_before: i8, dt: f32) -> (i8, f32, i8) {
    if rz.abs() <= D::STEP_THRESHOLD {
        return (0, 0.0, 0);
    }
    let direction: i8 = if rz < 0.0 { 1 } else { -1 };
    let flipped = dir_before != 0 && dir_before.signum() != direction;
    if held_before <= 0.0 || flipped {
        // Crossing: step now. The hold time counts from this frame (0), stored as the smallest
        // positive value so it reads as "already stepped".
        return (direction, f32::MIN_POSITIVE, direction);
    }
    let held = held_before + dt;
    let repeats_before = (held_before / D::STEP_REPEAT_S).floor();
    let repeats_now = (held / D::STEP_REPEAT_S).floor();
    let step = if repeats_now > repeats_before {
        direction
    } else {
        0
    };
    (step, held, direction)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(axes: [f32; 6]) -> SpaceMouseState {
        SpaceMouseState {
            translation: [axes[0], axes[1], axes[2]],
            rotation: [axes[3], axes[4], axes[5]],
            buttons: 0,
        }
    }

    fn close(a: f32, b: f32, tolerance: f32) -> bool {
        (a - b).abs() <= tolerance
    }

    #[test]
    fn dead_zone_edge_and_quadratic_curve() {
        assert_eq!(shape_axis(0.0), 0.0);
        assert_eq!(shape_axis(0.04), 0.0, "exactly at the dead zone edge");
        assert_eq!(shape_axis(-0.039), 0.0);
        assert!(shape_axis(0.05) > 0.0, "just past the edge moves");
        assert!(shape_axis(0.05) < 1e-3, "and only barely: {}", shape_axis(0.05));
        let half = (0.52_f32 - 0.04) / 0.96; // = 0.5
        assert!(
            close(shape_axis(0.52), half * half, 1e-6),
            "quadratic: 0.25 at t = 0.5"
        );
        assert!(close(shape_axis(-0.52), -0.25, 1e-6), "sign kept");
        assert_eq!(shape_axis(1.0), 1.0, "full speed at the stop");
        assert_eq!(shape_axis(3.0), 1.0, "clamped");
        assert_eq!(shape_axis(f32::NAN), 0.0);
    }

    /// The golden table: one 0.1 s frame at full or half (0.52 → 0.25) deflection per axis.
    #[test]
    fn golden_table_orbit_and_fly() {
        let half = 0.52; // shapes to 0.25
        // (axes, context, expected delta)
        let rows: [([f32; 6], NavContext, NavDelta); 8] = [
            (
                [1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                NavContext::orbit(10.0),
                NavDelta {
                    pan: Vec3::new(1.0, 0.0, 0.0),
                    ..NavDelta::IDENTITY
                },
            ),
            (
                [0.0, half, 0.0, 0.0, 0.0, 0.0],
                NavContext::orbit(4.0),
                NavDelta {
                    pan: Vec3::new(0.0, 0.1, 0.0),
                    ..NavDelta::IDENTITY
                },
            ),
            (
                [0.0, 0.0, -1.0, 0.0, 0.0, 0.0],
                NavContext::orbit(10.0),
                NavDelta {
                    zoom: (-1.5_f32 * 0.1).exp(),
                    ..NavDelta::IDENTITY
                },
            ),
            (
                [0.0, 0.0, 0.0, 0.0, 1.0, 0.0],
                NavContext::orbit(10.0),
                NavDelta {
                    orbit: Vec2::new(0.15, 0.0),
                    ..NavDelta::IDENTITY
                },
            ),
            (
                [0.0, 0.0, 0.0, half, 0.0, 0.0],
                NavContext::orbit(10.0),
                NavDelta {
                    orbit: Vec2::new(0.0, 0.0375),
                    ..NavDelta::IDENTITY
                },
            ),
            (
                [0.0, 0.0, 0.0, 0.0, 0.0, 1.0],
                NavContext::orbit(10.0),
                NavDelta::IDENTITY, // roll ignored under horizon lock
            ),
            (
                [1.0, half, -1.0, 0.0, -1.0, 0.0],
                NavContext::fly(20.0),
                NavDelta {
                    translate: Vec3::new(2.0, 0.5, -2.0),
                    orbit: Vec2::new(-0.15, 0.0),
                    ..NavDelta::IDENTITY
                },
            ),
            (
                [0.0, 0.0, 0.0, 0.0, 0.0, 1.0],
                NavContext {
                    horizon_lock: false,
                    ..NavContext::fly(1.0)
                },
                NavDelta {
                    roll: 0.15,
                    ..NavDelta::IDENTITY
                },
            ),
        ];
        // Every row is one 0.1 s frame (MAX_DT, the largest step).
        let dt = 0.1;
        for (i, (axes, ctx, want)) in rows.into_iter().enumerate() {
            let got = step(&state(axes), dt, ctx);
            assert!(
                close(got.pan.x, want.pan.x, 1e-5),
                "row {i} pan.x: {got:?} vs {want:?}"
            );
            assert!(
                close(got.pan.y, want.pan.y, 1e-5),
                "row {i} pan.y: {got:?} vs {want:?}"
            );
            assert_eq!(got.pan.z, 0.0, "row {i}");
            assert!(
                close(got.zoom, want.zoom, 1e-5),
                "row {i} zoom: {got:?} vs {want:?}"
            );
            assert!(
                close(got.orbit.x, want.orbit.x, 1e-5),
                "row {i} yaw: {got:?} vs {want:?}"
            );
            assert!(
                close(got.orbit.y, want.orbit.y, 1e-5),
                "row {i} pitch: {got:?} vs {want:?}"
            );
            assert!(
                close(got.roll, want.roll, 1e-5),
                "row {i} roll: {got:?} vs {want:?}"
            );
            assert!(
                (got.translate - want.translate).length() < 1e-5,
                "row {i} translate: {got:?} vs {want:?}"
            );
        }
    }

    #[test]
    fn mode_specific_fields_stay_neutral() {
        let s = state([1.0, 1.0, 1.0, 0.0, 0.0, 0.0]);
        let orbit = step(&s, 0.05, NavContext::orbit(3.0));
        assert_eq!(orbit.translate, Vec3::ZERO, "orbit never translates");
        let fly = step(&s, 0.05, NavContext::fly(3.0));
        assert_eq!(fly.pan, Vec3::ZERO, "fly never pans");
        assert_eq!(fly.zoom, 1.0, "fly never zooms");
        assert!(step(&SpaceMouseState::default(), 0.016, NavContext::orbit(5.0)).is_identity());
    }

    /// One second of the same held motion at 30 and at 144 fps lands within 1 %.
    #[test]
    fn frame_rate_independent_at_30_and_144_fps() {
        let s = state([0.7, -0.4, 0.6, 0.5, -0.8, 0.0]);
        let run = |fps: u32| {
            let dt = 1.0 / fps as f32;
            let (mut distance, mut yaw, mut pitch, mut target) = (10.0_f32, 0.0_f32, 0.2_f32, Vec2::ZERO);
            for _ in 0..fps {
                let d = step(&s, dt, NavContext::orbit(distance).with_pitch(pitch));
                target += Vec2::new(d.pan.x, d.pan.y);
                distance *= d.zoom;
                yaw += d.orbit.x;
                pitch += d.orbit.y;
            }
            (distance, yaw, pitch, target)
        };
        let (a, b) = (run(30), run(144));
        let within = |x: f32, y: f32| (x - y).abs() <= 0.01 * x.abs().max(y.abs()).max(1e-3);
        assert!(within(a.0, b.0), "distance {} vs {}", a.0, b.0);
        assert!(within(a.1, b.1), "yaw {} vs {}", a.1, b.1);
        assert!(within(a.2, b.2), "pitch {} vs {}", a.2, b.2);
        assert!(within(a.3.x, b.3.x), "pan x {} vs {}", a.3.x, b.3.x);
        assert!(within(a.3.y, b.3.y), "pan y {} vs {}", a.3.y, b.3.y);
    }

    #[test]
    fn horizon_lock_clamps_pitch_and_blocks_roll() {
        let limit = 89.0_f32.to_radians();
        let s = state([0.0, 0.0, 0.0, 1.0, 0.0, 1.0]);
        let near = step(&s, 0.1, NavContext::orbit(5.0).with_pitch(limit - 0.01));
        assert!(close(near.orbit.y, 0.01, 1e-6), "stops at +89°: {}", near.orbit.y);
        assert_eq!(near.roll, 0.0);
        let at = step(&s, 0.1, NavContext::orbit(5.0).with_pitch(limit));
        assert_eq!(at.orbit.y, 0.0, "no further past the limit");
        let back = step(
            &state([0.0, 0.0, 0.0, -1.0, 0.0, 0.0]),
            0.1,
            NavContext::orbit(5.0).with_pitch(limit),
        );
        assert!(back.orbit.y < 0.0, "moving back inside is free");
        let beyond = step(&s, 0.1, NavContext::orbit(5.0).with_pitch(1.6));
        assert_eq!(beyond.orbit.y, 0.0, "an out-of-range camera is not jumped back");
        let down = step(
            &state([0.0, 0.0, 0.0, -1.0, 0.0, 0.0]),
            0.1,
            NavContext::orbit(5.0).with_pitch(-limit + 0.02),
        );
        assert!(
            close(down.orbit.y, -0.02, 1e-6),
            "stops at -89°: {}",
            down.orbit.y
        );
        let free = step(
            &s,
            0.1,
            NavContext {
                horizon_lock: false,
                ..NavContext::orbit(5.0).with_pitch(limit)
            },
        );
        assert!(close(free.orbit.y, 0.15, 1e-6), "no clamp without horizon lock");
        assert!(close(free.roll, 0.15, 1e-6), "roll with horizon lock off");
    }

    #[test]
    fn sensitivity_and_invert_overrides() {
        let s = state([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
        let base = step(&s, 0.1, NavContext::orbit(2.0));
        let config = SpaceMouseConfig {
            sensitivity: 2.0,
            invert: [true, false, false, false, true, false],
            ..SpaceMouseConfig::DEFAULT
        };
        let fast = step(&s, 0.1, NavContext::orbit(2.0).with_config(&config));
        assert!(
            close(fast.pan.x, -2.0 * base.pan.x, 1e-6),
            "doubled and inverted: {fast:?}"
        );
        assert!(close(fast.orbit.x, -2.0 * base.orbit.x, 1e-6));
        let wild = SpaceMouseConfig {
            sensitivity: 100.0,
            ..SpaceMouseConfig::DEFAULT
        };
        let capped = step(&s, 0.1, NavContext::orbit(2.0).with_config(&wild));
        assert!(
            close(capped.pan.x, 4.0 * base.pan.x, 1e-6),
            "sensitivity capped at 4.0"
        );
        let zoom = step(
            &state([0.0, 0.0, 1.0, 0.0, 0.0, 0.0]),
            0.1,
            NavContext::orbit(2.0).with_config(&config),
        );
        assert!(
            close(zoom.zoom.ln(), 2.0 * 1.5 * 0.1, 1e-6),
            "sensitivity scales zoom"
        );
    }

    #[test]
    fn dominant_axis_isolates_the_largest() {
        let s = state([0.6, 0.0, 0.0, 0.0, -0.9, 0.3]);
        let ctx = NavContext {
            dominant: true,
            ..NavContext::orbit(5.0)
        };
        let d = step(&s, 0.1, ctx);
        assert_eq!(d.pan, Vec3::ZERO, "translation suppressed");
        assert!(d.orbit.x < 0.0, "yaw (the largest) kept: {d:?}");
        let off = step(&s, 0.1, NavContext::orbit(5.0));
        assert!(off.pan.x > 0.0, "default off: all axes move");
    }

    #[test]
    fn dominant_axis_tie_keeps_the_lower_index() {
        let ctx = NavContext {
            dominant: true,
            ..NavContext::orbit(10.0)
        };
        let tied = step(&state([1.0, 0.0, 0.0, 0.0, -1.0, 0.0]), 0.1, ctx);
        assert_eq!(tied.pan, Vec3::new(1.0, 0.0, 0.0));
        assert_eq!(tied.orbit, Vec2::ZERO);
        assert_eq!(tied.roll, 0.0);
        assert_eq!(tied.zoom, 1.0);
        let translation_tie = step(&state([-1.0, 1.0, 0.0, 0.0, 0.0, 0.0]), 0.1, ctx);
        assert_eq!(translation_tie.pan, Vec3::new(-1.0, 0.0, 0.0));
        assert_eq!(translation_tie.orbit, Vec2::ZERO);
    }

    #[test]
    fn hitch_dt_is_capped_and_bad_input_is_harmless() {
        let s = state([1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(
            step(&s, 2.0, NavContext::orbit(1.0)),
            step(&s, D::MAX_DT, NavContext::orbit(1.0))
        );
        assert!(step(&s, -1.0, NavContext::orbit(1.0)).is_identity());
        assert!(step(&s, f32::NAN, NavContext::orbit(1.0)).is_identity());
        let nan = step(&s, 0.1, NavContext::orbit(f32::NAN));
        assert!(nan.is_identity(), "{nan:?}");
    }

    /// Golden table for the 2D and object modes (one 0.1 s frame).
    #[test]
    fn golden_table_turntable_view2d_timeline() {
        let half = 0.52; // shapes to 0.25
        let s = state([1.0, half, -1.0, half, 1.0, 1.0]);
        let orbit = step(&s, 0.1, NavContext::orbit(4.0));
        let turntable = step(&s, 0.1, NavContext::turntable(4.0));
        assert_eq!(
            turntable, orbit,
            "Turntable is the Orbit delta (applied to the object)"
        );

        let view = step(&s, 0.1, NavContext::view2d(200.0));
        assert!(
            close(view.pan.x, 20.0, 1e-4) && close(view.pan.y, 5.0, 1e-4),
            "{view:?}"
        );
        assert!(close(view.zoom, (-0.15_f32).exp(), 1e-6), "push away zooms in");
        assert_eq!(
            (view.orbit, view.roll, view.scrub, view.step),
            (Vec2::ZERO, 0.0, 0.0, 0)
        );

        let timeline = step(&s, 0.1, NavContext::timeline(30.0));
        assert!(
            close(timeline.scrub, 3.0, 1e-5),
            "full twist: one span per second: {timeline:?}"
        );
        assert!(close(timeline.zoom, (-0.15_f32).exp(), 1e-6));
        assert_eq!((timeline.pan, timeline.orbit), (Vec3::ZERO, Vec2::ZERO));
        let back = step(
            &state([0.0, 0.0, 0.0, 0.0, -half, 0.0]),
            0.1,
            NavContext::timeline(30.0),
        );
        assert!(close(back.scrub, -0.75, 1e-5), "twist clockwise scrubs back");
    }

    /// Step: threshold, direction, repeat interval.
    #[test]
    fn step_threshold_direction_and_repeat() {
        let right = state([0.0, 0.0, 0.0, 0.0, 0.0, -1.0]);
        let below = state([0.0, 0.0, 0.0, 0.0, 0.0, -0.7]); // shapes to ~0.47 < 0.5
        let d = step(&below, 0.016, NavContext::list(0.0));
        assert_eq!((d.step, d.step_held), (0, 0.0), "below the threshold");

        // A binary-exact frame time: frame k is k/128 s after the crossing.
        let dt = 1.0 / 128.0;
        let mut held = 0.0;
        let mut steps = Vec::new();
        for frame in 0..100 {
            let d = step(&right, dt, NavContext::list(held));
            held = d.step_held;
            if d.step != 0 {
                steps.push((frame, d.step));
            }
        }
        // Crossing at frame 0, then the first frame at or past 0.35 s (44.8 frames) and 0.70 s
        // (89.6 frames) while held: frames 0, 45, 90.
        assert_eq!(steps, [(0, 1), (45, 1), (90, 1)], "tilt right = next, repeating");
        let just_past = step(
            &state([0.0, 0.0, 0.0, 0.0, 0.0, -0.72]),
            dt,
            NavContext::list(0.0),
        );
        assert_eq!(just_past.step, 1, "shaped 0.7083² = 0.502 > 0.5");
        let released = step(&SpaceMouseState::default(), dt, NavContext::list(held));
        assert_eq!((released.step, released.step_held), (0, 0.0));
        let left = step(&state([0.0, 0.0, 0.0, 0.0, 0.0, 1.0]), dt, NavContext::list(0.0));
        assert_eq!(left.step, -1, "tilt left = previous");
        assert!(!left.is_identity());
        assert!(
            step(&right, dt, NavContext::list(0.05)).is_identity(),
            "held between repeats"
        );
    }

    /// Tilting from one side straight to the other (no frame inside the threshold) is a new
    /// crossing: it steps at once, restarts the hold time and the repeat clock.
    #[test]
    fn step_direction_change_is_a_new_crossing() {
        let right = state([0.0, 0.0, 0.0, 0.0, 0.0, -1.0]);
        let left = state([0.0, 0.0, 0.0, 0.0, 0.0, 1.0]);
        let dt = 1.0 / 128.0;

        // Held right for a while (past the first repeat), then the cap flips left.
        let held_right = NavContext::list_dir(0.5, 1);
        let flip = step(&left, dt, held_right);
        assert_eq!(flip.step, -1, "the flip steps immediately");
        assert_eq!(flip.step_held, f32::MIN_POSITIVE, "and restarts the hold time");
        assert_eq!(flip.step_dir, -1);

        // The same thing the other way.
        let back = step(&right, dt, NavContext::list_dir(0.5, -1));
        assert_eq!(
            (back.step, back.step_held, back.step_dir),
            (1, f32::MIN_POSITIVE, 1)
        );

        // A flip between two repeats (0.2 s: no repeat due) still steps, while the same
        // direction does not.
        assert_eq!(step(&left, dt, NavContext::list_dir(0.2, 1)).step, -1);
        assert_eq!(step(&left, dt, NavContext::list_dir(0.2, -1)).step, 0);

        // After the flip the repeat clock counts from the flip: no step before 0.35 s, one at 0.35 s.
        let (mut held, mut dir) = (flip.step_held, flip.step_dir);
        let mut steps = Vec::new();
        for frame in 1..60 {
            let d = step(&left, dt, NavContext::list_dir(held, dir));
            (held, dir) = (d.step_held, d.step_dir);
            if d.step != 0 {
                steps.push((frame, d.step));
            }
        }
        assert_eq!(
            steps,
            [(45, -1)],
            "first repeat 0.35 s after the flip, same direction"
        );

        // Unknown direction (0) keeps the old behaviour: the tilt is taken as continuing.
        assert_eq!(step(&left, dt, NavContext::list(0.2)).step, 0);
        // Release clears the memory.
        let released = step(&SpaceMouseState::default(), dt, NavContext::list_dir(0.5, 1));
        assert_eq!(
            (released.step, released.step_held, released.step_dir),
            (0, 0.0, 0)
        );
        // A flipped tilt that is also released and re-pressed in one go is just a crossing.
        assert_eq!(step(&left, dt, NavContext::list_dir(0.0, 1)).step, -1);
    }

    #[test]
    fn step_dir_defaults_when_missing_from_stored_json() {
        let ctx: NavContext = serde_json::from_str(
            &serde_json::to_string(&NavContext::list(0.2))
                .expect("serialise")
                .replace(",\"step_dir\":0", ""),
        )
        .expect("older stored contexts have no step_dir");
        assert_eq!(ctx.step_dir, 0);
    }

    /// § 2.7: `sensitivity` multiplies every rate (fly translation and turn, the 2D pan and zoom,
    /// the timeline scrub), not only the orbit ones, and is capped at 4.0 there too.
    #[test]
    fn sensitivity_scales_fly_view2d_and_timeline_rates() {
        let s = state([1.0, 0.52, -1.0, 0.52, 1.0, 0.0]);
        let doubled = SpaceMouseConfig {
            sensitivity: 2.0,
            ..SpaceMouseConfig::DEFAULT
        };
        let wild = SpaceMouseConfig {
            sensitivity: 50.0,
            ..SpaceMouseConfig::DEFAULT
        };
        let dt = 0.05;
        for (name, ctx) in [
            ("fly", NavContext::fly(20.0)),
            ("view2d", NavContext::view2d(200.0)),
            ("timeline", NavContext::timeline(30.0)),
        ] {
            let base = step(&s, dt, ctx);
            for (config, factor) in [(doubled, 2.0_f32), (wild, 4.0)] {
                let got = step(&s, dt, ctx.with_config(&config));
                assert!(
                    (got.translate - base.translate * factor).length() < 1e-4,
                    "{name} translate ×{factor}: {got:?} vs {base:?}"
                );
                assert!(
                    (got.pan - base.pan * factor).length() < 1e-4,
                    "{name} pan ×{factor}: {got:?} vs {base:?}"
                );
                assert!(
                    close(got.orbit.x, base.orbit.x * factor, 1e-6)
                        && close(got.orbit.y, base.orbit.y * factor, 1e-6),
                    "{name} turn ×{factor}: {got:?} vs {base:?}"
                );
                assert!(
                    close(got.scrub, base.scrub * factor, 1e-4),
                    "{name} scrub ×{factor}: {got:?} vs {base:?}"
                );
                assert!(
                    close(got.zoom.ln(), base.zoom.ln() * factor, 1e-5),
                    "{name} zoom exponent ×{factor}: {got:?} vs {base:?}"
                );
            }
        }
        // Each mode moved something, so the comparisons above were not all 0 = 0.
        assert!(step(&s, dt, NavContext::fly(20.0)).translate.length() > 0.0);
        assert!(step(&s, dt, NavContext::view2d(200.0)).pan.length() > 0.0);
        assert!(step(&s, dt, NavContext::timeline(30.0)).scrub != 0.0);
    }

    /// § 2.7: sensitivity is a rate multiplier only. The Step threshold (shaped 0.5) and repeat
    /// interval (0.35 s) are fixed defaults, so the strongest sensitivity moves neither.
    #[test]
    fn sensitivity_moves_neither_the_step_threshold_nor_its_repeat() {
        let fastest = SpaceMouseConfig {
            sensitivity: SpaceMouseConfig::SENSITIVITY_MAX,
            ..SpaceMouseConfig::DEFAULT
        };
        let below = state([0.0, 0.0, 0.0, 0.0, 0.0, -0.7]); // shapes to ~0.47 < 0.5
        assert_eq!(
            step(&below, 0.016, NavContext::list(0.0).with_config(&fastest)).step,
            0
        );
        let right = state([0.0, 0.0, 0.0, 0.0, 0.0, -1.0]);
        let dt = 1.0 / 128.0;
        let mut held = 0.0;
        let mut steps = Vec::new();
        for frame in 0..100 {
            let d = step(&right, dt, NavContext::list(held).with_config(&fastest));
            held = d.step_held;
            if d.step != 0 {
                steps.push(frame);
            }
        }
        assert_eq!(steps, [0, 45, 90], "the same frames as at sensitivity 1");
    }

    /// § 2.2: the curve is `t^CURVE_EXPONENT` (quadratic), and § 2.7's `invert` flips the sign
    /// before the dead zone: an inverted axis is the mirror image of the plain one.
    #[test]
    fn the_curve_follows_its_constant_and_invert_mirrors_the_axis() {
        assert_eq!(D::CURVE_EXPONENT, 2.0);
        for n in [-1.0_f32, -0.73, -0.3, -0.05, 0.0, 0.041, 0.2, 0.52, 0.9, 1.0] {
            let t = ((n.abs() - D::DEAD_ZONE) / (1.0 - D::DEAD_ZONE)).max(0.0);
            let want = n.signum() * t.powf(D::CURVE_EXPONENT);
            assert!(
                close(shape_axis(n), want, 1e-6),
                "n = {n}: {} vs {want}",
                shape_axis(n)
            );
        }
        let s = state([0.3, -0.6, 0.9, -0.2, 0.45, -0.03]);
        let plain = shaped_axes(&s, &NavContext::DEFAULT);
        let inverted = shaped_axes(
            &s,
            &NavContext {
                invert: [true; 6],
                ..NavContext::DEFAULT
            },
        );
        for i in 0..6 {
            assert_eq!(inverted[i], -plain[i], "axis {i}");
        }
        assert_eq!(plain[5], 0.0, "inside the dead zone either way");
    }

    /// § 2.3 Fly: the pitch clamp and the roll rule of horizon lock apply to fly cameras too, and
    /// pushing the cap away (−Z) moves the camera forward (view-space −Z).
    #[test]
    fn fly_obeys_horizon_lock_and_moves_forward_on_push() {
        let limit = D::PITCH_LIMIT_DEG.to_radians();
        let tilt_and_roll = state([0.0, 0.0, 0.0, 1.0, 0.0, 1.0]);
        let near = step(&tilt_and_roll, 0.1, NavContext::fly(5.0).with_pitch(limit - 0.01));
        assert!(close(near.orbit.y, 0.01, 1e-6), "stops at +89°: {near:?}");
        assert_eq!(near.roll, 0.0, "no roll under horizon lock");
        let unlocked = step(
            &tilt_and_roll,
            0.1,
            NavContext {
                horizon_lock: false,
                ..NavContext::fly(5.0).with_pitch(limit)
            },
        );
        assert!(close(unlocked.orbit.y, 0.15, 1e-6) && close(unlocked.roll, 0.15, 1e-6));
        let push = step(&state([0.0, 0.0, -1.0, 0.0, 0.0, 0.0]), 0.1, NavContext::fly(5.0));
        assert!(
            close(push.translate.z, -0.5, 1e-6),
            "forward at scene_scale m/s: {push:?}"
        );
        assert_eq!(push.zoom, 1.0, "fly never zooms");
    }
}
