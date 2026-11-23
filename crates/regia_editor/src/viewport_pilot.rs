// SPDX-License-Identifier: MPL-2.0
//! Pilot, look-through and the viewport's camera menu (spec addendum § 3.6-3.7, ADR-7768,
//! `book/agents/viewport-tools.md`).
//!
//! **Pilot** locks the main viewport's editor camera to an entity: a scene camera, a light or any
//! entity with a `Transform`. The camera jumps to the entity's position and facing (its −Z); from
//! then on every camera move (fly, pan, dolly, the SpaceMouse, an eased focus) moves the entity by
//! exactly the camera's motion, and every turn turns it by the camera's turn. While piloting, the
//! orbit drags look around in place instead of circling a point ahead. The whole session is one
//! undo step: the frames that moved the entity coalesce into one transform edit, sealed when the
//! session begins and when it ends. Eject (the banner's button, `Ctrl+Shift+L` again, the
//! `view.eject` command, `editor.pilot { entity: null }`) ends it and leaves the editor camera where
//! it is, with its own field of view back.
//!
//! **Look through** shows a scene camera's view without editing anything: the editor camera takes
//! that camera's pose and field of view every frame (navigation is ignored), until another camera
//! is picked or **Editor camera** returns to the pose saved when it began.
//!
//! For a scene camera (`regia_render::Camera`) both use its vertical field of view, so the framing
//! is the game's. Orthographic scene cameras still draw in perspective, as the runtime frame does
//! (ADR-869).
//!
//! The viewport toolbar's **Camera** menu ([`camera_menu`]) lists every scene camera (with a
//! search field), looks through or pilots one, pilots the selection, ejects, sets the camera
//! preview size ([`crate::camera_preview`]) and toggles the composition guides
//! ([`crate::overlays::ViewportGuides`]). The pane hook ([`pane_hook`]) runs the follow step,
//! draws the "Piloting `<name>`" banner with its Eject button, the picture-in-picture preview and the
//! guides, for the main viewport's perspective pane only.

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::*;
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

use crate::command::{SetTransform, UndoStack};
use crate::viewport::EditorCamera;
use crate::viewport_agent::Handler;

/// The orbit pitch limit of [`EditorCamera::orbit`], radians: a piloted pose never leaves it,
/// so the next drag does not jump.
const PITCH_LIMIT: f32 = 1.45;

/// What the main view is locked to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PilotMode {
    /// Camera moves move and turn the entity (one undo step per session).
    Pilot,
    /// The view follows a scene camera; nothing is edited.
    LookThrough,
}

impl PilotMode {
    /// The protocol name (`pilot`, `look_through`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Pilot => "pilot",
            Self::LookThrough => "look_through",
        }
    }
}

/// The running pilot or look-through session of the main viewport (view state: never saved,
/// never on the undo stack itself).
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct ViewportPilot {
    /// The piloted or viewed entity.
    pub entity: Entity,
    /// Pilot or look through.
    pub mode: PilotMode,
    /// The editor camera when the session began: look-through returns to it, pilot restores its
    /// field of view.
    pub saved: EditorCamera,
    /// The editor camera after the last follow step (the next step's motion is measured from it).
    camera: EditorCamera,
    /// The entity's world rotation relative to the camera's: `entity = camera_rotation * offset`.
    offset: Quat,
    /// The egui frame of the last pane follow step: egui's second pass of a frame repeats the held
    /// fly keys, and the entity must move once per painted frame (review sprint-w12-rev-g1 #1).
    followed_frame: Option<u64>,
}

/// Set when a session began or ended outside the pane: the pane hook then takes the camera
/// resource as the truth for this frame instead of the pane's copy.
#[derive(Resource, Debug, Clone, Copy, Default)]
struct CameraSync;

/// Why a pilot or look-through could not begin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PilotError {
    /// The entity does not exist.
    NotFound,
    /// The entity has no `Transform`.
    NoTransform,
    /// Look through needs a scene camera (`regia_render::Camera`).
    NotACamera,
    /// Pilot edits the level: not in a play world.
    Playing,
    /// Nothing is selected to pilot.
    NoSelection,
    /// `fly` was asked with no pilot session running.
    NotActive,
}

