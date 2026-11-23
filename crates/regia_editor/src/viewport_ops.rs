// SPDX-License-Identifier: MPL-2.0
//! Viewport tools and menu actions of `docs/specs/19-editor.md` §§ 2–3 (ADR-7204):
//!
//! - the CPU picking fallback, a ray against each drawn mesh (its cached `regia_pick::MeshBvh` for
//!   rigid meshes, the triangles otherwise) and the terrain, instead of translation spheres
//!   ([`cpu_pick`]);
//! - view presets on the number keys and eased camera moves of a fixed duration ([`ViewPreset`],
//!   [`CameraEase`], [`CameraEases`]);
//! - the gizmo pivot mode ([`PivotMode`]) and the snap toggle that Ctrl inverts while held
//!   ([`effective_snap`]);
//! - gizmo edits of a whole selection as one undo step, Alt+drag duplicating first
//!   ([`gizmo_drag`], [`GizmoEdit`]);
//! - the File and Edit menu actions ([`invoke_menu`], [`MENU_PATHS`]) with an entity clipboard
//!   ([`EditorClipboard`]);
//! - the Outliner's visibility and lock toggles ([`set_visible`], [`set_locked`], [`EditorLocked`]).
//!
//! The agent surface is [`crate::viewport_ops_agent`] (`book/agents/viewport-tools.md`).

use std::collections::{HashMap, HashSet};

use regia_core::id::Guid;
use regia_core::math::{Affine3A, Quat, Vec3};
use regia_ecs::prelude::*;
use regia_scene::prelude::{EntityGuid, Transform, Visibility};

use crate::command::{EditorCommand, SpawnEntities, UndoStack, resolve_entity};
use crate::hierarchy_ops::{self, HierarchyError};
use crate::selection::{SelectMode, Selection};
use crate::viewport::EditorCamera;

// ---------------------------------------------------------------------------------------------
// Lock

/// An Outliner lock: the entity (and its mesh parts) cannot be picked or box-selected in the
/// viewport. Editor-session state, not saved with the level (no reflection registration).
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EditorLocked;

/// Whether `entity`, its pick owner (the model a part entity draws for) or an ancestor is locked,
/// by an Outliner lock or a locked layer it belongs to (ADR-7792).
#[must_use]
pub fn is_locked(world: &World, entity: Entity) -> bool {
    let owner = crate::mesh_source::pick_owner(world, entity);
    [entity, owner]
        .into_iter()
        .chain(hierarchy_ops::ancestors(world, owner))
        .any(|e| world.get::<EditorLocked>(e).is_some() || crate::layers_panel::layer_locked(world, e))
}

/// Whether a click may pick `entity` (a mesh entity or its owner): it draws in the viewport (no
/// outliner eye, layer parking or `Visibility::Hidden` on it or its owner), it is not an
/// editor-internal entity, and it is not locked.
#[must_use]
pub fn pickable(world: &World, entity: Entity) -> bool {
    let owner = crate::mesh_source::pick_owner(world, entity);
    crate::picking::draws_in_viewport(world, entity)
        && !crate::visibility::is_hidden(world, owner)
        && !is_locked(world, entity)
        // Hidden in the editor only (H, isolate; ADR-7770).
        && !crate::view_state::is_view_hidden(world, entity)
}

// ---------------------------------------------------------------------------------------------
// CPU picking

/// The CPU picking fallback (`specs/19` § 3, used when no id buffer is available): the nearest
/// surface along the ray over every pickable mesh entity and the terrain. Each mesh is cast with
/// [`regia_pick::raycast_mesh_entity`]: a box early-out, then the cached mesh BVH for rigid meshes
/// with enough triangles, the triangles themselves otherwise (skinned or morphed meshes in their
/// drawn pose). Returns what a click selects (the mesh's owner, or the terrain entity) and the
/// world point; `None` on background. `far` bounds the terrain cast. Greybox primitives and
/// mesh sources are resolved to their drawn meshes first (the edit world runs no render plugin).
pub fn cpu_pick(world: &mut World, origin: Vec3, direction: Vec3, far: f32) -> Option<(Entity, Vec3)> {
    let _span = tracing::info_span!(target: "regia_editor", "cpu_pick").entered();
    if direction.length_squared() < 1e-12 || !direction.is_finite() || !origin.is_finite() {
        return None;
    }
    crate::mesh_source::resolve_mesh_sources(world);
    let world: &World = world;
    let direction = direction.normalize();
    let mesh =
        crate::picking::raycast_world(world, origin, direction, crate::picking::FaceCull::None, &|e| {
            pickable(world, e)
        });
    let limit = mesh.as_ref().map_or(far, |hit| hit.distance);
    let terrain = crate::picking::pick_terrain(world, origin, direction, limit)
        .filter(|(entity, _)| !is_locked(world, *entity) && !crate::visibility::is_hidden(world, *entity));
    match (mesh, terrain) {
        (_, Some((entity, t))) => Some((entity, origin + direction * t)),
        (Some(hit), None) => Some((hit.owner, hit.position)),
        (None, None) => None,
    }
}

// ---------------------------------------------------------------------------------------------
// View presets and eased camera moves

/// How long an eased camera move takes (focus, view preset), seconds.
pub const EASE_SECONDS: f32 = 0.25;

/// The perspective camera's pitch for the top preset: 89° (straight down would make the view
/// matrix's up vector parallel to the view direction).
pub const TOP_PITCH: f32 = 89.0 * std::f32::consts::PI / 180.0;

/// A numeric view preset (`specs/19` § 3): bare `1` perspective, `2` top, `3` front, `4` side in
/// the active pane. The target, distance, speed and field of view are kept; yaw and pitch change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewPreset {
    /// The editor's start angle (yaw 0.6, pitch 0.45).
    Perspective,
    /// Looking down (pitch [`TOP_PITCH`], yaw 0).
    Top,
    /// Looking along −Z (yaw 0, pitch 0).
    Front,
    /// Looking along −X (yaw π/2, pitch 0).
    Side,
    /// Looking up (pitch −[`TOP_PITCH`], yaw 0; ADR-7755, Alt+2).
    Bottom,
    /// Looking along +Z (yaw π, pitch 0; Alt+3).
    Back,
    /// Looking along +X (yaw −π/2, pitch 0; Alt+4).
    OppositeSide,
}

impl ViewPreset {
    /// The presets of the bare number keys, in key order (`1`..`4`).
    pub const ALL: [Self; 4] = [Self::Perspective, Self::Top, Self::Front, Self::Side];

    /// Every preset: the perspective view and the six axis views (ADR-7755).
    pub const EVERY: [Self; 7] = [
        Self::Perspective,
        Self::Top,
        Self::Front,
        Self::Side,
        Self::Bottom,
        Self::Back,
        Self::OppositeSide,
    ];

    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Perspective => "perspective",
            Self::Top => "top",
            Self::Front => "front",
            Self::Side => "side",
            Self::Bottom => "bottom",
            Self::Back => "back",
            Self::OppositeSide => "opposite_side",
        }
    }

    /// Parses a protocol name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::EVERY.into_iter().find(|p| p.name() == name)
    }

    /// The orthographic pane kind looking the same way (`None` for the perspective preset).
    #[must_use]
    pub fn ortho_kind(self) -> Option<crate::views::ViewKind> {
        use crate::views::ViewKind;
        match self {
            Self::Perspective => None,
            Self::Top => Some(ViewKind::Top),
            Self::Front => Some(ViewKind::Front),
            Self::Side => Some(ViewKind::Side),
            Self::Bottom => Some(ViewKind::Bottom),
            Self::Back => Some(ViewKind::Back),
            Self::OppositeSide => Some(ViewKind::OppositeSide),
        }
    }

    /// The preset of number key `digit` (1..=4).
    #[must_use]
    pub fn from_digit(digit: usize) -> Option<Self> {
        digit.checked_sub(1).and_then(|i| Self::ALL.get(i).copied())
    }

    /// `(yaw, pitch)` of the preset, radians.
    #[must_use]
    pub fn angles(self) -> (f32, f32) {
        match self {
            Self::Perspective => {
                let home = EditorCamera::default();
                (home.yaw, home.pitch)
            }
            Self::Top => (0.0, TOP_PITCH),
            Self::Front => (0.0, 0.0),
            Self::Side => (std::f32::consts::FRAC_PI_2, 0.0),
            Self::Bottom => (0.0, -TOP_PITCH),
            Self::Back => (std::f32::consts::PI, 0.0),
            Self::OppositeSide => (-std::f32::consts::FRAC_PI_2, 0.0),
        }
    }

    /// `camera` turned to the preset (target and distance kept).
    #[must_use]
    pub fn pose(self, camera: &EditorCamera) -> EditorCamera {
        let (yaw, pitch) = self.angles();
        EditorCamera {
            yaw,
            pitch,
            ..camera.clone()
        }
    }
}

