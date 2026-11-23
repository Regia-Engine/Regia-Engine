// SPDX-License-Identifier: MPL-2.0
//! Viewport camera navigation options (ADR-7755, `docs/specs/19-editor.md` § 3, parity rows
//! P-039, P-040, P-042, P-043, P-044, P-049, P-050): invert per axis, the fly speed steps, orbit
//! around the selection, zoom toward the cursor, the navigation presets (pointer-chord tables and
//! view-key tables, data only), and moving the camera to the selection or the selection to the
//! camera.
//!
//! Every pointer gesture of the viewport goes through [`apply_drag`] and every wheel step through
//! [`CameraNav::wheel_factor`], so the invert options apply in one place. The options live in the
//! [`CameraNav`] resource: the `camera.*` preferences set its defaults, the viewport toolbar and
//! `editor.set_viewport` change it for the session. None of it is a document edit, so none of it
//! is on the undo stack, except [`selection_to_camera`], which moves an entity.

use regia_core::math::{Affine3A, Mat3, Quat, Vec2, Vec3};
use regia_ecs::prelude::*;
use regia_scene::prelude::Transform;

use crate::command::{EditorCommand, SetTransform, UndoStack};
use crate::viewport::EditorCamera;
use crate::viewport_ops::ViewPreset;
use crate::views::ViewCamera;

/// Which drag axes and the wheel run the other way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Invert {
    /// Orbit, horizontal drag.
    pub orbit_x: bool,
    /// Orbit, vertical drag.
    pub orbit_y: bool,
    /// Pan, horizontal drag.
    pub pan_x: bool,
    /// Pan, vertical drag.
    pub pan_y: bool,
    /// Fly look (secondary-button drag), vertical drag.
    pub fly_y: bool,
    /// The wheel: zoom and the fly speed change.
    pub wheel: bool,
}

/// What a pointer drag does to the camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavGesture {
    /// Turn around the orbit pivot (the target, or the selection with
    /// [`CameraNav::orbit_selection`]).
    Orbit,
    /// Move the target in the view plane.
    Pan,
    /// Move toward or away from the target.
    Dolly,
    /// Look around while flying (half-scale orbit; the fly keys move while it is held).
    Look,
}

/// A pointer button, as the chord tables name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavButton {
    /// The primary (left) button.
    Primary,
    /// The secondary (right) button.
    Secondary,
    /// The middle button.
    Middle,
}

/// One row of a pointer-chord table: a button with modifier requirements (`None` = either way)
/// and the gesture it drives. The first matching row wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChordRow {
    /// The held button.
    pub button: NavButton,
    /// Alt must be held (`Some(true)`), must not be held (`Some(false)`), or does not matter.
    pub alt: Option<bool>,
    /// Shift, as for `alt`.
    pub shift: Option<bool>,
    /// The gesture.
    pub gesture: NavGesture,
}

const fn row(button: NavButton, alt: Option<bool>, shift: Option<bool>, gesture: NavGesture) -> ChordRow {
    ChordRow {
        button,
        alt,
        shift,
        gesture,
    }
}

/// The editor's own chords: Alt+primary orbits, middle pans, Alt+secondary dollies, secondary
/// looks (and flies with the fly keys).
pub const DEFAULT_CHORDS: &[ChordRow] = &[
    row(NavButton::Primary, Some(true), None, NavGesture::Orbit),
    row(NavButton::Middle, None, None, NavGesture::Pan),
    row(NavButton::Secondary, Some(true), None, NavGesture::Dolly),
    row(NavButton::Secondary, Some(false), None, NavGesture::Look),
];

/// Three-button mouse emulation, for a device without a middle button: Alt+Shift+primary is the
/// middle-button pan; the rest is [`DEFAULT_CHORDS`].
pub const THREE_BUTTON_CHORDS: &[ChordRow] = &[
    row(NavButton::Primary, Some(true), Some(true), NavGesture::Pan),
    row(NavButton::Primary, Some(true), None, NavGesture::Orbit),
    row(NavButton::Middle, None, None, NavGesture::Pan),
    row(NavButton::Secondary, Some(true), None, NavGesture::Dolly),
    row(NavButton::Secondary, Some(false), None, NavGesture::Look),
];

/// One row of a view-key table: a number-row digit, with or without Alt, and the view it turns to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewKey {
    /// The digit, 0-9.
    pub digit: u8,
    /// Alt held.
    pub alt: bool,
    /// The view.
    pub preset: ViewPreset,
}

const fn key(digit: u8, alt: bool, preset: ViewPreset) -> ViewKey {
    ViewKey { digit, alt, preset }
}

/// The editor's own view keys: bare 1-4 perspective, top, front, side; Alt turns 2-4 to the
/// opposite view (bottom, back, the other side). Ctrl and Shift digits stay the camera bookmarks.
pub const DEFAULT_VIEW_KEYS: &[ViewKey] = &[
    key(1, false, ViewPreset::Perspective),
    key(2, false, ViewPreset::Top),
    key(3, false, ViewPreset::Front),
    key(4, false, ViewPreset::Side),
    key(2, true, ViewPreset::Bottom),
    key(3, true, ViewPreset::Back),
    key(4, true, ViewPreset::OppositeSide),
];

/// Numpad emulation: the number row laid out by where the keys sit on a numeric keypad around
/// its center 5. 5 looks from the front, 8 (above it) from the top, 2 (below) from the bottom,
/// 6 (right) from the +X side, 4 (left) from the -X side, 0 is the perspective view. Alt turns
/// each to the opposite view (Alt+5 is the back).
pub const NUMPAD_VIEW_KEYS: &[ViewKey] = &[
    key(0, false, ViewPreset::Perspective),
    key(5, false, ViewPreset::Front),
    key(5, true, ViewPreset::Back),
    key(8, false, ViewPreset::Top),
    key(8, true, ViewPreset::Bottom),
    key(2, false, ViewPreset::Bottom),
    key(2, true, ViewPreset::Top),
    key(6, false, ViewPreset::Side),
    key(6, true, ViewPreset::OppositeSide),
    key(4, false, ViewPreset::OppositeSide),
    key(4, true, ViewPreset::Side),
];