impl PilotError {
    /// The stable agent error reason.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::NotFound => "pilot_not_found",
            Self::NoTransform => "pilot_no_transform",
            Self::NotACamera => "look_through_not_camera",
            Self::Playing => "pilot_in_play",
            Self::NoSelection => "pilot_no_selection",
            Self::NotActive => "pilot_not_active",
        }
    }

    /// A sentence for the status line.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::NotFound => "the entity does not exist",
            Self::NoTransform => "the entity has no transform to pilot",
            Self::NotACamera => "only a scene camera can be looked through",
            Self::Playing => "pilot edits the level; stop play first",
            Self::NoSelection => "select a camera, light or entity to pilot",
            Self::NotActive => "no pilot session is running",
        }
    }

    fn rpc(self) -> RpcError {
        let hint = match self {
            Self::NotFound => "pass an entity id from editor.cameras or the outliner",
            Self::NoTransform => "pilot an entity that has a Transform",
            Self::NotACamera => "pass a camera from editor.cameras, or use editor.pilot",
            Self::Playing => "editor.stop_play first",
            Self::NoSelection => "pass `entity`",
            Self::NotActive => "start one with editor.pilot { entity }",
        };
        RpcError::new(INVALID_PARAMS, self.code(), self.message(), hint)
    }
}

/// The world rotation of the editor camera's view (looking down its −Z).
#[must_use]
pub fn camera_rotation(camera: &EditorCamera) -> Quat {
    Quat::from_rotation_y(camera.yaw) * Quat::from_rotation_x(-camera.pitch)
}

/// The unit vector from the orbit target to the eye.
fn orbit_dir(yaw: f32, pitch: f32) -> Vec3 {
    let (sin_yaw, cos_yaw) = yaw.sin_cos();
    let (sin_pitch, cos_pitch) = pitch.sin_cos();
    Vec3::new(cos_pitch * sin_yaw, sin_pitch, cos_pitch * cos_yaw)
}

/// An entity's world position and rotation (from its local transforms up the hierarchy).
#[must_use]
pub fn entity_pose(world: &World, entity: Entity) -> Option<(Vec3, Quat)> {
    world.get::<Transform>(entity)?;
    let affine = crate::hierarchy_ops::world_affine(world, entity);
    let (_, rotation, translation) = affine.to_scale_rotation_translation();
    Some((translation, rotation.normalize()))
}

/// The editor camera at `eye` looking along `rotation`'s forward (−Z), keeping `base`'s
/// distance, speed and field of view. The pitch stays inside the orbit limit.
#[must_use]
pub fn camera_at(base: &EditorCamera, eye: Vec3, rotation: Quat) -> EditorCamera {
    let back = -(rotation * Vec3::NEG_Z).normalize_or(Vec3::NEG_Z);
    let pitch = back.y.clamp(-1.0, 1.0).asin().clamp(-PITCH_LIMIT, PITCH_LIMIT);
    let yaw = if back.x.abs() + back.z.abs() > 1e-6 {
        back.x.atan2(back.z)
    } else {
        // Straight up or down: the entity's up gives the heading.
        let up = rotation * Vec3::Y;
        (-up.x).atan2(-up.z)
    };
    let mut camera = base.clone();
    camera.yaw = yaw;
    camera.pitch = pitch;
    camera.target = eye - orbit_dir(yaw, pitch) * camera.distance;
    camera
}

/// A scene camera's vertical field of view in degrees.
fn camera_fov_deg(world: &World, entity: Entity) -> Option<f32> {
    world
        .get::<regia_render::Camera>(entity)
        .map(|camera| camera.fov_y.to_degrees())
        .filter(|fov| fov.is_finite())
        .map(|fov| fov.clamp(1.0, 179.0))
}

/// The session, if one runs.
#[must_use]
pub fn current(world: &World) -> Option<&ViewportPilot> {
    world.get_resource::<ViewportPilot>()
}

/// Whether the primary selection is the piloted entity (its gizmo would sit on the eye, so the
/// viewport hides it while the session runs).
#[must_use]
pub fn hides_gizmo(world: &World) -> bool {
    current(world).is_some_and(|session| {
        session.mode == PilotMode::Pilot
            && world
                .get_resource::<crate::selection::Selection>()
                .and_then(crate::selection::Selection::primary)
                == Some(session.entity)
    })
}

/// Begins piloting or looking through `entity` (ending any session first; the camera saved by the
/// first one is kept, so **Editor camera** always returns to where the user was).
///
/// # Errors
///
/// [`PilotError`] when the entity is missing, has no transform, is not a camera (look through),
/// or the world is a play world (pilot).
pub fn start(world: &mut World, entity: Entity, mode: PilotMode) -> Result<(), PilotError> {
    if world.get_entity(entity).is_err() {
        return Err(PilotError::NotFound);
    }
    if mode == PilotMode::Pilot && world.contains_resource::<crate::render::PlayWorld>() {
        return Err(PilotError::Playing);
    }
    if mode == PilotMode::LookThrough && world.get::<regia_render::Camera>(entity).is_none() {
        return Err(PilotError::NotACamera);
    }
    let Some((eye, rotation)) = entity_pose(world, entity) else {
        return Err(PilotError::NoTransform);
    };
    let saved = match world.remove_resource::<ViewportPilot>() {
        Some(previous) => {
            if previous.mode == PilotMode::Pilot {
                world.get_resource_or_insert_with(UndoStack::new).seal();
            }
            previous.saved
        }
        None => world.get_resource_or_insert_with(EditorCamera::default).clone(),
    };
    let mut camera = camera_at(&saved, eye, rotation);
    if let Some(fov) = camera_fov_deg(world, entity) {
        camera.fov_deg = fov;
    }
    let offset = (camera_rotation(&camera).inverse() * rotation).normalize();
    if mode == PilotMode::Pilot {
        // The session's moves coalesce into one entry, apart from any edit before it.
        world.get_resource_or_insert_with(UndoStack::new).seal();
    }
    world.insert_resource(camera.clone());
    world.insert_resource(ViewportPilot {
        entity,
        mode,
        saved,
        camera,
        offset,
        followed_frame: None,
    });
    world.insert_resource(CameraSync);
    Ok(())
}