/// An eased move between two camera poses over a fixed duration (smoothstep in time; yaw takes
/// the shorter way round). [`Self::advance`] lands exactly on the end pose once the duration has
/// passed.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraEase {
    /// The pose when the move started.
    pub from: EditorCamera,
    /// The pose it ends on.
    pub to: EditorCamera,
    /// Seconds advanced so far.
    pub elapsed: f32,
    /// Total seconds ([`EASE_SECONDS`] for the editor's moves).
    pub duration: f32,
}

impl CameraEase {
    /// A move from `from` to `to` over [`EASE_SECONDS`].
    #[must_use]
    pub fn new(from: EditorCamera, to: EditorCamera) -> Self {
        Self {
            from,
            to,
            elapsed: 0.0,
            duration: EASE_SECONDS,
        }
    }

    /// Whether the move has reached its end.
    #[must_use]
    pub fn done(&self) -> bool {
        self.elapsed >= self.duration
    }

    /// The pose at the current time.
    #[must_use]
    pub fn sample(&self) -> EditorCamera {
        if self.done() || self.duration <= 0.0 {
            return self.to.clone();
        }
        let t = (self.elapsed / self.duration).clamp(0.0, 1.0);
        let s = t * t * (3.0 - 2.0 * t);
        let lerp = |a: f32, b: f32| a + (b - a) * s;
        let mut dyaw = (self.to.yaw - self.from.yaw).rem_euclid(std::f32::consts::TAU);
        if dyaw > std::f32::consts::PI {
            dyaw -= std::f32::consts::TAU;
        }
        EditorCamera {
            target: self.from.target.lerp(self.to.target, s),
            distance: lerp(self.from.distance, self.to.distance),
            yaw: self.from.yaw + dyaw * s,
            pitch: lerp(self.from.pitch, self.to.pitch),
            speed: self.to.speed,
            fov_deg: lerp(self.from.fov_deg, self.to.fov_deg),
            near: self.to.near,
            far: self.to.far,
        }
    }

    /// Advances by `dt` seconds (negative and non-finite steps count as zero) and returns the
    /// pose.
    pub fn advance(&mut self, dt: f32) -> EditorCamera {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        self.elapsed = (self.elapsed + dt).min(self.duration.max(0.0));
        self.sample()
    }
}

/// The key of the world's own perspective camera ([`EditorCamera`]) in [`CameraEases`]: the main
/// viewport's perspective pane and the agent calls share it.
pub const MAIN_CAMERA: (u64, usize) = (0, usize::MAX);

/// Running eased moves, keyed by `(viewport instance id, pane index)` ([`MAIN_CAMERA`] for the
/// world's perspective camera). The viewport panel advances its panes' moves by the frame time;
/// `editor.camera_ease` advances [`MAIN_CAMERA`] by the seconds it is given.
#[derive(Resource, Debug, Clone, Default)]
pub struct CameraEases {
    /// The moves.
    pub moves: HashMap<(u64, usize), CameraEase>,
}

impl CameraEases {
    /// Starts (or restarts from `from`) a move for `key`.
    pub fn start(&mut self, key: (u64, usize), from: EditorCamera, to: EditorCamera) {
        self.moves.insert(key, CameraEase::new(from, to));
    }

    /// Advances `key`'s move; returns its pose, `None` when none runs. A finished move is removed.
    pub fn advance(&mut self, key: (u64, usize), dt: f32) -> Option<EditorCamera> {
        let ease = self.moves.get_mut(&key)?;
        let pose = ease.advance(dt);
        if ease.done() {
            self.moves.remove(&key);
        }
        Some(pose)
    }
}

/// The camera [`crate::viewport::focus_camera`] would frame `entities` with, without moving
/// `camera`.
#[must_use]
pub fn focus_pose(world: &World, camera: &EditorCamera, entities: &[Entity]) -> EditorCamera {
    let mut to = camera.clone();
    crate::viewport::focus_camera(world, &mut to, entities);
    to
}

// ---------------------------------------------------------------------------------------------
// Pivot, snap and gizmo edits

/// Where a gizmo rotates and scales a multi-selection about (`specs/19` § 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PivotMode {
    /// The mean of the selected entities' translations.
    #[default]
    Median,
    /// Each entity about its own origin.
    Individual,
    /// The active (primary) entity's origin.
    Active,
    /// The center of the world box around the selection's meshes (its translations' box when no
    /// selected entity draws a mesh; ADR-7756).
    Bounds,
}

impl PivotMode {
    /// Every mode.
    pub const ALL: [Self; 4] = [Self::Median, Self::Individual, Self::Active, Self::Bounds];

    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Median => "median",
            Self::Individual => "individual",
            Self::Active => "active",
            Self::Bounds => "bounds",
        }
    }

    /// Parses a protocol name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.name() == name)
    }

    /// The toolbar label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Median => "Median point",
            Self::Individual => "Individual origins",
            Self::Active => "Active element",
            Self::Bounds => "Bounds center",
        }
    }
}

/// The gizmo's pivot mode, a world resource shared by every viewport.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GizmoPivot(pub PivotMode);

/// Whether a gizmo drag snaps: the toggle, inverted while Ctrl is held.
#[must_use]
pub fn effective_snap(toggle: bool, ctrl_held: bool) -> bool {
    toggle != ctrl_held
}

/// One gizmo step: a translation, then a rotation and a per-axis scale about the pivot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GizmoDelta {
    /// Added to each translation (parent space).
    pub translation: Vec3,
    /// Applied about the pivot.
    pub rotation: Quat,
    /// Multiplied into each scale (and the offset from the pivot).
    pub scale: Vec3,
}

impl Default for GizmoDelta {
    fn default() -> Self {
        Self {
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        }
    }
}

impl GizmoDelta {
    /// The step that takes the gizmo's `before` transform to `after`. A zero scale component in
    /// `before` gives a factor of 1 on that axis.
    #[must_use]
    pub fn between(before: &Transform, after: &Transform) -> Self {
        let ratio = |a: f32, b: f32| if a.abs() > 1e-12 { b / a } else { 1.0 };
        Self {
            translation: after.translation - before.translation,
            rotation: (after.rotation * before.rotation.inverse()).normalize(),
            scale: Vec3::new(
                ratio(before.scale.x, after.scale.x),
                ratio(before.scale.y, after.scale.y),
                ratio(before.scale.z, after.scale.z),
            ),
        }
    }
}

/// The pivot of `items` for `mode`: the mean translation, the active entity's translation (the
/// last item when `active` is not among them), or `None` for individual origins.
#[must_use]
pub fn pivot_point(items: &[(Entity, Transform)], active: Option<Entity>, mode: PivotMode) -> Option<Vec3> {
    if items.is_empty() {
        return None;
    }
    match mode {
        PivotMode::Median => {
            Some(items.iter().map(|(_, t)| t.translation).sum::<Vec3>() / items.len() as f32)
        }
        // Without the world this is the translations' box; `selection_pivot` measures meshes.
        PivotMode::Bounds => {
            let (lo, hi) = items.iter().fold(
                (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
                |(lo, hi), (_, t)| (lo.min(t.translation), hi.max(t.translation)),
            );
            Some((lo + hi) * 0.5)
        }
        PivotMode::Individual => None,
        PivotMode::Active => items
            .iter()
            .find(|(e, _)| Some(*e) == active)
            .or_else(|| items.last())
            .map(|(_, t)| t.translation),
    }
}

/// `transform` after `delta` about `pivot` (`None`: about its own origin).
#[must_use]
pub fn apply_delta(transform: &Transform, pivot: Option<Vec3>, delta: &GizmoDelta) -> Transform {
    let translation = match pivot {
        Some(p) => p + delta.rotation * ((transform.translation - p) * delta.scale) + delta.translation,
        None => transform.translation + delta.translation,
    };
    Transform {
        translation,
        rotation: (delta.rotation * transform.rotation).normalize(),
        scale: transform.scale * delta.scale,
    }
}

/// The transform the gizmo is drawn with for a selection: the pivot's position (the active
/// entity's for individual origins) with the active entity's rotation and scale.
#[must_use]
pub fn gizmo_frame(
    items: &[(Entity, Transform)],
    active: Option<Entity>,
    mode: PivotMode,
) -> Option<Transform> {
    let primary = items
        .iter()
        .find(|(e, _)| Some(*e) == active)
        .or_else(|| items.last())?
        .1;
    let at = pivot_point(items, active, mode).unwrap_or(primary.translation);
    Some(Transform {
        translation: at,
        ..primary
    })
}

// ---------------------------------------------------------------------------------------------
// Bounds pivot, temporary pivot offset, reference space, snap steps, fixed-step rotate (ADR-7756)

/// The world affine of `entity`'s parent (identity for a root): the space its `Transform` and a
/// gizmo delta of it live in.
#[must_use]
pub fn parent_affine(world: &World, entity: Entity) -> Affine3A {
    world.get::<ChildOf>(entity).map_or(Affine3A::IDENTITY, |c| {
        hierarchy_ops::world_affine(world, c.parent())
    })
}

/// The world pose of `local` under the parent `parent` (rotation and scale composed as the
/// parent's decomposed scale, rotation and translation).
#[must_use]
pub fn to_world(parent: Affine3A, local: &Transform) -> Transform {
    let (scale, rotation, _) = parent.to_scale_rotation_translation();
    Transform {
        translation: parent.transform_point3(local.translation),
        rotation: (rotation * local.rotation).normalize(),
        scale: scale * local.scale,
    }
}

/// The local pose under `parent` of the world pose `pose` (the inverse of [`to_world`]). `None`
/// when the parent has a zero scale axis or the result is not finite.
#[must_use]
pub fn to_parent(parent: Affine3A, pose: &Transform) -> Option<Transform> {
    if parent.matrix3.determinant().abs() <= 1e-12 {
        return None;
    }
    let (scale, rotation, _) = parent.to_scale_rotation_translation();
    let ratio = |a: f32, s: f32| if s.abs() > 1e-12 { a / s } else { a };
    let local = Transform {
        translation: parent.inverse().transform_point3(pose.translation),
        rotation: (rotation.inverse() * pose.rotation).normalize(),
        scale: Vec3::new(
            ratio(pose.scale.x, scale.x),
            ratio(pose.scale.y, scale.y),
            ratio(pose.scale.z, scale.z),
        ),
    };
    (local.translation.is_finite() && local.rotation.is_finite() && local.scale.is_finite()).then_some(local)
}

/// A temporary pivot offset: the world vector the user dragged the pivot by with the pivot key
/// held (`gizmo.pivot_offset`, or `editor.set_pivot_mode { offset }`). It belongs to the top-level
/// selection it was made for and lapses when that changes; individual origins ignore it.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct PivotOffset {
    /// World-space offset added to the pivot, meters.
    pub offset: Vec3,
    /// The sorted entity bits of the top-level selection it belongs to.
    pub selection: Vec<u64>,
}