/// A navigation preset: a pointer-chord table plus a view-key table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum NavPreset {
    /// The editor's own chords and view keys.
    #[default]
    RegiaDefault,
    /// [`THREE_BUTTON_CHORDS`] with the default view keys.
    ThreeButton,
    /// The default chords with [`NUMPAD_VIEW_KEYS`].
    Numpad,
}

impl NavPreset {
    /// Every preset, in menu order.
    pub const ALL: [Self; 3] = [Self::RegiaDefault, Self::ThreeButton, Self::Numpad];

    /// The protocol and preference name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::RegiaDefault => "regia_default",
            Self::ThreeButton => "three_button",
            Self::Numpad => "numpad",
        }
    }

    /// The menu label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::RegiaDefault => "Regia default",
            Self::ThreeButton => "Three-button mouse emulation",
            Self::Numpad => "Numpad emulation",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }

    /// The pointer-chord table.
    #[must_use]
    pub fn chords(self) -> &'static [ChordRow] {
        match self {
            Self::ThreeButton => THREE_BUTTON_CHORDS,
            Self::RegiaDefault | Self::Numpad => DEFAULT_CHORDS,
        }
    }

    /// The view-key table.
    #[must_use]
    pub fn view_keys(self) -> &'static [ViewKey] {
        match self {
            Self::Numpad => NUMPAD_VIEW_KEYS,
            Self::RegiaDefault | Self::ThreeButton => DEFAULT_VIEW_KEYS,
        }
    }

    /// The gesture of `button` held with the modifiers, or `None`.
    #[must_use]
    pub fn gesture(self, button: NavButton, alt: bool, shift: bool) -> Option<NavGesture> {
        self.chords()
            .iter()
            .find(|r| {
                r.button == button && r.alt.is_none_or(|a| a == alt) && r.shift.is_none_or(|s| s == shift)
            })
            .map(|r| r.gesture)
    }

    /// The view of number-row `digit` with or without Alt, or `None`.
    #[must_use]
    pub fn view_key(self, digit: u8, alt: bool) -> Option<ViewPreset> {
        self.view_keys()
            .iter()
            .find(|k| k.digit == digit && k.alt == alt)
            .map(|k| k.preset)
    }
}

/// The fly speed steps the viewport toolbar's slider shows as 1-8, meters per second (log scale:
/// each step doubles; step 4 is the default 6 m/s).
pub const SPEED_STEPS: [f32; 8] = [0.75, 1.5, 3.0, 6.0, 12.0, 24.0, 48.0, 96.0];

/// The slowest and fastest fly speed the wheel reaches, meters per second.
pub const SPEED_RANGE: (f32, f32) = (0.5, 500.0);

/// The orbit distance at which "speed scales with distance" flies at the set speed, meters (the
/// editor camera's start distance).
pub const SPEED_REFERENCE_DISTANCE: f32 = 8.0;

/// The step (1-8) nearest to `speed` on the log scale.
#[must_use]
pub fn speed_step(speed: f32) -> usize {
    let log = speed.max(1e-3).log2();
    let mut best = (0usize, f32::INFINITY);
    for (i, step) in SPEED_STEPS.iter().enumerate() {
        let d = (step.log2() - log).abs();
        if d < best.1 {
            best = (i, d);
        }
    }
    best.0 + 1
}

/// The speed of step `step` (1-8, clamped).
#[must_use]
pub fn speed_for_step(step: usize) -> f32 {
    SPEED_STEPS[step.clamp(1, SPEED_STEPS.len()) - 1]
}

/// Session navigation options (the `camera.*` preferences are their defaults).
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct CameraNav {
    /// Inverted axes.
    pub invert: Invert,
    /// The fly speed scales with the orbit distance ([`SPEED_REFERENCE_DISTANCE`] flies at the
    /// set speed).
    pub speed_with_distance: bool,
    /// Orbit turns around the selection's bounds center (the target when nothing is selected),
    /// without framing it first.
    pub orbit_selection: bool,
    /// The wheel zooms toward the point under the cursor instead of the orbit target.
    pub zoom_to_cursor: bool,
    /// The chord and view-key tables.
    pub preset: NavPreset,
}

impl Default for CameraNav {
    fn default() -> Self {
        Self {
            invert: Invert::default(),
            speed_with_distance: false,
            orbit_selection: false,
            zoom_to_cursor: true,
            preset: NavPreset::RegiaDefault,
        }
    }
}

fn flip(on: bool, v: f32) -> f32 {
    if on { -v } else { v }
}

impl CameraNav {
    /// The drag delta a gesture applies, after the invert options.
    #[must_use]
    pub fn drag_delta(&self, gesture: NavGesture, delta: Vec2) -> Vec2 {
        match gesture {
            NavGesture::Orbit => Vec2::new(
                flip(self.invert.orbit_x, delta.x),
                flip(self.invert.orbit_y, delta.y),
            ),
            NavGesture::Pan => Vec2::new(flip(self.invert.pan_x, delta.x), flip(self.invert.pan_y, delta.y)),
            NavGesture::Look => Vec2::new(delta.x, flip(self.invert.fly_y, delta.y)),
            NavGesture::Dolly => delta,
        }
    }