/// Pilots the primary selection.
///
/// # Errors
///
/// [`PilotError::NoSelection`] with nothing selected, else as [`start`].
pub fn pilot_selection(world: &mut World) -> Result<Entity, PilotError> {
    let entity = world
        .get_resource::<crate::selection::Selection>()
        .and_then(crate::selection::Selection::primary)
        .ok_or(PilotError::NoSelection)?;
    start(world, entity, PilotMode::Pilot).map(|()| entity)
}

/// Ends the session. Pilot leaves the editor camera where it is (its own field of view back) and
/// seals the session's undo step; look through returns to the saved camera. Answers the session
/// that ended.
pub fn eject(world: &mut World) -> Option<ViewportPilot> {
    let state = world.remove_resource::<ViewportPilot>()?;
    match state.mode {
        PilotMode::Pilot => {
            world.get_resource_or_insert_with(UndoStack::new).seal();
            world.get_resource_or_insert_with(EditorCamera::default).fov_deg = state.saved.fov_deg;
        }
        PilotMode::LookThrough => {
            world.insert_resource(state.saved.clone());
        }
    }
    world.insert_resource(CameraSync);
    Some(state)
}

/// Moves `entity` by the world-space `delta` and, when given, gives it the world rotation
/// `rotation`, as one coalescing transform edit (the session's undo step). A move that does not
/// turn keeps the entity's rotation bit for bit.
fn move_entity(world: &mut World, entity: Entity, delta: Vec3, rotation: Option<Quat>) {
    let Some(old) = world.get::<Transform>(entity).copied() else {
        return;
    };
    let parent = world.get::<ChildOf>(entity).map(ChildOf::parent);
    let (local_delta, parent_rotation) = match parent {
        Some(parent) => {
            let affine = crate::hierarchy_ops::world_affine(world, parent);
            if affine.matrix3.determinant().abs() <= 1e-12 {
                return;
            }
            let (_, parent_rotation, _) = affine.to_scale_rotation_translation();
            (affine.inverse().transform_vector3(delta), parent_rotation)
        }
        None => (delta, Quat::IDENTITY),
    };
    let mut new = old;
    new.translation = old.translation + local_delta;
    if let Some(rotation) = rotation {
        new.rotation = (parent_rotation.inverse() * rotation).normalize();
    }
    if new == old || !new.translation.is_finite() || !new.rotation.is_finite() {
        return;
    }
    world.entity_mut(entity).insert(new);
    world
        .get_resource_or_insert_with(UndoStack::new)
        .push(Box::new(SetTransform {
            entity: entity.to_bits(),
            old,
            new,
        }));
}

/// One follow step of the session on `camera` (the main viewport's perspective camera after this
/// frame's input). Pilot: the camera's motion since the last step moves the entity (looking turns
/// in place); look through: the camera takes the scene camera's pose. A session whose entity is
/// gone ends. Answers whether a session runs after the step.
pub fn follow(world: &mut World, camera: &mut EditorCamera) -> bool {
    let Some(mut state) = world.get_resource::<ViewportPilot>().cloned() else {
        return false;
    };
    let Some((eye, rotation)) = entity_pose(world, state.entity) else {
        if state.mode == PilotMode::Pilot {
            world.get_resource_or_insert_with(UndoStack::new).seal();
        }
        world.remove_resource::<ViewportPilot>();
        return false;
    };
    match state.mode {
        PilotMode::LookThrough => {
            let mut next = camera_at(camera, eye, rotation);
            next.fov_deg = camera_fov_deg(world, state.entity).unwrap_or(next.fov_deg);
            *camera = next;
        }
        PilotMode::Pilot => {
            if *camera == state.camera {
                return true;
            }
            let from = &state.camera;
            // Fly and pan move the target with the eye; a dolly shortens the distance; an orbit
            // turns around the target. Piloting keeps the distance, moves the eye by the first two
            // and turns in place for the third.
            let moved = camera.target - from.target;
            let dolly = from.distance - camera.distance;
            let mut next = camera.clone();
            next.distance = from.distance;
            let dir = orbit_dir(next.yaw, next.pitch);
            let from_eye = from.eye();
            let eye_next = from_eye + moved - dir * dolly;
            next.target = eye_next - dir * next.distance;
            let turned = (next.yaw != from.yaw || next.pitch != from.pitch)
                .then(|| (camera_rotation(&next) * state.offset).normalize());
            move_entity(world, state.entity, eye_next - from_eye, turned);
            *camera = next;
        }
    }
    state.camera = camera.clone();
    world.insert_resource(state);
    true
}