fn selection_key(world: &World, entities: &[Entity]) -> Vec<u64> {
    let mut bits: Vec<u64> = hierarchy_ops::top_level(world, entities)
        .into_iter()
        .map(Entity::to_bits)
        .collect();
    bits.sort_unstable();
    bits
}

/// The pivot offset that applies to `entities` (zero when none was made for exactly them).
#[must_use]
pub fn pivot_offset_for(world: &World, entities: &[Entity]) -> Vec3 {
    world
        .get_resource::<PivotOffset>()
        .filter(|o| o.selection == selection_key(world, entities))
        .map_or(Vec3::ZERO, |o| o.offset)
}

/// Sets the temporary pivot offset of `entities` (world meters); a zero offset clears it.
pub fn set_pivot_offset(world: &mut World, entities: &[Entity], offset: Vec3) {
    if offset == Vec3::ZERO || !offset.is_finite() {
        world.remove_resource::<PivotOffset>();
        return;
    }
    let selection = selection_key(world, entities);
    world.insert_resource(PivotOffset { offset, selection });
}

/// Drops a pivot offset made for another selection than `entities` (the viewport calls it when the
/// selection changes).
pub fn lapse_pivot_offset(world: &mut World, entities: &[Entity]) {
    let stale = world
        .get_resource::<PivotOffset>()
        .is_some_and(|o| o.selection != selection_key(world, entities));
    if stale {
        world.remove_resource::<PivotOffset>();
    }
}

/// The world box around the meshes of the subtrees of `entities` (their world translations' box
/// when none draws a mesh). `None` for no entity.
///
/// A member with cached model bounds (a resolved mesh source or primitive,
/// `mesh_source::model_bounds`) counts as the 8 corners of that box in world space, the box the
/// selection outline draws (skinned parts in bind pose, as there); only meshes with no cached bounds
/// are measured vertex by vertex. The gizmo asks for this on every frame, per pane and per egui pass,
/// so a level or prefab root must not walk every vertex of the level (review finding 13 of
/// ADR-7756).
#[must_use]
pub fn selection_bounds(world: &World, entities: &[Entity]) -> Option<(Vec3, Vec3)> {
    let _span = tracing::trace_span!(target: "regia_editor", "selection_bounds").entered();
    let mut lo = Vec3::splat(f32::MAX);
    let mut hi = Vec3::splat(f32::MIN);
    let mut any = false;
    for root in entities {
        for member in crate::outliner::subtree_of(world, *root) {
            if crate::mesh_source::model_bounds(world, member).is_some()
                && let Some(corners) = crate::overlays::world_bounds_corners(world, member)
            {
                for p in corners {
                    if p.is_finite() {
                        lo = lo.min(p);
                        hi = hi.max(p);
                        any = true;
                    }
                }
                continue;
            }
            for mesh in crate::picking::mesh_entities_of(world, member) {
                let Some(renderer) = world.get::<regia_render::MeshRenderer>(mesh) else {
                    continue;
                };
                let model =
                    crate::mesh_source::world_matrix(world, mesh).unwrap_or(regia_core::math::Mat4::IDENTITY);
                let palette = world
                    .get::<regia_render::SkinJoints>(mesh)
                    .map_or(&[][..], |s| s.palette.as_slice());
                for vertex in &renderer.mesh.vertices {
                    let p = crate::handles::vertex_world_position(vertex, model, palette);
                    if p.is_finite() {
                        lo = lo.min(p);
                        hi = hi.max(p);
                        any = true;
                    }
                }
            }
        }
    }
    if !any {
        for root in entities {
            let p: Vec3 = hierarchy_ops::world_affine(world, *root).translation.into();
            lo = lo.min(p);
            hi = hi.max(p);
            any = true;
        }
    }
    any.then_some((lo, hi))
}

/// The pivot of `items` (local transforms, top-level) for `mode` in the space of the active
/// entity's parent, with the bounds pivot measured on the meshes and the temporary pivot offset of
/// `selection` added. `None` for individual origins.
#[must_use]
pub fn selection_pivot(
    world: &World,
    items: &[(Entity, Transform)],
    active: Option<Entity>,
    mode: PivotMode,
    selection: &[Entity],
) -> Option<Vec3> {
    let anchor = items
        .iter()
        .find(|(e, _)| Some(*e) == active)
        .or_else(|| items.last())?
        .0;
    let parent = parent_affine(world, anchor);
    let pivot = match mode {
        PivotMode::Individual => return None,
        PivotMode::Bounds => {
            let entities: Vec<Entity> = items.iter().map(|(e, _)| *e).collect();
            let (lo, hi) = selection_bounds(world, &entities)?;
            let center = (lo + hi) * 0.5;
            Some(to_parent(parent, &Transform::from_translation(center)).map_or(center, |t| t.translation))
        }
        _ => pivot_point(items, active, mode),
    }?;
    let offset = pivot_offset_for(world, selection);
    if offset == Vec3::ZERO {
        return Some(pivot);
    }
    let local = parent.inverse().transform_vector3(offset);
    Some(if local.is_finite() { pivot + local } else { pivot })
}

/// [`gizmo_frame`] with the world: the bounds pivot measured on the meshes and the temporary pivot
/// offset of `selection` added.
#[must_use]
pub fn gizmo_frame_in(
    world: &World,
    items: &[(Entity, Transform)],
    active: Option<Entity>,
    mode: PivotMode,
    selection: &[Entity],
) -> Option<Transform> {
    let primary = items
        .iter()
        .find(|(e, _)| Some(*e) == active)
        .or_else(|| items.last())?
        .1;
    let at = selection_pivot(world, items, active, mode, selection).unwrap_or(primary.translation);
    Some(Transform {
        translation: at,
        ..primary
    })
}

/// The rotation whose axes the gizmo uses in `space` (ADR-7756): identity for world, the active
/// entity's world rotation for local, its parent's world rotation for parent and the camera's
/// (right, up, toward the eye) for view.
#[must_use]
pub fn space_basis(
    space: crate::viewport::GizmoSpace,
    entity_rotation: Quat,
    parent_rotation: Quat,
    view_rotation: Quat,
) -> Quat {
    use crate::viewport::GizmoSpace;
    match space {
        GizmoSpace::World => Quat::IDENTITY,
        GizmoSpace::Local => entity_rotation.normalize(),
        GizmoSpace::Parent => parent_rotation.normalize(),
        GizmoSpace::View => view_rotation.normalize(),
    }
}

/// The camera's rotation: its columns are the view's right, up and backward (toward the eye) axes.
#[must_use]
pub fn view_rotation(view: &crate::views::ViewCamera) -> Quat {
    let (_, rotation, _) = view.view().inverse().to_scale_rotation_translation();
    if rotation.is_finite() {
        rotation.normalize()
    } else {
        Quat::IDENTITY
    }
}

/// A step given in `basis` (translation along its axes, rotation about them) as a gizmo delta in
/// the space of the parent `parent`. The per-axis scale stays along each entity's own axes.
#[must_use]
pub fn delta_in_parent(
    basis: Quat,
    parent: Affine3A,
    translation: Vec3,
    rotation: Quat,
    scale: Vec3,
) -> GizmoDelta {
    let (_, parent_rotation, _) = parent.to_scale_rotation_translation();
    let world_translation = basis * translation;
    let local_translation = if parent.matrix3.determinant().abs() > 1e-12 {
        parent.inverse().transform_vector3(world_translation)
    } else {
        world_translation
    };
    let world_rotation = basis * rotation * basis.inverse();
    GizmoDelta {
        translation: local_translation,
        rotation: (parent_rotation.inverse() * world_rotation * parent_rotation).normalize(),
        scale,
    }
}