    /// The wheel direction after [`Invert::wheel`]: +1 zooms in (or speeds up while flying),
    /// -1 out, 0 for no scroll.
    #[must_use]
    pub fn wheel_sign(&self, scroll: f32) -> f32 {
        if scroll == 0.0 {
            0.0
        } else {
            flip(self.invert.wheel, scroll.signum())
        }
    }

    /// The distance factor of one wheel step (`scroll` > 0 zooms in).
    #[must_use]
    pub fn wheel_factor(&self, scroll: f32) -> f32 {
        1.0 - self.wheel_sign(scroll) * 0.1
    }

    /// The fly speed after one wheel step while flying.
    #[must_use]
    pub fn wheel_speed(&self, speed: f32, scroll: f32) -> f32 {
        let factor = match self.wheel_sign(scroll) {
            s if s > 0.0 => 1.1,
            s if s < 0.0 => 0.9,
            _ => 1.0,
        };
        (speed * factor).clamp(SPEED_RANGE.0, SPEED_RANGE.1)
    }

    /// The speed the camera flies at: its set speed, scaled by the orbit distance when
    /// [`Self::speed_with_distance`] is on.
    #[must_use]
    pub fn fly_speed(&self, camera: &EditorCamera) -> f32 {
        if self.speed_with_distance {
            camera.speed * (camera.distance / SPEED_REFERENCE_DISTANCE).clamp(0.05, 50.0)
        } else {
            camera.speed
        }
    }
}

/// The camera's `(right, up, back)` basis (`back` points from the target to the eye).
fn basis(camera: &EditorCamera) -> (Vec3, Vec3, Vec3) {
    let forward = camera.forward();
    let right = camera.right();
    let up = right.cross(forward).normalize_or_zero();
    (right, up, -forward)
}

/// One orbit drag about `pivot`: yaw and pitch change as [`EditorCamera::orbit`] does, and the
/// target turns about `pivot` with the eye, so `pivot` keeps its place on screen and its distance
/// from the eye (a rigid turn of the whole camera about the pivot).
pub fn orbit_about(camera: &mut EditorCamera, pivot: Vec3, dx: f32, dy: f32) {
    let (r0, u0, b0) = basis(camera);
    let offset = camera.target - pivot;
    let local = Vec3::new(offset.dot(r0), offset.dot(u0), offset.dot(b0));
    camera.orbit(dx, dy);
    let (r1, u1, b1) = basis(camera);
    camera.target = pivot + r1 * local.x + u1 * local.y + b1 * local.z;
}

/// Zooms by `factor` (the distance multiplier) keeping the world point `anchor` where it is on
/// screen: the target and eye move along their lines through `anchor`. In an orthographic pane
/// `anchor` must lie in the plane through the target facing the view; there the zoom keeps every
/// point of that plane under the same pixel scaled about `anchor`.
pub fn zoom_toward(camera: &mut EditorCamera, factor: f32, anchor: Vec3) {
    let before = camera.distance;
    camera.zoom(factor);
    let applied = camera.distance / before.max(1e-6);
    camera.target = anchor + (camera.target - anchor) * applied;
}

/// The world point under pane pixel `pixel` in the plane through the view's target facing the
/// view (the zoom anchor). `None` when the ray runs along that plane.
#[must_use]
pub fn cursor_anchor(view: &ViewCamera, pixel: Vec2, size: Vec2) -> Option<Vec3> {
    let (origin, direction) = view.ray(pixel, size);
    let normal = view.forward();
    let denom = direction.dot(normal);
    if denom.abs() < 1e-6 {
        return None;
    }
    let t = (view.target - origin).dot(normal) / denom;
    (t.is_finite() && t > 0.0).then_some(origin + direction * t)
}

/// One pointer gesture: the invert options, then the camera move. `pivot` is the orbit pivot
/// ([`orbit_pivot`]); `None` orbits about the target.
pub fn apply_drag(
    camera: &mut EditorCamera,
    nav: &CameraNav,
    gesture: NavGesture,
    delta: Vec2,
    pivot: Option<Vec3>,
) {
    let d = nav.drag_delta(gesture, delta);
    match gesture {
        NavGesture::Orbit => match pivot {
            Some(pivot) => orbit_about(camera, pivot, d.x, d.y),
            None => camera.orbit(d.x, d.y),
        },
        NavGesture::Pan => camera.pan(d.x, d.y),
        NavGesture::Dolly => camera.zoom(1.0 + d.y * 0.01),
        NavGesture::Look => camera.orbit(d.x * 0.5, d.y * 0.5),
    }
}

/// The orbit pivot: the selection's bounds center when [`CameraNav::orbit_selection`] is on and
/// something with bounds is selected, else `None` (the target).
#[must_use]
pub fn orbit_pivot(world: &World, nav: &CameraNav) -> Option<Vec3> {
    if !nav.orbit_selection {
        return None;
    }
    let selection = world.get_resource::<crate::selection::Selection>()?;
    if selection.entities().is_empty() {
        return None;
    }
    let (min, max) = crate::overlays::selection_aabb(world, selection.entities())?;
    let center = (min + max) * 0.5;
    center.is_finite().then_some(center)
}

/// Why a camera/selection move did nothing (`editor.camera_to_selection`,
/// `editor.selection_to_camera` error codes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraMoveError {
    /// Nothing is selected.
    NothingSelected,
    /// The entity has no `Transform`.
    NoTransform,
    /// The entity's parent cannot be inverted (a zero scale axis).
    SingularParent,
}

impl CameraMoveError {
    /// The protocol code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::NothingSelected => "nothing_selected",
            Self::NoTransform => "no_transform",
            Self::SingularParent => "singular_parent",
        }
    }

    /// A one-line message.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::NothingSelected => "nothing is selected",
            Self::NoTransform => "the selected entity has no transform",
            Self::SingularParent => "the selected entity's parent has a zero scale axis",
        }
    }
}