/// [`follow`] once per egui frame `frame` for a pilot session (the pane's path): egui's second pass
/// of a frame repeats the held fly keys and would move the entity twice in one painted frame, so a
/// repeat of the frame leaves it; the next frame's step carries the camera's motion since the last
/// step. Look through follows on every pass (it only sets the camera). Answers whether a session
/// runs.
pub fn follow_in_frame(world: &mut World, camera: &mut EditorCamera, frame: u64) -> bool {
    let Some(state) = world.get_resource::<ViewportPilot>() else {
        return false;
    };
    if state.mode == PilotMode::Pilot && state.followed_frame == Some(frame) {
        return true;
    }
    let running = follow(world, camera);
    if let Some(mut state) = world.get_resource_mut::<ViewportPilot>() {
        state.followed_frame = Some(frame);
    }
    running
}

/// [`follow`] on the world's [`EditorCamera`] (the agent path, with no pane drawing).
pub fn follow_world(world: &mut World) -> bool {
    let mut camera = world.get_resource_or_insert_with(EditorCamera::default).clone();
    let running = follow(world, &mut camera);
    if running {
        world.insert_resource(camera);
    }
    running
}

/// Every scene camera of the world (entities with `regia_render::Camera` and a `Transform`),
/// sorted by name, filtered by a case-insensitive `query` on the name.
#[must_use]
pub fn scene_cameras(world: &mut World, query: &str) -> Vec<(Entity, String)> {
    let needle = query.trim().to_lowercase();
    let mut cameras: Vec<(Entity, String)> = world
        .query_filtered::<Entity, (With<regia_render::Camera>, With<Transform>)>()
        .iter(world)
        .collect::<Vec<_>>()
        .into_iter()
        .map(|entity| (entity, crate::outliner::display_name(world, entity)))
        .filter(|(_, name)| needle.is_empty() || name.to_lowercase().contains(&needle))
        .collect();
    cameras.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.to_bits().cmp(&b.0.to_bits())));
    cameras
}

/// Runs a palette command of this module (`view.pilot`, `view.eject`, `view.look_through_next`,
/// `view.editor_camera`); answers the status line.
pub fn run_command(world: &mut World, id: &str) -> String {
    match id {
        "view.pilot" => {
            if current(world).is_some_and(|p| p.mode == PilotMode::Pilot) {
                return eject_message(world);
            }
            match pilot_selection(world) {
                Ok(entity) => format!("Piloting {}", crate::outliner::display_name(world, entity)),
                Err(err) => format!("Pilot: {}", err.message()),
            }
        }
        "view.eject" => eject_message(world),
        "view.look_through_next" => {
            let cameras = scene_cameras(world, "");
            if cameras.is_empty() {
                return "The level has no cameras".to_owned();
            }
            let at = current(world)
                .filter(|p| p.mode == PilotMode::LookThrough)
                .and_then(|p| cameras.iter().position(|(e, _)| *e == p.entity));
            let (entity, name) = cameras[at.map_or(0, |i| (i + 1) % cameras.len())].clone();
            match start(world, entity, PilotMode::LookThrough) {
                Ok(()) => format!("Looking through {name}"),
                Err(err) => format!("Look through: {}", err.message()),
            }
        }
        "view.editor_camera" => {
            if current(world).is_some() {
                eject_message(world)
            } else {
                "Already on the editor camera".to_owned()
            }
        }
        _ => String::new(),
    }
}

fn eject_message(world: &mut World) -> String {
    match eject(world) {
        Some(state) => {
            let name = crate::outliner::display_name(world, state.entity);
            match state.mode {
                PilotMode::Pilot => format!("Ejected from {name}"),
                PilotMode::LookThrough => "Back to the editor camera".to_owned(),
            }
        }
        None => "Nothing to eject".to_owned(),
    }
}