/// The grid steps of the snap menu, meters: powers of two and decimal steps.
pub const GRID_STEP_PRESETS: &[f32] = &[
    0.01, 0.0625, 0.1, 0.125, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 10.0, 16.0, 100.0,
];
/// The angle steps of the snap menu, degrees.
pub const ANGLE_STEP_PRESETS: &[f32] = &[5.0, 10.0, 15.0, 30.0, 45.0, 90.0];
/// The scale steps of the snap menu.
pub const SCALE_STEP_PRESETS: &[f32] = &[0.01, 0.1, 0.25, 0.5, 1.0];

/// The project's snap steps file: per-user, per-project view state (the `viewport-snap` format, ADR-7902).
pub const SNAP_STEPS_FILE: &str = ".regia/viewport_snap.ron";
/// Version of [`SNAP_STEPS_FILE`].
pub const SNAP_STEPS_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct SnapStepsFile {
    #[serde(default = "crate::editor_prefs::version_missing")]
    format_version: u32,
    distance: f32,
    angle_deg: f32,
    scale: f32,
}

/// The `from` versions of the registered `.regia/viewport_snap.ron` migration steps (none: version
/// 1 is the first).
#[must_use]
pub fn snap_steps_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape of `.regia/viewport_snap.ron` (ADR-7902: the `viewport-snap` format).
#[must_use]
pub fn snap_steps_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<SnapStepsFile>()])
}

/// Parses `.regia/viewport_snap.ron` text: the `(distance, angle_deg, scale)` steps as written
/// (the caller keeps a step that is not a finite number above zero at its current value).
///
/// # Errors
///
/// Text that is not this shape, or a file written by a newer editor (the message names the version).
pub fn parse_snap_steps(text: &str) -> Result<(f32, f32, f32), String> {
    let file = crate::editor_prefs::read_versioned(text, SNAP_STEPS_FORMAT_VERSION, |f: &SnapStepsFile| {
        f.format_version
    })
    .map_err(|e| match e {
        crate::editor_prefs::VersionedError::Parse(m) => m,
        crate::editor_prefs::VersionedError::Newer(found) => format!(
            "written by a newer editor (format_version {found}, this editor reads up to {SNAP_STEPS_FORMAT_VERSION}); left untouched"
        ),
    })?;
    Ok((file.distance, file.angle_deg, file.scale))
}

/// `.regia/viewport_snap.ron`'s text (current version) for the three steps.
#[must_use]
pub fn snap_steps_text(distance: f32, angle_deg: f32, scale: f32) -> String {
    let file = SnapStepsFile {
        format_version: SNAP_STEPS_FORMAT_VERSION,
        distance,
        angle_deg,
        scale,
    };
    ron::ser::to_string_pretty(&file, regia_core::ron_pretty().struct_names(false)).unwrap_or_default()
}

/// The project root whose snap steps the gizmo state holds (loaded once per project).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct SnapStepsLoaded(pub Option<std::path::PathBuf>);

/// Why a snap step change was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapStepError {
    /// A step is not a finite number above zero (the parameter name).
    Invalid(&'static str),
    /// Writing the project's snap steps file failed.
    Io(String),
}

impl SnapStepError {
    /// The agent error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "snap_step_invalid",
            Self::Io(_) => "snap_steps_io",
        }
    }
}

impl std::fmt::Display for SnapStepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(name) => write!(f, "`{name}` must be a number > 0"),
            Self::Io(err) => write!(f, "saving the snap steps: {err}"),
        }
    }
}

impl std::error::Error for SnapStepError {}

/// Reads the open project's snap steps into the gizmo state once per project (a missing or
/// unreadable file keeps the current steps). Cheap when already loaded: the toolbar calls it each
/// frame.
pub fn ensure_snap_steps(world: &mut World) {
    let root = crate::mesh_source::project_root(world);
    let previous = world.get_resource::<SnapStepsLoaded>().map(|l| l.0.clone());
    if previous.as_ref() == Some(&root) {
        return;
    }
    world.insert_resource(SnapStepsLoaded(root.clone()));
    if matches!(previous, Some(Some(_))) {
        // Another project was open: its steps are not this one's. Start from the defaults, so a
        // project with no saved steps does not inherit them (and save them on the first change).
        let defaults = crate::viewport::GizmoState::default();
        let mut gizmo = world.get_resource_or_insert_with(crate::viewport::GizmoState::default);
        gizmo.snap_distance = defaults.snap_distance;
        gizmo.snap_angle_deg = defaults.snap_angle_deg;
        gizmo.snap_scale = defaults.snap_scale;
    }
    let Some(root) = root else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(root.join(SNAP_STEPS_FILE)) else {
        return;
    };
    let (distance, angle_deg, scale) = match parse_snap_steps(&text) {
        Ok(steps) => steps,
        Err(err) => {
            // A newer editor's file or a malformed one: keep the current steps.
            tracing::warn!(target: "regia_editor", "snap steps: {err}");
            return;
        }
    };
    let valid = |v: f32| v.is_finite() && v > 0.0;
    let mut gizmo = world.get_resource_or_insert_with(crate::viewport::GizmoState::default);
    if valid(distance) {
        gizmo.snap_distance = distance;
    }
    if valid(angle_deg) {
        gizmo.snap_angle_deg = angle_deg;
    }
    if valid(scale) {
        gizmo.snap_scale = scale;
    }
}

/// Sets the gizmo's snap steps (any of them) and saves all three to the open project's
/// [`SNAP_STEPS_FILE`]: what the toolbar's step menus and `editor.set_snap` both call. Returns the
/// file written, if a project is open.
///
/// # Errors
/// A step that is not a finite number above zero (nothing changes), or the file write failing (the
/// steps are set anyway).
pub fn set_snap_steps(
    world: &mut World,
    distance: Option<f32>,
    angle_deg: Option<f32>,
    scale: Option<f32>,
) -> Result<Option<std::path::PathBuf>, SnapStepError> {
    for (name, value) in [("distance", distance), ("angle_deg", angle_deg), ("scale", scale)] {
        if value.is_some_and(|v| !v.is_finite() || v <= 0.0) {
            return Err(SnapStepError::Invalid(name));
        }
    }
    ensure_snap_steps(world);
    let file = {
        let mut gizmo = world.get_resource_or_insert_with(crate::viewport::GizmoState::default);
        if let Some(d) = distance {
            gizmo.snap_distance = d;
        }
        if let Some(a) = angle_deg {
            gizmo.snap_angle_deg = a;
        }
        if let Some(s) = scale {
            gizmo.snap_scale = s;
        }
        snap_steps_text(gizmo.snap_distance, gizmo.snap_angle_deg, gizmo.snap_scale)
    };
    let Some(root) = crate::mesh_source::project_root(world) else {
        return Ok(None);
    };
    let path = root.join(SNAP_STEPS_FILE);
    let text = file;
    write_text_atomic(&path, &text).map_err(|e| SnapStepError::Io(e.to_string()))?;
    Ok(Some(path))
}