/// The rotation that looks along `forward` with +Y up (−Z is an entity's forward, as for
/// cameras and lights).
#[must_use]
pub fn look_rotation(forward: Vec3) -> Quat {
    let back = -forward.normalize_or_zero();
    let mut right = Vec3::Y.cross(back);
    if right.length_squared() < 1e-10 {
        right = Vec3::X;
    }
    let right = right.normalize();
    let up = back.cross(right);
    Quat::from_mat3(&Mat3::from_cols(right, up, back)).normalize()
}

/// `camera` placed with its eye at `eye`, looking along `forward` (distance, speed and field of
/// view kept; pitch limited to the orbit range, so a straight-down look tilts slightly).
#[must_use]
pub fn camera_looking(camera: &EditorCamera, eye: Vec3, forward: Vec3) -> EditorCamera {
    let back = -forward.normalize_or_zero();
    let pitch = back.y.clamp(-1.0, 1.0).asin().clamp(-1.45, 1.45);
    let yaw = back.x.atan2(back.z);
    let (sin_yaw, cos_yaw) = yaw.sin_cos();
    let (sin_pitch, cos_pitch) = pitch.sin_cos();
    let back = Vec3::new(cos_pitch * sin_yaw, sin_pitch, cos_pitch * cos_yaw);
    EditorCamera {
        yaw,
        pitch,
        target: eye - back * camera.distance,
        ..camera.clone()
    }
}

/// The world affine of `entity` (its own chain of `Transform`s).
fn world_affine(world: &World, entity: Entity) -> Affine3A {
    crate::hierarchy_ops::world_affine(world, entity)
}

/// Move camera to selection: the editor camera's eye goes to the primary selected entity's world
/// position, looking along its forward (−Z), so a selected camera or light is looked through.
/// Returns the new camera; the caller stores it.
///
/// # Errors
///
/// [`CameraMoveError::NothingSelected`], [`CameraMoveError::NoTransform`].
pub fn camera_to_selection(world: &World, camera: &EditorCamera) -> Result<EditorCamera, CameraMoveError> {
    let primary = world
        .get_resource::<crate::selection::Selection>()
        .and_then(crate::selection::Selection::primary)
        .ok_or(CameraMoveError::NothingSelected)?;
    if world.get::<Transform>(primary).is_none() {
        return Err(CameraMoveError::NoTransform);
    }
    let (_, rotation, translation) = world_affine(world, primary).to_scale_rotation_translation();
    Ok(camera_looking(camera, translation, rotation * Vec3::NEG_Z))
}