/// The pane hook of the main viewport's perspective pane, before the pane stores its camera: the
/// follow step, the pilot banner with Eject, the camera preview and the composition guides.
pub fn pane_hook(
    ui: &mut egui::Ui,
    world: &mut World,
    device: Option<&regia_render::RenderDevice>,
    rect: egui::Rect,
    camera: &mut EditorCamera,
) {
    if world.remove_resource::<CameraSync>().is_some() {
        *camera = world.get_resource_or_insert_with(EditorCamera::default).clone();
    }
    follow_in_frame(world, camera, ui.ctx().cumulative_frame_nr());
    crate::overlays::paint_guides(ui, rect, world);
    crate::camera_preview::draw(ui, world, device, rect);
    banner(ui, world, rect, camera);
}

/// "Piloting `<name>`" or "Looking through `<name>`" and its Eject button, at the pane's top left.
fn banner(ui: &mut egui::Ui, world: &mut World, rect: egui::Rect, camera: &mut EditorCamera) {
    let Some(state) = current(world).cloned() else {
        return;
    };
    let name = crate::outliner::display_name(world, state.entity);
    let text = match state.mode {
        PilotMode::Pilot => format!("Piloting {name}"),
        PilotMode::LookThrough => format!("Looking through {name}"),
    };
    let spacing = ui.spacing().item_spacing.x;
    let area = egui::Rect::from_min_size(
        rect.left_top() + egui::vec2(spacing, spacing),
        egui::vec2(
            (rect.width() - 2.0 * spacing).max(1.0),
            ui.spacing().interact_size.y * 1.5,
        ),
    );
    let mut ejected = false;
    ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(text).strong());
                let action = match state.mode {
                    PilotMode::Pilot => &crate::agent_parity::VIEW_EJECT,
                    PilotMode::LookThrough => &crate::agent_parity::VIEW_EDITOR_CAMERA,
                };
                if crate::agent_parity::action_button(ui, action).clicked() {
                    ejected = true;
                }
            });
        });
    });
    if ejected {
        eject(world);
        world.remove_resource::<CameraSync>();
        *camera = world.get_resource_or_insert_with(EditorCamera::default).clone();
    }
}

/// The egui temp-data key of the camera menu's search text.
fn search_id() -> egui::Id {
    egui::Id::new("regia_editor::viewport-camera-search")
}

/// The viewport toolbar's **Camera** menu (main viewport): scene cameras with a search field
/// (look through, pilot), Editor camera, pilot the selection, eject, the preview size and the
/// composition guides. Every item is a focusable button (Tab reaches it; Enter runs it).
pub fn camera_menu(ui: &mut egui::Ui, world: &mut World) {
    use crate::agent_parity as parity;
    let session = current(world).cloned();
    let title = match &session {
        Some(state) => format!("Camera: {}", crate::outliner::display_name(world, state.entity)),
        None => "Camera: Editor".to_owned(),
    };
    ui.menu_button(title, |ui| {
        let mut query = ui
            .ctx()
            .data(|data| data.get_temp::<String>(search_id()))
            .unwrap_or_default();
        let search = ui.add(egui::TextEdit::singleline(&mut query).hint_text("Search cameras"));
        let search = crate::a11y_audit::spoken_only(search, egui::WidgetType::TextEdit, "Search cameras");
        if search.changed() {
            ui.ctx().data_mut(|data| data.insert_temp(search_id(), query.clone()));
        }
        let editor_label = if session.is_none() {
            "Editor camera (current)".to_owned()
        } else {
            parity::VIEW_EDITOR_CAMERA.label.to_owned()
        };
        if parity::action_button_text(ui, &parity::VIEW_EDITOR_CAMERA, editor_label).clicked() {
            eject(world);
            ui.close();
        }
        let cameras = scene_cameras(world, &query);
        if cameras.is_empty() {
            ui.label("No scene cameras match");
        }
        for (entity, name) in cameras {
            ui.horizontal(|ui| {
                let viewing = session
                    .as_ref()
                    .is_some_and(|s| s.entity == entity && s.mode == PilotMode::LookThrough);
                let label = if viewing { format!("{name} (viewing)") } else { name.clone() };
                if parity::action_button_text(ui, &parity::VIEW_LOOK_THROUGH, label).clicked() {
                    if let Err(err) = start(world, entity, PilotMode::LookThrough) {
                        tracing::warn!(target: "regia_editor", code = err.code(), "look through: {}", err.message());
                    }
                    ui.close();
                }
                if parity::action_button_text(ui, &parity::VIEW_PILOT_CAMERA, format!("Pilot {name}")).clicked() {
                    if let Err(err) = start(world, entity, PilotMode::Pilot) {
                        tracing::warn!(target: "regia_editor", code = err.code(), "pilot: {}", err.message());
                    }
                    ui.close();
                }
            });
        }
        ui.separator();
        let has_selection = world
            .get_resource::<crate::selection::Selection>()
            .is_some_and(|s| s.primary().is_some());
        if parity::action_enabled(ui, has_selection, &parity::VIEW_PILOT, Some("Pilot selection (Ctrl+Shift+L)".to_owned()))
            .clicked()
        {
            if let Err(err) = pilot_selection(world) {
                tracing::warn!(target: "regia_editor", code = err.code(), "pilot: {}", err.message());
            }
            ui.close();
        }
        if session.as_ref().is_some_and(|s| s.mode == PilotMode::Pilot)
            && parity::action_button(ui, &parity::VIEW_EJECT).clicked()
        {
            eject(world);
            ui.close();
        }
        ui.separator();
        crate::camera_preview::size_menu(ui, world);
        ui.separator();
        crate::overlays::guides_menu(ui, world);
    });
}