/// Writes `text` to `path` through a temporary file and a rename, creating the directory.
fn write_text_atomic(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("ron.tmp{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// The fixed-step rotate (`gizmo.rotate_step`, `gizmo.rotate_step_back`): turns the selection by
/// the gizmo's angle step (`sign` +1 or -1) about the world up axis through the pivot, as one undo
/// step.
///
/// # Errors
/// When nothing selected has a transform.
pub fn rotate_step(world: &mut World, sign: f32) -> Result<GizmoOutcome, HierarchyError> {
    // The project's saved angle step, also before the toolbar first draws (headless, agents).
    ensure_snap_steps(world);
    let selected = selected_entities(world);
    let degrees = world
        .get_resource::<crate::viewport::GizmoState>()
        .map_or(15.0, |g| g.snap_angle_deg);
    let primary = world
        .get_resource::<Selection>()
        .and_then(Selection::primary)
        .or_else(|| selected.last().copied())
        .ok_or(HierarchyError::NoEntities)?;
    let parent = parent_affine(world, primary);
    let turn = Quat::from_rotation_y((sign.signum() * degrees).to_radians());
    let delta = delta_in_parent(Quat::IDENTITY, parent, Vec3::ZERO, turn, Vec3::ONE);
    seal_undo(world);
    gizmo_drag(world, &selected, Some(primary), &delta, false, true)
}

/// A gizmo edit of several entities as one undo entry, optionally after the duplicate an Alt+drag
/// made (applied first, reverted last). Continuous drags coalesce: a later `GizmoEdit` without a
/// duplicate over the same entities merges into this one until the stack is sealed.
pub struct GizmoEdit {
    /// What the entry does, for the Edit menu.
    pub label: String,
    /// The Alt+drag duplicate (a [`hierarchy_ops::CommandBatch`]).
    pub duplicate: Option<Box<dyn EditorCommand>>,
    /// `(entity bits, before, after)` per moved entity.
    pub entries: Vec<(u64, Transform, Transform)>,
}

impl GizmoEdit {
    fn write(world: &mut World, bits: u64, value: Transform) {
        if let Some(entity) = resolve_entity(world, bits) {
            world.entity_mut(entity).insert(value);
        }
    }
}

impl EditorCommand for GizmoEdit {
    fn apply(&mut self, world: &mut World) {
        if let Some(duplicate) = &mut self.duplicate {
            duplicate.apply(world);
        }
        for (bits, _, new) in &self.entries {
            Self::write(world, *bits, *new);
        }
    }

    fn revert(&mut self, world: &mut World) {
        for (bits, old, _) in self.entries.iter().rev() {
            Self::write(world, *bits, *old);
        }
        if let Some(duplicate) = &mut self.duplicate {
            duplicate.revert(world);
        }
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = crate::command::downcast_to::<GizmoEdit>(next) else {
            return false;
        };
        let same = next.duplicate.is_none()
            && next.entries.len() == self.entries.len()
            && next.entries.iter().zip(&self.entries).all(|(a, b)| a.0 == b.0);
        if same {
            for (mine, theirs) in self.entries.iter_mut().zip(&next.entries) {
                mine.2 = theirs.2;
            }
        }
        same
    }

    fn description(&self) -> String {
        self.label.clone()
    }
}

/// What [`gizmo_drag`] did.
#[derive(Debug, Clone, Default)]
pub struct GizmoOutcome {
    /// The entities moved (the copies after a duplicate), top-level only.
    pub moved: Vec<Entity>,
    /// The copies an Alt+drag made (empty without one).
    pub copies: Vec<Entity>,
    /// The pivot used (`None` for individual origins).
    pub pivot: Option<Vec3>,
}

/// The top-level members of `entities` with their local transforms, in input order: what a gizmo
/// edit moves.
#[must_use]
pub fn selection_items(world: &World, entities: &[Entity]) -> Vec<(Entity, Transform)> {
    top_transforms(world, entities)
}

/// The top-level members of `entities` with their transforms, in input order.
fn top_transforms(world: &World, entities: &[Entity]) -> Vec<(Entity, Transform)> {
    hierarchy_ops::top_level(world, entities)
        .into_iter()
        .filter_map(|e| world.get::<Transform>(e).map(|t| (e, *t)))
        .collect()
}

/// Applies `delta` to the top-level members of `entities` about the [`GizmoPivot`] mode's pivot
/// (`active` names the active entity, default the selection's primary), as one [`GizmoEdit`] undo
/// entry. With `duplicate` (Alt+drag) the entities are duplicated in place first and the copies
/// move; the originals stay. `seal` ends coalescing after the push (a discrete step: the agent
/// call); the viewport leaves it open while the pointer drags.
///
/// # Errors
/// When the duplicate fails ([`hierarchy_ops::duplicate_entities`]) or no entity has a transform.
pub fn gizmo_drag(
    world: &mut World,
    entities: &[Entity],
    active: Option<Entity>,
    delta: &GizmoDelta,
    duplicate: bool,
    seal: bool,
) -> Result<GizmoOutcome, HierarchyError> {
    let originals = top_transforms(world, entities);
    if originals.is_empty() {
        return Err(HierarchyError::NoEntities);
    }
    // The temporary pivot offset belongs to the selection the user sees: the originals (an
    // Alt+drag's copies sit where they are).
    let offset_for: Vec<Entity> = originals.iter().map(|(e, _)| *e).collect();
    let active = active
        .or_else(|| world.get_resource::<Selection>().and_then(Selection::primary))
        .filter(|a| originals.iter().any(|(e, _)| e == a));
    let mut batch = None;
    let mut copies = Vec::new();
    let (items, active) = if duplicate {
        let roots: Vec<Entity> = originals.iter().map(|(e, _)| *e).collect();
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .seal();
        copies = hierarchy_ops::duplicate_entities(world, &roots, Vec3::ZERO)?;
        batch = world
            .get_resource_mut::<UndoStack>()
            .and_then(|mut stack| stack.take_top());
        let active_copy = active
            .and_then(|a| roots.iter().position(|r| *r == a))
            .and_then(|i| copies.get(i).copied());
        (top_transforms(world, &copies), active_copy)
    } else {
        (originals, active)
    };
    let mode = world.get_resource::<GizmoPivot>().copied().unwrap_or_default().0;
    let pivot = selection_pivot(world, &items, active, mode, &offset_for);
    let entries: Vec<(u64, Transform, Transform)> = items
        .iter()
        .map(|(e, t)| (e.to_bits(), *t, apply_delta(t, pivot, delta)))
        .collect();
    let mut edit = GizmoEdit {
        label: match (duplicate, items.len()) {
            (true, 1) => "Duplicate and move entity".to_string(),
            (true, n) => format!("Duplicate and move {n} entities"),
            (false, 1) => "Transform gizmo".to_string(),
            (false, n) => format!("Transform {n} entities"),
        },
        duplicate: None,
        entries,
    };
    // The duplicate is already applied: write only the moves now, then hand the batch over.
    edit.apply(world);
    edit.duplicate = batch;
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    stack.push(Box::new(edit));
    if seal {
        stack.seal();
    }
    Ok(GizmoOutcome {
        moved: items.iter().map(|(e, _)| *e).collect(),
        copies,
        pivot,
    })
}

/// Ctrl+D in the viewport or Edit ▸ Duplicate: the selection's copies, offset by one metre on X
/// like the Outliner's, as one undo step; the copies become the selection.
///
/// # Errors
/// When nothing is selected or the duplicate fails.
pub fn duplicate_selection(world: &mut World) -> Result<Vec<Entity>, HierarchyError> {
    let selected = selected_entities(world);
    let copies = hierarchy_ops::duplicate_entities(world, &selected, Vec3::X)?;
    seal_undo(world);
    Ok(copies)
}

fn seal_undo(world: &mut World) {
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .seal();
}

fn selected_entities(world: &World) -> Vec<Entity> {
    world
        .get_resource::<Selection>()
        .map(|s| s.entities().to_vec())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Clipboard and the Edit / File menus

/// The editor's entity clipboard: the subtrees Copy or Cut took, as a scene file (guids of the
/// copied entities; Paste gives the pasted ones fresh guids).
#[derive(Resource, Clone, Default)]
pub struct EditorClipboard {
    /// The copied entities with their descendants; `None` when empty.
    pub file: Option<regia_scene::prelude::SceneFile>,
    /// How many top-level entities were copied.
    pub roots: usize,
    /// Each copied root's world pose at copy time, by guid (Paste at original position).
    pub world_poses: HashMap<Guid, Transform>,
}

/// Copies the subtrees of `entities` (top level) to the [`EditorClipboard`]; returns how many
/// roots were copied.
///
/// # Errors
/// When nothing is given.
pub fn copy_entities(world: &mut World, entities: &[Entity]) -> Result<usize, HierarchyError> {
    let top = hierarchy_ops::top_level(world, entities);
    if top.is_empty() {
        return Err(HierarchyError::NoEntities);
    }
    let bits: Vec<u64> = top.iter().map(|e| e.to_bits()).collect();
    let file = crate::command::subtree_snapshot(world, &bits);
    let world_poses = top
        .iter()
        .filter(|e| world.get::<Transform>(**e).is_some())
        .filter_map(|e| {
            let guid = world.get::<EntityGuid>(*e)?.0;
            Some((
                guid,
                Transform::from_affine(hierarchy_ops::world_affine(world, *e)),
            ))
        })
        .collect();
    world.insert_resource(EditorClipboard {
        file: Some(file),
        roots: top.len(),
        world_poses,
    });
    Ok(top.len())
}

/// Where Paste puts the clipboard's roots (ADR-7757).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PasteOptions {
    /// Paste as children of this entity (Edit ▸ Paste as child); `None` = the level's top level.
    pub into: Option<Entity>,
    /// Put each root at the world position it had when copied (Edit ▸ Paste at original
    /// position); otherwise its copied local transform applies in its new parent's space.
    pub at_original: bool,
}

/// Spawns the [`EditorClipboard`] with fresh guids as one undo step (children stay under their
/// pasted parents, the roots at the level's top) and selects the pasted roots. Pasting twice makes
/// two independent copies.
///
/// # Errors
/// When the clipboard is empty.
pub fn paste(world: &mut World) -> Result<Vec<Entity>, HierarchyError> {
    paste_with(world, PasteOptions::default())
}

/// [`paste`] with a target parent and the original-position option, as one undo step (the spawn
/// and the move under `into` together).
///
/// # Errors
/// When the clipboard is empty, or `into` is missing or editor-internal.
pub fn paste_with(world: &mut World, options: PasteOptions) -> Result<Vec<Entity>, HierarchyError> {
    let Some(clipboard) = world.get_resource::<EditorClipboard>().cloned() else {
        return Err(HierarchyError::NoEntities);
    };
    let Some(mut file) = clipboard.file else {
        return Err(HierarchyError::NoEntities);
    };
    if let Some(parent) = options.into {
        if world.get_entity(parent).is_err() {
            return Err(HierarchyError::ParentMissing(parent.to_bits()));
        }
        if crate::visibility::is_hidden(world, parent) {
            return Err(HierarchyError::ParentInternal(parent.to_bits()));
        }
    }
    if options.at_original {
        // Only the copied roots have a recorded pose; they spawn at the top level, so their
        // local transform is the world pose.
        for record in &mut file.entities {
            if let Some(pose) = clipboard.world_poses.get(&record.guid)
                && let Some(raw) = transform_raw(world, pose)
            {
                record
                    .components
                    .insert("regia_scene::transform::Transform".to_string(), raw);
            }
        }
    }
    let remap: HashMap<Guid, Guid> = file
        .entities
        .iter()
        .map(|record| (record.guid, Guid::new_random()))
        .collect();
    let copied: HashSet<Guid> = remap.keys().copied().collect();
    let mut roots = Vec::new();
    for record in &mut file.entities {
        let parent = record.parent.filter(|p| copied.contains(p));
        if parent.is_none() {
            roots.push(remap.get(&record.guid).copied().unwrap_or(record.guid));
        }
        record.guid = remap.get(&record.guid).copied().unwrap_or(record.guid);
        record.parent = parent.and_then(|p| remap.get(&p).copied());
    }
    // A pasted reflection capture takes an unused id (a Cut's keeps its own; ADR-7663).
    crate::shapes::fresh_capture_ids(world, &mut file.entities);
    let mut spawn = SpawnEntities {
        file,
        spawned_guids: Vec::new(),
        spawned_entities: Vec::new(),
    };
    spawn.apply(world);
    let pasted: Vec<Entity> = roots
        .iter()
        .filter_map(|guid| hierarchy_ops::guid_entity(world, *guid))
        .collect();
    let mut commands: Vec<Box<dyn EditorCommand>> = vec![Box::new(spawn)];
    if let Some(parent) = options.into {
        // Paste as child: the roots move under `into` in the same undo step, keeping the world
        // pose when pasting at the original position, else their copied local transform.
        match hierarchy_ops::plan_reparent(world, &pasted, Some(parent), options.at_original) {
            Ok(plan) => {
                let mut command = plan.command;
                command.apply(world);
                commands.push(Box::new(command));
            }
            Err(err) => {
                for mut command in commands.into_iter().rev() {
                    command.revert(world);
                }
                return Err(err);
            }
        }
    }
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    if commands.len() == 1 {
        if let Some(command) = commands.pop() {
            stack.push(command);
        }
    } else {
        stack.push(Box::new(hierarchy_ops::CommandBatch {
            label: "Paste as child".to_string(),
            commands,
        }));
    }
    stack.seal();
    select_exactly(world, &pasted);
    Ok(pasted)
}

/// Serializes `transform` the way scene files store it.
fn transform_raw(world: &World, transform: &Transform) -> Option<Box<ron::value::RawValue>> {
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let mut codec = regia_ecs::serialize::ReflectCodec::new(&registry);
    let text = codec.to_ron_typed(transform.as_partial_reflect()).ok()?;
    ron::value::RawValue::from_ron(text.trim_end())
        .ok()
        .map(|raw| raw.to_owned())
}

fn select_exactly(world: &mut World, entities: &[Entity]) {
    let mut selection = world.get_resource_or_insert_with(Selection::default).clone();
    selection.clear();
    for entity in entities {
        selection.select(*entity, SelectMode::Add);
    }
    world.insert_resource(selection);
}

/// Every entity the Outliner lists (user entities) that is not locked: what Edit ▸ Select all
/// selects.
#[must_use]
pub fn selectable_entities(world: &World) -> Vec<Entity> {
    fn walk(nodes: &[crate::outliner::HierarchyNode], out: &mut Vec<Entity>) {
        for node in nodes {
            out.push(node.entity);
            walk(&node.children, out);
        }
    }
    let mut all = Vec::new();
    walk(&crate::outliner::hierarchy_tree(world), &mut all);
    all.retain(|e| world.get::<EditorLocked>(*e).is_none());
    all
}

/// Recently opened or saved levels, newest first (File ▸ Recent). Editor-session state; the
/// shell notes the open level each frame ([`note_recent_level`]).
#[derive(Resource, Debug, Clone, Default)]
pub struct RecentLevels {
    /// Level paths, newest first, at most [`RecentLevels::CAP`].
    pub paths: Vec<std::path::PathBuf>,
}

impl RecentLevels {
    /// How many paths are kept.
    pub const CAP: usize = 10;

    /// Moves `path` to the front.
    pub fn note(&mut self, path: &std::path::Path) {
        self.paths.retain(|p| p != path);
        self.paths.insert(0, path.to_path_buf());
        self.paths.truncate(Self::CAP);
    }
}

/// Adds the open level's path to [`RecentLevels`] when it is not already the newest entry.
pub fn note_recent_level(world: &mut World) {
    let Some(path) = world
        .get_resource::<crate::level_io::OpenLevel>()
        .and_then(|open| open.path.clone())
    else {
        return;
    };
    let mut recent = world.get_resource_or_insert_with(RecentLevels::default);
    if recent.paths.first() != Some(&path) {
        recent.note(&path);
    }
}

/// Every menu item [`invoke_menu`] runs, as `Menu/Item` paths (`File/Recent` takes an index:
/// `File/Recent/0` is the newest).
pub const MENU_PATHS: &[&str] = &[
    "File/Save all",
    "File/Recent",
    "File/Exit",
    "Edit/Undo",
    "Edit/Redo",
    "Edit/Cut",
    "Edit/Copy",
    "Edit/Paste",
    "Edit/Duplicate",
    "Edit/Delete",
    "Edit/Select all",
    "Edit/Paste as child",
    "Edit/Paste at original position",
];

/// Why [`invoke_menu`] did nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuError {
    /// No such menu item.
    UnknownPath(String),
    /// The item needs a selection and there is none.
    NothingSelected,
    /// Paste with an empty clipboard.
    ClipboardEmpty,
    /// `File/Recent/<n>` names no entry.
    NoRecent(String),
    /// The item ran and failed.
    Failed(String),
}

impl MenuError {
    /// The protocol error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownPath(_) => "menu_unknown_path",
            Self::NothingSelected => "menu_nothing_selected",
            Self::ClipboardEmpty => "menu_clipboard_empty",
            Self::NoRecent(_) => "menu_no_recent",
            Self::Failed(_) => "menu_failed",
        }
    }
}