/// Move selection to camera: the primary selected entity goes to the camera's eye, facing where
/// the camera looks (−Z forward), keeping its scale; one undo step. Returns the new local
/// transform.
///
/// # Errors
///
/// [`CameraMoveError`] when nothing is selected, the entity has no transform, or its parent
/// cannot be inverted.
pub fn selection_to_camera(world: &mut World, camera: &EditorCamera) -> Result<Transform, CameraMoveError> {
    let primary = world
        .get_resource::<crate::selection::Selection>()
        .and_then(crate::selection::Selection::primary)
        .ok_or(CameraMoveError::NothingSelected)?;
    let current = *world
        .get::<Transform>(primary)
        .ok_or(CameraMoveError::NoTransform)?;
    let world_rotation = look_rotation(camera.forward());
    let parent = world.get::<ChildOf>(primary).map(ChildOf::parent);
    let (translation, rotation) = match parent {
        None => (camera.eye(), world_rotation),
        Some(parent) => {
            let affine = world_affine(world, parent);
            if affine.matrix3.determinant().abs() <= 1e-12 {
                return Err(CameraMoveError::SingularParent);
            }
            let (_, parent_rotation, _) = affine.to_scale_rotation_translation();
            (
                affine.inverse().transform_point3(camera.eye()),
                (parent_rotation.inverse() * world_rotation).normalize(),
            )
        }
    };
    let new = Transform {
        translation,
        rotation,
        scale: current.scale,
    };
    let mut command = SetTransform::capture(world, primary, new);
    command.apply(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    regia_scene::propagate::propagate_transforms(world);
    Ok(new)
}

/// Move camera to selection for the camera keyed `ease_key` in
/// [`crate::viewport_ops::CameraEases`]: the one code path of the palette command, the View menu,
/// the toolbar's Camera menu and `editor.camera_to_selection` (ADR-7755, ADR-7797). It runs
/// [`camera_to_selection`] and stops that camera's running eased move, so a focus or view-preset
/// ease does not overwrite the result on the next frame. Returns the new camera; the caller stores
/// it.
///
/// # Errors
///
/// As [`camera_to_selection`]; the ease keeps running then.
pub fn move_camera_to_selection(
    world: &mut World,
    camera: &EditorCamera,
    ease_key: (u64, usize),
) -> Result<EditorCamera, CameraMoveError> {
    let moved = camera_to_selection(world, camera)?;
    if let Some(mut eases) = world.get_resource_mut::<crate::viewport_ops::CameraEases>() {
        eases.moves.remove(&ease_key);
    }
    Ok(moved)
}

/// The `camera.navigation_preset` preference's choices ([`NavPreset::name`]).
pub const NAV_PRESET_NAMES: &[&str] = &["regia_default", "three_button", "numpad"];

/// Applies one `camera.*` navigation preference to [`CameraNav`] or the main camera's clip planes;
/// false for a key this module does not own (the caller handles it).
pub fn apply_pref(world: &mut World, key: &str, value: &crate::editor_prefs::PrefValue) -> bool {
    use crate::editor_prefs::PrefValue;
    let mut nav = nav_of(world);
    match (key, value) {
        ("camera.navigation_preset", PrefValue::Text(name)) => {
            nav.preset = NavPreset::from_name(name).unwrap_or_default();
        }
        ("camera.near_clip" | "camera.far_clip", PrefValue::Float(v)) => {
            // The plane is checked against the camera's other plane first (a session change of
            // that plane is kept), then against the other plane's preference, so a change of both
            // in one preferences generation, e.g. (0.1, 1.5) to (15, 200), is not refused for
            // the old far plane (ADR-7797).
            let near_key = key == "camera.near_clip";
            let other_pref = world
                .get_resource::<crate::editor_prefs::EditorPreferences>()
                .map(|prefs| {
                    prefs.float(if near_key {
                        "camera.far_clip"
                    } else {
                        "camera.near_clip"
                    }) as f32
                });
            let mut camera = world.get_resource_or_insert_with(EditorCamera::default);
            let pair = |other: f32| {
                if near_key {
                    (*v as f32, other)
                } else {
                    (other, *v as f32)
                }
            };
            let session = pair(if near_key { camera.far } else { camera.near });
            let candidates = std::iter::once(session).chain(other_pref.map(pair));
            let mut last_err = None;
            for (near, far) in candidates {
                match crate::viewport::check_clip(near, far) {
                    Ok(()) => {
                        (camera.near, camera.far) = (near, far);
                        last_err = None;
                        break;
                    }
                    Err(err) => last_err = Some(err),
                }
            }
            if let Some(err) = last_err {
                tracing::warn!(target: "regia_editor", "{key}: {err}");
            }
            return true;
        }
        (_, PrefValue::Bool(on)) => {
            let slot = match key {
                "camera.invert_orbit_x" => &mut nav.invert.orbit_x,
                "camera.invert_orbit_y" => &mut nav.invert.orbit_y,
                "camera.invert_pan_x" => &mut nav.invert.pan_x,
                "camera.invert_pan_y" => &mut nav.invert.pan_y,
                "camera.invert_fly_y" => &mut nav.invert.fly_y,
                "camera.invert_wheel" => &mut nav.invert.wheel,
                "camera.speed_with_distance" => &mut nav.speed_with_distance,
                "camera.orbit_selection" => &mut nav.orbit_selection,
                "camera.zoom_to_cursor" => &mut nav.zoom_to_cursor,
                _ => return false,
            };
            *slot = *on;
        }
        _ => return false,
    }
    world.insert_resource(nav);
    true
}

// ---------------------------------------------------------------------------------------------
// Commands (palette, View menu, keymap) and the toolbar hunk

/// The palette commands of this module (`command_palette::COMMANDS` lists them; a keymap
/// binding runs them like any command).
pub const COMMAND_IDS: &[&str] = &[
    "view.camera_to_selection",
    "view.selection_to_camera",
    "view.toggle_orbit_selection",
    "view.toggle_zoom_to_cursor",
    "camera.speed_up",
    "camera.speed_down",
    "view.cycle_layout",
];

/// The layouts `view.cycle_layout` steps through, in order.
pub const LAYOUT_CYCLE: [&str; 5] = ["single", "side_by_side", "stacked", "one_large_two", "quad"];

fn nav_of(world: &mut World) -> CameraNav {
    *world.get_resource_or_insert_with(CameraNav::default)
}

/// Runs one of [`COMMAND_IDS`] on the main viewport's camera; returns the status line, `None` for
/// another id.
pub fn run_command(world: &mut World, id: &str) -> Option<String> {
    let camera = world.get_resource_or_insert_with(EditorCamera::default).clone();
    let message = match id {
        "view.camera_to_selection" => {
            match move_camera_to_selection(world, &camera, crate::viewport_ops::MAIN_CAMERA) {
                Ok(moved) => {
                    world.insert_resource(moved);
                    "Moved the camera to the selection".to_owned()
                }
                Err(err) => format!("Move camera to selection: {}", err.message()),
            }
        }
        "view.selection_to_camera" => match selection_to_camera(world, &camera) {
            Ok(_) => "Moved the selection to the camera".to_owned(),
            Err(err) => format!("Move selection to camera: {}", err.message()),
        },
        "view.toggle_orbit_selection" | "view.toggle_zoom_to_cursor" => {
            let mut nav = nav_of(world);
            let (name, flag) = if id == "view.toggle_orbit_selection" {
                ("Orbit around selection", &mut nav.orbit_selection)
            } else {
                ("Zoom toward cursor", &mut nav.zoom_to_cursor)
            };
            *flag = !*flag;
            let message = format!("{name} {}", if *flag { "on" } else { "off" });
            world.insert_resource(nav);
            message
        }
        "camera.speed_up" | "camera.speed_down" => {
            let step = speed_step(camera.speed);
            let step = if id == "camera.speed_up" {
                (step + 1).min(SPEED_STEPS.len())
            } else {
                step.saturating_sub(1).max(1)
            };
            let speed = speed_for_step(step);
            world.get_resource_or_insert_with(EditorCamera::default).speed = speed;
            format!("Camera speed {step} ({speed} m/s)")
        }
        "view.cycle_layout" => {
            let mut layout = world
                .get_resource_or_insert_with(crate::views::ViewportLayout::default)
                .clone();
            let at = LAYOUT_CYCLE
                .iter()
                .position(|name| *name == layout.layout_name())
                .unwrap_or(0);
            let next = LAYOUT_CYCLE[(at + 1) % LAYOUT_CYCLE.len()];
            layout.set_layout_name(next);
            world.insert_resource(layout);
            format!("Viewport layout {next}")
        }
        _ => return None,
    };
    Some(message)
}

/// The viewport layout picker of the toolbar (1, 2, 3 or 4 panes; ADR-7755).
pub fn layout_combo(ui: &mut egui::Ui, layout: &mut crate::views::ViewportLayout, instance: Option<u64>) {
    let combo = match instance {
        None => egui::ComboBox::from_id_salt("viewport-layout"),
        Some(id) => egui::ComboBox::from_id_salt(("viewport-layout", id)),
    };
    let current = if layout.quad {
        layout.split.label()
    } else {
        "1 pane"
    };
    let shown = combo
        .selected_text(format!("Layout: {current}"))
        .show_ui(ui, |ui| {
            let mut name = layout.layout_name();
            ui.selectable_value(&mut name, "single", "1 pane");
            for split in crate::views::PaneSplit::ALL {
                ui.selectable_value(&mut name, split.name(), split.label());
            }
            if name != layout.layout_name() {
                layout.set_layout_name(name);
            }
        });
    // A combo box shows only its value: give it a spoken name (ADR-7746; merge of 3207 and 3120).
    crate::a11y_bridge::name_node(&shown.response, "Viewport layout");
}

/// The right end of the viewport toolbar (ADR-7755): the fly speed slider (steps 1-8 and the
/// value), the orbit-around-selection toggle and the Camera menu (field of view, clip planes,
/// navigation options, invert, presets, camera to selection and back). `camera` is the active
/// pane's camera; `perspective` says whether that pane is the perspective one (camera to
/// selection and back are disabled otherwise); `ease_key` is that camera's key in
/// [`crate::viewport_ops::CameraEases`].
pub fn toolbar(
    ui: &mut egui::Ui,
    world: &mut World,
    camera: &mut EditorCamera,
    perspective: bool,
    ease_key: (u64, usize),
) {
    use crate::agent_parity as parity;
    let mut nav = nav_of(world);
    let before = nav;
    let mut step = speed_step(camera.speed);
    let slider = egui::Slider::new(&mut step, 1..=SPEED_STEPS.len())
        .text(format!("Speed {:.1} m/s", camera.speed))
        .clamping(egui::SliderClamping::Always);
    if ui
        .add(slider)
        .on_hover_text("Fly speed (secondary drag with the fly keys); the wheel while flying changes it too")
        .changed()
    {
        camera.speed = speed_for_step(step);
    }
    parity::action_toggle(ui, &parity::VIEWPORT_ORBIT_SELECTION, &mut nav.orbit_selection)
        .on_hover_text("Orbit around the selection's center without framing it first");
    ui.menu_button("Camera", |ui| {
        if perspective {
            ui.add(
                egui::DragValue::new(&mut camera.fov_deg)
                    .range(20.0..=120.0)
                    .speed(0.5)
                    .prefix("Field of view ")
                    .suffix("°"),
            );
            let (mut near, mut far) = (camera.near, camera.far);
            let ((near_min, near_max), (far_min, far_max)) = crate::viewport::CLIP_RANGE;
            ui.add(
                egui::DragValue::new(&mut near)
                    .range(near_min..=near_max)
                    .speed(0.01)
                    .prefix("Near ")
                    .suffix(" m"),
            );
            ui.add(
                egui::DragValue::new(&mut far)
                    .range(far_min..=far_max)
                    .speed(10.0)
                    .prefix("Far ")
                    .suffix(" m"),
            );
            if crate::viewport::check_clip(near, far).is_ok() {
                (camera.near, camera.far) = (near, far);
            }
        } else {
            ui.label("Field of view and clip planes apply to perspective panes");
        }
        ui.separator();
        parity::action_toggle(ui, &parity::VIEWPORT_ZOOM_TO_CURSOR, &mut nav.zoom_to_cursor);
        parity::action_toggle(
            ui,
            &parity::VIEWPORT_SPEED_WITH_DISTANCE,
            &mut nav.speed_with_distance,
        );
        ui.menu_button("Invert", |ui| {
            parity::action_toggle(ui, &parity::VIEWPORT_INVERT_ORBIT_X, &mut nav.invert.orbit_x);
            parity::action_toggle(ui, &parity::VIEWPORT_INVERT_ORBIT_Y, &mut nav.invert.orbit_y);
            parity::action_toggle(ui, &parity::VIEWPORT_INVERT_PAN_X, &mut nav.invert.pan_x);
            parity::action_toggle(ui, &parity::VIEWPORT_INVERT_PAN_Y, &mut nav.invert.pan_y);
            parity::action_toggle(ui, &parity::VIEWPORT_INVERT_FLY_Y, &mut nav.invert.fly_y);
            parity::action_toggle(ui, &parity::VIEWPORT_INVERT_WHEEL, &mut nav.invert.wheel);
        });
        ui.menu_button("Navigation preset", |ui| {
            for preset in NavPreset::ALL {
                ui.selectable_value(&mut nav.preset, preset, preset.label());
            }
        });
        ui.separator();
        // Camera and object (ADR-7797): perspective panes only, through the same functions as
        // the palette, the View menu and the agent. An orthographic pane's stored eye pose is
        // never shown, so moving to or from it would place things nowhere on screen.
        const ORTHO_ONLY: &str = "Perspective panes only: an orthographic pane has no eye to move to or from";
        let to_selection = ui
            .add_enabled_ui(perspective, |ui| {
                parity::action_menu_item(ui, &parity::VIEWPORT_CAMERA_TO_SELECTION)
            })
            .inner
            .on_disabled_hover_text(ORTHO_ONLY);
        if perspective && to_selection.clicked() {
            match move_camera_to_selection(world, camera, ease_key) {
                Ok(moved) => *camera = moved,
                Err(err) => {
                    tracing::info!(target: "regia_editor", code = err.code(), "move camera to selection")
                }
            }
            ui.close();
        }
        let to_camera = ui
            .add_enabled_ui(perspective, |ui| {
                parity::action_menu_item(ui, &parity::VIEWPORT_SELECTION_TO_CAMERA)
            })
            .inner
            .on_disabled_hover_text(ORTHO_ONLY);
        if perspective && to_camera.clicked() {
            if let Err(err) = selection_to_camera(world, camera) {
                tracing::info!(target: "regia_editor", code = err.code(), "move selection to camera");
            }
            ui.close();
        }
    });
    if nav != before {
        world.insert_resource(nav);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam() -> EditorCamera {
        EditorCamera {
            target: Vec3::new(1.0, 2.0, 3.0),
            distance: 10.0,
            yaw: 0.3,
            pitch: 0.2,
            ..EditorCamera::default()
        }
    }

    #[test]
    fn invert_flips_exactly_the_named_axis() {
        let mut nav = CameraNav::default();
        let d = Vec2::new(10.0, -4.0);
        for gesture in [
            NavGesture::Orbit,
            NavGesture::Pan,
            NavGesture::Look,
            NavGesture::Dolly,
        ] {
            assert_eq!(nav.drag_delta(gesture, d), d, "{gesture:?} untouched by default");
        }
        nav.invert.orbit_x = true;
        assert_eq!(nav.drag_delta(NavGesture::Orbit, d), Vec2::new(-10.0, -4.0));
        assert_eq!(nav.drag_delta(NavGesture::Pan, d), d);
        nav.invert.pan_y = true;
        assert_eq!(nav.drag_delta(NavGesture::Pan, d), Vec2::new(10.0, 4.0));
        nav.invert.fly_y = true;
        assert_eq!(nav.drag_delta(NavGesture::Look, d), Vec2::new(10.0, 4.0));
        assert_eq!(nav.wheel_factor(1.0), 0.9);
        nav.invert.wheel = true;
        assert_eq!(nav.wheel_factor(1.0), 1.1);
        assert!(nav.wheel_speed(6.0, 1.0) < 6.0);
    }

    #[test]
    fn inverted_orbit_moves_the_camera_the_opposite_way() {
        let nav = CameraNav::default();
        let mut inverted = CameraNav::default();
        inverted.invert.orbit_x = true;
        inverted.invert.orbit_y = true;
        let (mut a, mut b) = (cam(), cam());
        apply_drag(&mut a, &nav, NavGesture::Orbit, Vec2::new(20.0, 10.0), None);
        apply_drag(&mut b, &inverted, NavGesture::Orbit, Vec2::new(20.0, 10.0), None);
        // yaw -= dx * 0.01, pitch += dy * 0.01.
        assert!(
            (a.yaw - 0.1).abs() < 1e-6 && (a.pitch - 0.3).abs() < 1e-6,
            "{a:?}"
        );
        assert!(
            (b.yaw - 0.5).abs() < 1e-6 && (b.pitch - 0.1).abs() < 1e-6,
            "{b:?}"
        );
    }

    #[test]
    fn orbit_about_a_pivot_keeps_its_screen_point_and_distance() {
        let mut camera = cam();
        let pivot = Vec3::new(3.0, 1.0, -2.0);
        let size = Vec2::new(640.0, 480.0);
        let view = |c: &EditorCamera| ViewCamera::of(crate::views::ViewKind::Perspective, c);
        let before_px = view(&camera).project(pivot, size).expect("in front");
        let before_d = (camera.eye() - pivot).length();
        for _ in 0..5 {
            orbit_about(&mut camera, pivot, 17.0, -6.0);
        }
        let after_px = view(&camera).project(pivot, size).expect("in front");
        let after_d = (camera.eye() - pivot).length();
        assert!((after_d - before_d).abs() < 1e-3, "{before_d} {after_d}");
        assert!(
            (after_px - before_px).length() < 0.05,
            "{before_px:?} {after_px:?}"
        );
        assert!((camera.yaw - (0.3 - 5.0 * 0.17)).abs() < 1e-5);
    }

    #[test]
    fn zoom_toward_keeps_the_anchor_under_its_pixel() {
        for kind in [crate::views::ViewKind::Perspective, crate::views::ViewKind::Top] {
            let mut camera = cam();
            let size = Vec2::new(640.0, 480.0);
            let pixel = Vec2::new(500.0, 120.0);
            let view = ViewCamera::of(kind, &camera);
            let anchor = cursor_anchor(&view, pixel, size).expect("anchor");
            zoom_toward(&mut camera, 0.9, anchor);
            let after = ViewCamera::of(kind, &camera)
                .project(anchor, size)
                .expect("visible");
            assert!((after - pixel).length() < 1.0, "{kind:?}: {after:?}");
            assert!((camera.distance - 9.0).abs() < 1e-5);
        }
    }

    #[test]
    fn speed_steps_round_trip_and_the_nearest_step_is_on_the_log_scale() {
        for (i, s) in SPEED_STEPS.iter().enumerate() {
            assert_eq!(speed_step(*s), i + 1);
            assert_eq!(speed_for_step(i + 1), *s);
        }
        assert_eq!(speed_step(EditorCamera::default().speed), 4);
        assert_eq!(speed_step(8.0), 4); // nearer 6 than 12 in log
        assert_eq!(speed_step(9.0), 5);
        assert_eq!(speed_for_step(0), SPEED_STEPS[0]);
        assert_eq!(speed_for_step(99), SPEED_STEPS[7]);
        let nav = CameraNav {
            speed_with_distance: true,
            ..CameraNav::default()
        };
        let mut camera = EditorCamera::default();
        assert_eq!(nav.fly_speed(&camera), camera.speed);
        camera.distance = 16.0;
        assert_eq!(nav.fly_speed(&camera), camera.speed * 2.0);
    }

    #[test]
    fn presets_are_tables_and_three_button_emulates_the_middle_pan() {
        for preset in NavPreset::ALL {
            assert_eq!(NavPreset::from_name(preset.name()), Some(preset));
            assert_eq!(
                preset.gesture(NavButton::Middle, false, false),
                Some(NavGesture::Pan)
            );
            assert_eq!(
                preset.gesture(NavButton::Primary, false, false),
                None,
                "plain primary selects"
            );
        }
        let d = NavPreset::RegiaDefault;
        assert_eq!(d.gesture(NavButton::Primary, true, true), Some(NavGesture::Orbit));
        assert_eq!(
            d.gesture(NavButton::Secondary, true, false),
            Some(NavGesture::Dolly)
        );
        assert_eq!(
            d.gesture(NavButton::Secondary, false, true),
            Some(NavGesture::Look)
        );
        let t = NavPreset::ThreeButton;
        assert_eq!(t.gesture(NavButton::Primary, true, true), Some(NavGesture::Pan));
        assert_eq!(
            t.gesture(NavButton::Primary, true, false),
            Some(NavGesture::Orbit)
        );
        assert_eq!(d.view_key(2, true), Some(ViewPreset::Bottom));
        assert_eq!(d.view_key(5, false), None);
        let n = NavPreset::Numpad;
        assert_eq!(n.view_key(5, false), Some(ViewPreset::Front));
        assert_eq!(n.view_key(5, true), Some(ViewPreset::Back));
        assert_eq!(n.view_key(1, false), None);
        // Every preset reaches every view.
        for preset in NavPreset::ALL {
            for view in ViewPreset::EVERY {
                assert!(
                    preset.view_keys().iter().any(|k| k.preset == view),
                    "{preset:?} {view:?}"
                );
            }
        }
    }

    #[test]
    fn preference_defaults_match_the_session_defaults() {
        let prefs = crate::editor_prefs::EditorPreferences::default();
        let nav = CameraNav::default();
        let camera = EditorCamera::default();
        assert_eq!(prefs.bool("camera.zoom_to_cursor"), nav.zoom_to_cursor);
        assert_eq!(prefs.bool("camera.orbit_selection"), nav.orbit_selection);
        assert_eq!(prefs.bool("camera.speed_with_distance"), nav.speed_with_distance);
        assert!(!prefs.bool("camera.invert_wheel") && nav.invert == Invert::default());
        assert_eq!(prefs.text("camera.navigation_preset"), nav.preset.name());
        assert_eq!(prefs.float("camera.near_clip") as f32, camera.near);
        assert_eq!(prefs.float("camera.far_clip") as f32, camera.far);
        let names: Vec<&str> = NavPreset::ALL.iter().map(|p| p.name()).collect();
        assert_eq!(names, NAV_PRESET_NAMES);
        // Every command is in the palette.
        for id in COMMAND_IDS {
            assert!(crate::command_palette::find(id).is_some(), "{id}");
        }
    }

    /// Review row 25 (w12-rev-early-k): near and far changed in one preferences generation are
    /// applied as a pair, not each against the other's old value.
    #[test]
    fn near_and_far_preferences_changed_together_apply_as_a_pair() {
        use crate::editor_prefs::{EditorPreferences, PrefValue, apply_prefs};
        let mut world = World::new();
        world.insert_resource(EditorCamera::default());
        let mut prefs = EditorPreferences::default();
        prefs
            .set("camera.near_clip", &PrefValue::Float(0.1))
            .expect("near");
        prefs.set("camera.far_clip", &PrefValue::Float(1.5)).expect("far");
        world.insert_resource(prefs.clone());
        apply_prefs(None, &mut world);
        let camera = world.resource::<EditorCamera>();
        assert_eq!((camera.near, camera.far), (0.1, 1.5));

        prefs
            .set("camera.near_clip", &PrefValue::Float(15.0))
            .expect("near");
        prefs
            .set("camera.far_clip", &PrefValue::Float(200.0))
            .expect("far");
        world.insert_resource(prefs.clone());
        apply_prefs(None, &mut world);
        let camera = world.resource::<EditorCamera>();
        assert_eq!(
            (camera.near, camera.far),
            (15.0, 200.0),
            "both planes as the preferences window shows them"
        );

        // A change of one plane keeps the session's other plane when the pair is valid.
        world.resource_mut::<EditorCamera>().far = 900.0;
        prefs
            .set("camera.near_clip", &PrefValue::Float(20.0))
            .expect("near");
        world.insert_resource(prefs.clone());
        apply_prefs(None, &mut world);
        let camera = world.resource::<EditorCamera>();
        assert_eq!((camera.near, camera.far), (20.0, 900.0));

        // An invalid pair is still refused: near 50 needs far at least 500.
        prefs
            .set("camera.near_clip", &PrefValue::Float(50.0))
            .expect("near");
        world.insert_resource(prefs);
        world.resource_mut::<EditorCamera>().far = 200.0;
        apply_prefs(None, &mut world);
        let camera = world.resource::<EditorCamera>();
        assert_eq!((camera.near, camera.far), (20.0, 200.0), "kept");
    }

    #[test]
    fn camera_looking_puts_the_eye_where_asked() {
        let camera = cam();
        let eye = Vec3::new(4.0, 5.0, 6.0);
        let forward = Vec3::new(1.0, -0.5, -1.0).normalize();
        let placed = camera_looking(&camera, eye, forward);
        assert!((placed.eye() - eye).length() < 1e-4, "{:?}", placed.eye());
        assert!((placed.forward() - forward).length() < 1e-4);
        assert_eq!(placed.distance, camera.distance);
        let q = look_rotation(forward);
        assert!((q * Vec3::NEG_Z - forward).length() < 1e-5);
        assert!((q * Vec3::Y).y > 0.0, "up stays up");
    }
}