// ---------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------

fn entity_param(world: &World, params: &Value) -> Result<Option<Option<Entity>>, RpcError> {
    match params.get("entity") {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(value) => {
            let bits = value
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params("`entity` is an entity id or null"))?;
            let entity = Entity::try_from_bits(bits).ok_or_else(|| PilotError::NotFound.rpc())?;
            if world.get_entity(entity).is_err() {
                return Err(PilotError::NotFound.rpc());
            }
            Ok(Some(Some(entity)))
        }
    }
}

fn session_json(world: &World) -> Value {
    let camera = world.get_resource::<EditorCamera>().cloned().unwrap_or_default();
    let eye = camera.eye();
    match current(world) {
        Some(state) => json!({
            "mode": state.mode.name(),
            "entity": state.entity.to_bits(),
            "name": crate::outliner::display_name(world, state.entity),
            "eye": [eye.x, eye.y, eye.z],
            "fov_deg": camera.fov_deg,
        }),
        None => json!({
            "mode": Value::Null,
            "entity": Value::Null,
            "eye": [eye.x, eye.y, eye.z],
            "fov_deg": camera.fov_deg,
        }),
    }
}

/// `editor.pilot`.
fn pilot_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    match entity_param(world, params)? {
        Some(Some(entity)) => start(world, entity, PilotMode::Pilot).map_err(PilotError::rpc)?,
        Some(None) => {
            eject(world);
        }
        None => {}
    }
    if let Some(fly) = params.get("fly") {
        if current(world).is_none_or(|p| p.mode != PilotMode::Pilot) {
            return Err(PilotError::NotActive.rpc());
        }
        let axis = |name: &str| fly.get(name).and_then(Value::as_f64).unwrap_or(0.0) as f32;
        let seconds = fly.get("seconds").and_then(Value::as_f64).unwrap_or(1.0 / 60.0) as f32;
        if !(seconds.is_finite() && (0.0..=10.0).contains(&seconds)) {
            return Err(RpcError::invalid_params("`fly.seconds` is 0 to 10"));
        }
        let steps = fly.get("steps").and_then(Value::as_u64).unwrap_or(1).min(600);
        for _ in 0..steps {
            let mut camera = world.get_resource_or_insert_with(EditorCamera::default).clone();
            camera.fly(
                seconds,
                axis("forward").clamp(-1.0, 1.0),
                axis("right").clamp(-1.0, 1.0),
                axis("up").clamp(-1.0, 1.0),
            );
            world.insert_resource(camera);
            follow_world(world);
        }
    }
    Ok(session_json(world))
}

/// `editor.cameras`.
fn cameras_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let query = params.get("query").and_then(Value::as_str).unwrap_or("");
    let session = current(world).map(|s| (s.entity, s.mode));
    let rows: Vec<Value> = scene_cameras(world, query)
        .into_iter()
        .map(|(entity, name)| {
            json!({
                "entity": entity.to_bits(),
                "name": name,
                "fov_deg": camera_fov_deg(world, entity),
                "viewing": session.and_then(|(e, mode)| (e == entity).then_some(mode.name())),
            })
        })
        .collect();
    Ok(json!({ "cameras": rows, "session": session_json(world) }))
}

/// `editor.look_through`.
fn look_through_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    match entity_param(world, params)? {
        Some(Some(entity)) => start(world, entity, PilotMode::LookThrough).map_err(PilotError::rpc)?,
        Some(None) => {
            eject(world);
        }
        None => {
            return Err(RpcError::invalid_params(
                "`entity` (a camera id, or null for the editor camera) is required",
            ));
        }
    }
    follow_world(world);
    Ok(session_json(world))
}