impl std::fmt::Display for MenuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownPath(path) => write!(f, "no menu item `{path}`"),
            Self::NothingSelected => write!(f, "nothing is selected"),
            Self::ClipboardEmpty => write!(f, "the clipboard is empty"),
            Self::NoRecent(what) => write!(f, "no recent level {what}"),
            Self::Failed(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for MenuError {}

fn bits_json(entities: &[Entity]) -> serde_json::Value {
    serde_json::json!(entities.iter().map(|e| e.to_bits()).collect::<Vec<_>>())
}

/// Runs a File or Edit menu item by its `Menu/Item` path ([`MENU_PATHS`]); the menu bar and
/// `editor.menu_invoke` share it. Returns what the item did as JSON.
///
/// - `File/Save all`: saves the open level (when it has a path), the 2D mode's dirty tile map and
///   every other document the Save all dialog can save (the menu item itself opens the dialog:
///   `editor.save_all { dialog: true }`).
/// - `File/Recent` lists [`RecentLevels`]; `File/Recent/<n>` opens entry `n`.
/// - `File/Exit`: asks the editor to quit, the same request as the window's close button.
/// - `Edit/Cut` copies then deletes the selection (one undo step), `Edit/Copy` copies it,
///   `Edit/Paste` spawns the clipboard (one undo step), `Edit/Duplicate` is Ctrl+D,
///   `Edit/Delete` deletes the selection, `Edit/Select all` selects every unlocked entity.
///
/// # Errors
/// [`MenuError`]: an unknown path, a missing selection or clipboard, or a failed item.
pub fn invoke_menu(world: &mut World, path: &str) -> Result<serde_json::Value, MenuError> {
    use serde_json::json;
    let failed = |e: HierarchyError| MenuError::Failed(e.to_string());
    let selection = selected_entities(world);
    let need_selection = || {
        if selection.is_empty() {
            Err(MenuError::NothingSelected)
        } else {
            Ok(())
        }
    };
    if let Some(index) = path.strip_prefix("File/Recent/") {
        let n: usize = index
            .parse()
            .map_err(|_| MenuError::NoRecent(format!("`{index}`")))?;
        let entry = world
            .get_resource::<RecentLevels>()
            .and_then(|r| r.paths.get(n).cloned())
            .ok_or_else(|| MenuError::NoRecent(format!("{n}")))?;
        let open =
            crate::level_io::open_level_file(world, &entry).map_err(|e| MenuError::Failed(e.to_string()))?;
        world.insert_resource(open);
        crate::level_io::reset_edit_state(world);
        note_recent_level(world);
        return Ok(json!({ "opened": entry.display().to_string() }));
    }
    match path {
        "File/Save all" => {
            let has_path = world
                .get_resource::<crate::level_io::OpenLevel>()
                .is_some_and(|open| open.path.is_some());
            let level = if has_path {
                Some(
                    crate::agent_ext::editor_save_level(world, &json!({}))
                        .map_err(|e| MenuError::Failed(e.message.clone()))?,
                )
            } else {
                None
            };
            note_recent_level(world);
            // The Save all dialog's other savable documents too (prefab, material graphs, UI
            // document, shot list), as its Save selected with every row checked does
            // (sprint-w12-rev-g10 row 3).
            let ids: Vec<String> = crate::save_all_dialog::dirty_documents(world)
                .into_iter()
                .filter(|doc| doc.can_save && doc.id != crate::save_all_dialog::LEVEL_ID)
                .map(|doc| doc.id)
                .collect();
            let others = crate::save_all_dialog::save_documents(world, &ids).to_json();
            Ok(json!({
                "level": level,
                "saved": others["saved"],
                "failed": others["failed"],
                "skipped": others["skipped"],
            }))
        }
        "File/Recent" => {
            let paths: Vec<String> = world
                .get_resource::<RecentLevels>()
                .map(|r| r.paths.iter().map(|p| p.display().to_string()).collect())
                .unwrap_or_default();
            Ok(json!({ "recent": paths }))
        }
        "File/Exit" => {
            let request = regia_platform::request_quit(world);
            Ok(json!({ "exiting": request.will_exit }))
        }
        "Edit/Undo" => Ok(json!({ "undone": crate::command::undo_world(world) })),
        "Edit/Redo" => Ok(json!({ "redone": crate::command::redo_world(world) })),
        "Edit/Copy" => {
            need_selection()?;
            let roots = copy_entities(world, &selection).map_err(failed)?;
            Ok(json!({ "copied": roots }))
        }
        "Edit/Cut" => {
            need_selection()?;
            let roots = copy_entities(world, &selection).map_err(failed)?;
            let deleted = hierarchy_ops::delete_entities(world, &selection).map_err(failed)?;
            seal_undo(world);
            Ok(json!({ "copied": roots, "deleted": bits_json(&deleted) }))
        }
        "Edit/Paste" => {
            let pasted = paste(world).map_err(|e| match e {
                HierarchyError::NoEntities => MenuError::ClipboardEmpty,
                other => failed(other),
            })?;
            Ok(json!({ "pasted": bits_json(&pasted) }))
        }
        "Edit/Duplicate" => {
            need_selection()?;
            let copies = duplicate_selection(world).map_err(failed)?;
            Ok(json!({ "copies": bits_json(&copies) }))
        }
        "Edit/Delete" => {
            need_selection()?;
            let deleted = hierarchy_ops::delete_entities(world, &selection).map_err(failed)?;
            seal_undo(world);
            Ok(json!({ "deleted": bits_json(&deleted) }))
        }
        "Edit/Select all" => {
            let all = selectable_entities(world);
            select_exactly(world, &all);
            Ok(json!({ "selected": all.len() }))
        }
        "Edit/Paste as child" | "Edit/Paste at original position" => {
            let as_child = path == "Edit/Paste as child";
            let into = if as_child {
                need_selection()?;
                world.get_resource::<Selection>().and_then(Selection::primary)
            } else {
                None
            };
            let options = PasteOptions {
                into,
                at_original: !as_child,
            };
            let pasted = paste_with(world, options).map_err(|e| match e {
                HierarchyError::NoEntities => MenuError::ClipboardEmpty,
                other => failed(other),
            })?;
            Ok(json!({ "pasted": bits_json(&pasted), "parent": into.map(|e| e.to_bits()) }))
        }
        other => Err(MenuError::UnknownPath(other.to_string())),
    }
}

// ---------------------------------------------------------------------------------------------
// Outliner toggles

/// Shows or hides an entity in the viewport (the Outliner eye) as one undo step: hidden writes
/// `Visibility::Hidden`, shown restores the value it had (`Inherited` when it had none hidden).
pub struct SetVisibility {
    /// The entity (bits).
    pub entity: u64,
    /// The component before (`None`: absent).
    pub old: Option<Visibility>,
    /// The component after.
    pub new: Visibility,
}

impl SetVisibility {
    fn write(world: &mut World, bits: u64, value: Option<Visibility>) {
        let Some(entity) = resolve_entity(world, bits) else {
            return;
        };
        match value {
            Some(v) => {
                world.entity_mut(entity).insert(v);
            }
            None => {
                world.entity_mut(entity).remove::<Visibility>();
            }
        }
    }
}

impl EditorCommand for SetVisibility {
    fn apply(&mut self, world: &mut World) {
        Self::write(world, self.entity, Some(self.new));
    }

    fn revert(&mut self, world: &mut World) {
        Self::write(world, self.entity, self.old);
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        if self.new == Visibility::Hidden {
            "Hide entity".to_string()
        } else {
            "Show entity".to_string()
        }
    }
}

/// Whether the Outliner eye shows `entity` (its own `Visibility` is not `Hidden`).
#[must_use]
pub fn is_visible(world: &World, entity: Entity) -> bool {
    !matches!(world.get::<Visibility>(entity), Some(Visibility::Hidden))
}

/// The Outliner eye: shows or hides `entity` as one undo step. Returns false when it already was.
pub fn set_visible(world: &mut World, entity: Entity, visible: bool) -> bool {
    if is_visible(world, entity) == visible {
        return false;
    }
    let old = world.get::<Visibility>(entity).copied();
    let new = if visible {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
    let mut command = SetVisibility {
        entity: entity.to_bits(),
        old,
        new,
    };
    command.apply(world);
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    stack.push(Box::new(command));
    stack.seal();
    true
}

/// The Outliner lock: a locked entity cannot be picked or box-selected and leaves the selection.
/// Not an undo step (editor-session state). Returns false when it already was.
pub fn set_locked(world: &mut World, entity: Entity, locked: bool) -> bool {
    if (world.get::<EditorLocked>(entity).is_some()) == locked {
        return false;
    }
    if locked {
        world.entity_mut(entity).insert(EditorLocked);
        let mut selection = world.get_resource_or_insert_with(Selection::default).clone();
        let kept: Vec<Entity> = selection
            .entities()
            .iter()
            .copied()
            .filter(|e| !is_locked(world, *e))
            .collect();
        selection.clear();
        for e in kept {
            selection.select(e, SelectMode::Add);
        }
        world.insert_resource(selection);
    } else {
        world.entity_mut(entity).remove::<EditorLocked>();
    }
    true
}

/// The Outliner's prefab marks of `entity`: whether it is a prefab instance root (the link icon)
/// and whether that instance has overrides (the override marker).
#[must_use]
pub fn prefab_marks(world: &World, entity: Entity) -> (bool, bool) {
    match world.get::<regia_scene::prefab::PrefabInstance>(entity) {
        Some(instance) => (
            true,
            !instance.overrides.is_empty()
                || !instance.removed_children.is_empty()
                || !instance.added_components.is_empty()
                || !instance.removed_components.is_empty(),
        ),
        None => (false, false),
    }
}

/// Whether `entity` has a scene guid (copy, cut and duplicate need one).
#[must_use]
pub fn has_guid(world: &World, entity: Entity) -> bool {
    world.get::<EntityGuid>(entity).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(x: f32, y: f32, z: f32) -> Transform {
        Transform::from_translation(Vec3::new(x, y, z))
    }

    #[test]
    fn preset_ease_lands_exactly_and_passes_between() {
        let from = EditorCamera::default();
        let to = ViewPreset::Top.pose(&from);
        let mut ease = CameraEase::new(from.clone(), to.clone());
        let mid = ease.advance(EASE_SECONDS * 0.5);
        assert!(mid.pitch > from.pitch && mid.pitch < to.pitch, "{mid:?}");
        let end = ease.advance(EASE_SECONDS * 0.5);
        assert_eq!(end, to);
        assert!(ease.done());
    }

    #[test]
    fn yaw_eases_the_short_way_round() {
        let from = EditorCamera {
            yaw: 3.0,
            ..EditorCamera::default()
        };
        let to = EditorCamera {
            yaw: -3.0,
            ..from.clone()
        };
        let ease = CameraEase {
            elapsed: EASE_SECONDS * 0.5,
            ..CameraEase::new(from, to)
        };
        let mid = ease.sample();
        assert!(mid.yaw > 3.0, "crosses π, not 0: {}", mid.yaw);
    }

    #[test]
    fn pivot_modes_rotate_about_their_points() {
        let mut world = World::new();
        let a = world.spawn_empty().id();
        let b = world.spawn_empty().id();
        let items = [(a, t(0.0, 0.0, 0.0)), (b, t(2.0, 0.0, 0.0))];
        let half_turn = GizmoDelta {
            rotation: Quat::from_rotation_y(std::f32::consts::PI),
            ..GizmoDelta::default()
        };
        let rotate = |mode| {
            let p = pivot_point(&items, Some(b), mode);
            items
                .iter()
                .map(|(_, tr)| apply_delta(tr, p, &half_turn).translation)
                .collect::<Vec<_>>()
        };
        let near = |v: Vec3, w: Vec3| (v - w).length() < 1e-4;
        let median = rotate(PivotMode::Median);
        assert!(
            near(median[0], Vec3::new(2.0, 0.0, 0.0)) && near(median[1], Vec3::ZERO),
            "{median:?}"
        );
        let active = rotate(PivotMode::Active);
        assert!(near(active[0], Vec3::new(4.0, 0.0, 0.0)) && near(active[1], Vec3::new(2.0, 0.0, 0.0)));
        let individual = rotate(PivotMode::Individual);
        assert!(near(individual[0], Vec3::ZERO) && near(individual[1], Vec3::new(2.0, 0.0, 0.0)));
    }

    #[test]
    fn world_and_parent_poses_round_trip_under_a_turned_scaled_parent() {
        let parent = Affine3A::from_scale_rotation_translation(
            Vec3::splat(2.0),
            Quat::from_rotation_y(0.7),
            Vec3::new(5.0, -1.0, 2.0),
        );
        let local = Transform {
            translation: Vec3::new(1.0, 2.0, 3.0),
            rotation: Quat::from_rotation_x(0.4),
            scale: Vec3::new(1.0, 0.5, 2.0),
        };
        let pose = to_world(parent, &local);
        assert!((pose.translation - parent.transform_point3(local.translation)).length() < 1e-5);
        assert!((pose.scale - local.scale * 2.0).length() < 1e-5);
        let back = to_parent(parent, &pose).expect("invertible");
        assert!((back.translation - local.translation).length() < 1e-5);
        assert!(back.rotation.angle_between(local.rotation) < 1e-5);
        assert!((back.scale - local.scale).length() < 1e-5);
        assert!(to_parent(Affine3A::from_scale(Vec3::new(0.0, 1.0, 1.0)), &pose).is_none());
    }

    #[test]
    fn each_space_has_its_basis_and_a_step_in_it_lands_in_the_parent() {
        use crate::viewport::GizmoSpace;
        let own = Quat::from_rotation_z(0.5);
        let parent_rotation = Quat::from_rotation_y(1.0);
        let view = Quat::from_rotation_x(-0.3);
        assert_eq!(
            space_basis(GizmoSpace::World, own, parent_rotation, view),
            Quat::IDENTITY
        );
        assert_eq!(space_basis(GizmoSpace::Local, own, parent_rotation, view), own);
        assert_eq!(
            space_basis(GizmoSpace::Parent, own, parent_rotation, view),
            parent_rotation
        );
        assert_eq!(space_basis(GizmoSpace::View, own, parent_rotation, view), view);
        // Along the parent's own X in its space is local +X.
        let parent = Affine3A::from_rotation_translation(parent_rotation, Vec3::new(3.0, 0.0, 0.0));
        let step = delta_in_parent(parent_rotation, parent, Vec3::X, Quat::IDENTITY, Vec3::ONE);
        assert!(
            (step.translation - Vec3::X).length() < 1e-5,
            "{}",
            step.translation
        );
        // A world turn about Y under a parent turned about Y is the same turn locally.
        let turn = delta_in_parent(
            Quat::IDENTITY,
            parent,
            Vec3::ZERO,
            Quat::from_rotation_y(0.2),
            Vec3::ONE,
        );
        assert!(turn.rotation.angle_between(Quat::from_rotation_y(0.2)) < 1e-5);
        // A view rotation's columns are the camera's right, up and back.
        let camera = crate::views::ViewCamera {
            eye: Vec3::new(0.0, 0.0, 10.0),
            target: Vec3::ZERO,
            fov_y: 0.8,
            near: 0.1,
            far: 100.0,
            ortho: false,
        };
        let rotation = view_rotation(&camera);
        assert!((rotation * Vec3::X - Vec3::X).length() < 1e-5);
        assert!(
            (rotation * Vec3::Z - Vec3::Z).length() < 1e-5,
            "back toward the eye"
        );
    }

    /// Review finding 13 of ADR-7756: an entity with cached model bounds (here a resolved primitive
    /// whose box is ±1, scaled 2 and moved to x = 2) counts as that box's world corners; its part
    /// mesh, a much larger box, is not walked vertex by vertex.
    #[test]
    fn cached_model_bounds_give_the_bounds_without_a_vertex_walk() {
        let mut world = World::new();
        let place = Transform {
            translation: Vec3::new(2.0, 0.0, 0.0),
            scale: Vec3::splat(2.0),
            ..Transform::IDENTITY
        };
        let owner = world
            .spawn((
                place,
                regia_scene::prelude::GlobalTransform(place.compute_affine()),
                crate::mesh_source::ResolvedPrimitive {
                    primitive: regia_render::Primitive::default(),
                    part: None,
                    aabb: Some((Vec3::splat(-1.0), Vec3::splat(1.0))),
                    error: None,
                },
            ))
            .id();
        let (vertices, indices) = regia_render::box_mesh([50.0, 50.0, 50.0]);
        world.spawn((
            regia_render::MeshRenderer::new(
                std::sync::Arc::new(regia_render::CpuMesh { vertices, indices }),
                std::sync::Arc::new(regia_render::LabMaterial::default()),
            ),
            crate::mesh_source::MeshPart { owner },
        ));
        assert_eq!(
            selection_bounds(&world, &[owner]),
            Some((Vec3::new(0.0, -2.0, -2.0), Vec3::new(4.0, 2.0, 2.0)))
        );
    }

    #[test]
    fn bounds_without_meshes_is_the_translations_box() {
        let mut world = World::new();
        let a = world.spawn_empty().id();
        let b = world.spawn_empty().id();
        let c = world.spawn_empty().id();
        let items = [
            (a, t(0.0, 0.0, 0.0)),
            (b, t(1.0, 0.0, 0.0)),
            (c, t(4.0, 2.0, 0.0)),
        ];
        assert_eq!(
            pivot_point(&items, None, PivotMode::Bounds),
            Some(Vec3::new(2.0, 1.0, 0.0))
        );
        assert_eq!(PivotMode::from_name("bounds"), Some(PivotMode::Bounds));
        assert!(PivotMode::ALL.contains(&PivotMode::Bounds));
    }

    #[test]
    fn a_bad_snap_step_changes_nothing_and_steps_without_a_project_are_not_saved() {
        let mut world = World::new();
        world.insert_resource(crate::viewport::GizmoState::default());
        assert_eq!(
            set_snap_steps(&mut world, Some(1.0), Some(f32::NAN), None),
            Err(SnapStepError::Invalid("angle_deg"))
        );
        assert_eq!(world.resource::<crate::viewport::GizmoState>().snap_distance, 0.5);
        assert_eq!(set_snap_steps(&mut world, Some(1.0), None, None), Ok(None));
        assert_eq!(world.resource::<crate::viewport::GizmoState>().snap_distance, 1.0);
        assert_eq!(SnapStepError::Invalid("x").code(), "snap_step_invalid");
        assert_eq!(SnapStepError::Io(String::new()).code(), "snap_steps_io");
    }

    #[test]
    fn snap_toggle_is_inverted_by_ctrl() {
        assert!(!effective_snap(false, false));
        assert!(effective_snap(false, true));
        assert!(effective_snap(true, false));
        assert!(!effective_snap(true, true));
    }

    #[test]
    fn delta_between_round_trips() {
        let before = Transform {
            translation: Vec3::new(1.0, 2.0, 3.0),
            rotation: Quat::from_rotation_x(0.3),
            scale: Vec3::new(1.0, 2.0, 4.0),
        };
        let after = Transform {
            translation: Vec3::new(2.0, 2.0, 3.0),
            rotation: Quat::from_rotation_y(0.5) * before.rotation,
            scale: Vec3::new(2.0, 2.0, 2.0),
        };
        let delta = GizmoDelta::between(&before, &after);
        let again = apply_delta(&before, None, &delta);
        assert!((again.translation - after.translation).length() < 1e-5);
        assert!((again.scale - after.scale).length() < 1e-5);
        assert!(again.rotation.angle_between(after.rotation) < 1e-4);
    }

    #[test]
    fn presets_parse_and_map_digits() {
        for preset in ViewPreset::ALL {
            assert_eq!(ViewPreset::from_name(preset.name()), Some(preset));
        }
        assert_eq!(ViewPreset::from_digit(2), Some(ViewPreset::Top));
        assert_eq!(ViewPreset::from_digit(0), None);
        assert_eq!(ViewPreset::from_digit(5), None);
        assert_eq!(PivotMode::from_name("active"), Some(PivotMode::Active));
    }
}