/// The agent methods of pilot and look-through (`book/agents/viewport-tools.md`).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.pilot",
                tool: "pilot",
                description: "Pilot an entity from the main viewport (ADR-7768): the editor camera locks to it and camera moves move it (one undo step per session). entity null ejects. fly runs fixed fly steps through the pilot.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: false,
                        description: "Entity to pilot (a camera, light or any entity with a Transform); null ejects.",
                    },
                    ParamSchema {
                        name: "fly",
                        kind: "object",
                        required: false,
                        description: "{ forward, right, up (each -1..1), seconds (default 1/60), steps (default 1, at most 600) }: fly steps applied through the running pilot.",
                    },
                ],
                result: "{ mode: pilot|look_through|null, entity, name, eye, fov_deg }",
                mutating: true,
                permission: Permission::EditScene,
            },
            pilot_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.cameras",
                tool: "cameras",
                description: "Every scene camera of the level (the viewport's Camera menu), sorted by name, and the running pilot or look-through session.",
                params: &[ParamSchema {
                    name: "query",
                    kind: "string",
                    required: false,
                    description: "Case-insensitive name filter (the menu's search field).",
                }],
                result: "{ cameras: [{ entity, name, fov_deg, viewing }], session }",
                mutating: false,
                permission: Permission::Read,
            },
            cameras_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.look_through",
                tool: "look_through",
                description: "Look through a scene camera in the main viewport without editing it (ADR-7768); entity null returns to the editor camera.",
                params: &[ParamSchema {
                    name: "entity",
                    kind: "number",
                    required: true,
                    description: "A camera id from editor.cameras, or null.",
                }],
                result: "{ mode, entity, name, eye, fov_deg }",
                mutating: false,
                permission: Permission::Read,
            },
            look_through_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_with_light() -> (World, Entity) {
        let mut world = World::new();
        world.insert_resource(EditorCamera::default());
        world.insert_resource(UndoStack::new());
        let light = world
            .spawn((
                Transform {
                    translation: Vec3::new(2.0, 3.0, -4.0),
                    rotation: Quat::from_rotation_y(0.7) * Quat::from_rotation_x(-0.3),
                    scale: Vec3::splat(1.5),
                },
                regia_render::PointLight::default(),
            ))
            .id();
        (world, light)
    }

    #[test]
    fn pilot_starts_at_the_entity_eye_and_facing() {
        let (mut world, light) = world_with_light();
        start(&mut world, light, PilotMode::Pilot).expect("pilot");
        let camera = world.resource::<EditorCamera>().clone();
        let (eye, rotation) = entity_pose(&world, light).expect("pose");
        assert!(
            (camera.eye() - eye).length() < 1e-4,
            "{:?} vs {eye:?}",
            camera.eye()
        );
        let forward = rotation * Vec3::NEG_Z;
        assert!((camera.forward() - forward).length() < 1e-4);
    }

    /// The brief's acceptance: pilot a light, fly 10 fixed steps, eject: the light moved by exactly
    /// the camera's motion and one undo restores it.
    #[test]
    fn piloting_a_light_moves_it_by_the_camera_motion_and_one_undo_restores_it() {
        let (mut world, light) = world_with_light();
        let before = *world.get::<Transform>(light).expect("transform");
        let depth = world.resource::<UndoStack>().undo_depth();
        start(&mut world, light, PilotMode::Pilot).expect("pilot");
        let eye_start = world.resource::<EditorCamera>().eye();
        for step in 0..10 {
            let mut camera = world.resource::<EditorCamera>().clone();
            camera.fly(1.0 / 60.0, 1.0, if step % 2 == 0 { 0.5 } else { -0.25 }, 0.3);
            world.insert_resource(camera);
            assert!(follow_world(&mut world));
        }
        let eye_end = world.resource::<EditorCamera>().eye();
        eject(&mut world).expect("eject");
        let after = *world.get::<Transform>(light).expect("transform");
        let camera_motion = eye_end - eye_start;
        assert!(camera_motion.length() > 0.5, "the camera flew: {camera_motion:?}");
        let light_motion = after.translation - before.translation;
        assert!(
            (light_motion - camera_motion).length() < 1e-4,
            "light {light_motion:?} vs camera {camera_motion:?}"
        );
        assert_eq!(after.rotation, before.rotation, "flying does not turn it");
        assert_eq!(after.scale, before.scale);
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            depth + 1,
            "one undo step"
        );
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(*world.get::<Transform>(light).expect("transform"), before);
    }

    #[test]
    fn looking_around_while_piloting_turns_in_place() {
        let (mut world, light) = world_with_light();
        start(&mut world, light, PilotMode::Pilot).expect("pilot");
        let before = *world.get::<Transform>(light).expect("transform");
        let mut camera = world.resource::<EditorCamera>().clone();
        camera.orbit(40.0, 10.0);
        world.insert_resource(camera);
        follow_world(&mut world);
        let after = *world.get::<Transform>(light).expect("transform");
        assert!(
            (after.translation - before.translation).length() < 1e-4,
            "stays put"
        );
        assert!(after.rotation.angle_between(before.rotation) > 0.1, "turns");
        let camera = world.resource::<EditorCamera>().clone();
        let (_, rotation) = entity_pose(&world, light).expect("pose");
        assert!(
            (camera.forward() - rotation * Vec3::NEG_Z).length() < 1e-3,
            "still looks along it"
        );
    }

    #[test]
    fn look_through_follows_a_camera_without_editing_and_editor_camera_returns() {
        let mut world = World::new();
        let editor = EditorCamera::default();
        world.insert_resource(editor.clone());
        world.insert_resource(UndoStack::new());
        let cam = world
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 2.0, 10.0)),
                regia_render::Camera {
                    fov_y: 30f32.to_radians(),
                    ..Default::default()
                },
            ))
            .id();
        assert_eq!(start(&mut world, cam, PilotMode::LookThrough), Ok(()));
        let mut camera = world.resource::<EditorCamera>().clone();
        assert!(
            (camera.fov_deg - 30.0).abs() < 1e-3,
            "exact framing: {}",
            camera.fov_deg
        );
        camera.fly(1.0, 1.0, 0.0, 0.0);
        follow(&mut world, &mut camera);
        assert!(
            (camera.eye() - Vec3::new(0.0, 2.0, 10.0)).length() < 1e-4,
            "navigation is ignored"
        );
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 0, "nothing edited");
        eject(&mut world);
        assert_eq!(*world.resource::<EditorCamera>(), editor);
        let point = world.spawn(Transform::default()).id();
        assert_eq!(
            start(&mut world, point, PilotMode::LookThrough),
            Err(PilotError::NotACamera)
        );
    }

    #[test]
    fn a_piloted_child_moves_by_the_world_motion() {
        let (mut world, _) = world_with_light();
        let parent = world
            .spawn(Transform {
                translation: Vec3::new(1.0, 0.0, 0.0),
                rotation: Quat::from_rotation_y(1.0),
                scale: Vec3::splat(2.0),
            })
            .id();
        let child = world
            .spawn((Transform::from_translation(Vec3::Y), ChildOf(parent)))
            .id();
        let (start_eye, _) = entity_pose(&world, child).expect("pose");
        start(&mut world, child, PilotMode::Pilot).expect("pilot");
        let mut camera = world.resource::<EditorCamera>().clone();
        camera.target += Vec3::new(0.0, 0.0, 3.0);
        world.insert_resource(camera);
        follow_world(&mut world);
        let (end_eye, _) = entity_pose(&world, child).expect("pose");
        assert!(
            (end_eye - start_eye - Vec3::new(0.0, 0.0, 3.0)).length() < 1e-4,
            "{end_eye:?}"
        );
    }

    #[test]
    fn guides_lay_out_the_letterbox_thirds_cross_and_safe_boxes() {
        use crate::overlays::{ViewportGuides, guide_lines, letterbox_rect, safe_rects};
        let pane = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 900.0));
        let wide = letterbox_rect(pane, Some(2.39));
        assert!((wide.width() - 1600.0).abs() < 1e-3);
        assert!(
            (wide.height() - 1600.0 / 2.39).abs() < 1e-2,
            "bars top and bottom"
        );
        assert_eq!(wide.center(), pane.center());
        let tall = letterbox_rect(pane, Some(9.0 / 16.0));
        assert!((tall.height() - 900.0).abs() < 1e-3);
        assert!(
            (tall.width() - 900.0 * 9.0 / 16.0).abs() < 1e-2,
            "bars left and right"
        );
        assert_eq!(letterbox_rect(pane, None), pane);
        let guides = ViewportGuides {
            thirds: true,
            center: true,
            safe_areas: true,
            letterbox: None,
        };
        let lines = guide_lines(pane, &guides);
        assert_eq!(lines.len(), 6, "four thirds lines and a two-arm cross");
        assert!((lines[0][0].x - 1600.0 / 3.0).abs() < 1e-3);
        assert!((lines[1][0].y - 300.0).abs() < 1e-3);
        let safe = safe_rects(pane, &guides);
        assert!((safe[0].width() - 1600.0 * crate::overlays::SAFE_ACTION).abs() < 1e-2);
        assert!((safe[1].height() - 900.0 * crate::overlays::SAFE_TITLE).abs() < 1e-2);
        assert!(guide_lines(pane, &ViewportGuides::default()).is_empty());
        assert!(!ViewportGuides::default().any());
    }

    #[test]
    fn the_piloted_selection_has_no_gizmo_while_piloted() {
        let (mut world, light) = world_with_light();
        let mut selection = crate::selection::Selection::default();
        selection.select(light, crate::selection::SelectMode::Replace);
        world.insert_resource(selection);
        assert!(!hides_gizmo(&world));
        assert_eq!(pilot_selection(&mut world), Ok(light));
        assert!(hides_gizmo(&world));
        eject(&mut world);
        assert!(!hides_gizmo(&world));
    }
}
