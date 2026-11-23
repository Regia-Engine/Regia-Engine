// SPDX-License-Identifier: MPL-2.0
//! Editable collider shapes in the viewport (`docs/specs/19-editor.md` § 3, ADR-2696): grips on the
//! faces of a box, the surface of a sphere and the ends and sides of a capsule change its size
//! about its center, and a grip at the shape's center moves its offset.
//!
//! The grips are [`crate::handles::Handle`]s, driven by the shared handle widget. A drag previews
//! live on the entity and pushes one [`SetCollider`] when the pointer lets go, so it is one undo
//! step however long it ran ([`begin_drag`], [`update_drag`], [`end_drag`]). The math is free
//! functions on a [`Collider`] and a body pose ([`handles_for`], [`apply_handle`]), so it is tested
//! without a viewport.
//!
//! The collider tool ([`ColliderHandleTool`]) is a viewport toggle: while it is on, the primary
//! selection's collider shows its outline and grips instead of the transform gizmo.
//!
//! Colliders of other shapes (cylinder, cone, hull, mesh, heightfield) get the offset grip only.
//! The body pose ignores scale, as the physics step does.
//!
//! The agent surface is `editor.collider_handles` and `editor.collider_handle_drag`
//! (`book/agents/viewport-tools.md`).

use regia::physics::{Collider, ColliderShape, ShapeAxis};
use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::{Isometry, Vec3};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::command::{EditorCommand, UndoStack, resolve_entity};
use crate::handles::{DragPhase, Handle, SnapSet};
use crate::hierarchy_ops::world_affine;
use crate::selection::Selection;
use crate::viewport_agent::Handler;
use crate::views::ViewCamera;

/// The smallest half-extent or radius a grip leaves, metres.
pub const MIN_COLLIDER_SIZE: f32 = 1.0e-3;
/// Segments per circle of the drawn outline.
const OUTLINE_SEGMENTS: usize = 24;
/// The part of the way from a shape's center to its surface where a size grip starts: the rest of
/// the line stays free to grab the center grip.
const GRIP_START: f32 = 0.75;
/// The drawn outline.
const OUTLINE_COLOR: egui::Color32 = egui::Color32::from_rgb(120, 230, 150);

/// What a collider grip edits. Axes are 0, 1, 2 for X, Y, Z of the shape's frame (for a capsule, the
/// frame whose Y is its long axis).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColliderHandleKind {
    /// A box half-extent, from the face on the positive or negative side.
    Extent {
        /// The axis.
        axis: u8,
        /// The face on the positive side.
        positive: bool,
    },
    /// A sphere's or capsule's radius, from a point on that axis.
    Radius {
        /// The axis.
        axis: u8,
    },
    /// A capsule's half-height, from the tip of its positive or negative end.
    HalfHeight {
        /// The positive end.
        positive: bool,
    },
    /// The collider's offset from its body.
    Offset,
}

const AXIS_NAMES: [&str; 3] = ["x", "y", "z"];

impl ColliderHandleKind {
    /// The wire name: `extent_x+`, `extent_x-`, `radius_y`, `half_height+`, `offset`.
    #[must_use]
    pub fn name(self) -> String {
        let sign = |positive: bool| if positive { '+' } else { '-' };
        match self {
            Self::Extent { axis, positive } => {
                format!(
                    "extent_{}{}",
                    AXIS_NAMES[usize::from(axis.min(2))],
                    sign(positive)
                )
            }
            Self::Radius { axis } => format!("radius_{}", AXIS_NAMES[usize::from(axis.min(2))]),
            Self::HalfHeight { positive } => format!("half_height{}", sign(positive)),
            Self::Offset => "offset".to_owned(),
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let axis = |name: &str| AXIS_NAMES.iter().position(|a| *a == name).map(|i| i as u8);
        if text == "offset" {
            return Some(Self::Offset);
        }
        if let Some(rest) = text.strip_prefix("extent_") {
            let (name, positive) = match (rest.strip_suffix('+'), rest.strip_suffix('-')) {
                (Some(name), _) => (name, true),
                (_, Some(name)) => (name, false),
                _ => return None,
            };
            return Some(Self::Extent {
                axis: axis(name)?,
                positive,
            });
        }
        if let Some(rest) = text.strip_prefix("radius_") {
            return Some(Self::Radius { axis: axis(rest)? });
        }
        match text {
            "half_height+" => Some(Self::HalfHeight { positive: true }),
            "half_height-" => Some(Self::HalfHeight { positive: false }),
            _ => None,
        }
    }
}

/// One grip: what it edits and where it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColliderGrip {
    /// What it edits.
    pub kind: ColliderHandleKind,
    /// The grip, world space.
    pub handle: Handle,
}

/// The body pose of `entity`: its world matrix without scale, never a frame stale.
#[must_use]
pub fn body_pose(world: &World, entity: Entity) -> Isometry {
    Isometry::from_affine(&world_affine(world, entity))
}

/// The shape's own frame: the body, then the offset, then a capsule's axis.
fn shape_frame(collider: &Collider, body: Isometry) -> Isometry {
    let pose = body * Isometry::new(collider.offset.rotation, collider.offset.translation);
    match collider.shape {
        ColliderShape::Capsule { axis, .. } => pose * axis_turn(axis),
        _ => pose,
    }
}

fn axis_turn(axis: ShapeAxis) -> Isometry {
    Isometry::new(axis.from_y_rotation(), Vec3::ZERO)
}

fn unit(axis: u8) -> Vec3 {
    match axis {
        0 => Vec3::X,
        1 => Vec3::Y,
        _ => Vec3::Z,
    }
}

/// A short arrow on the line from `center` along the unit `dir`, whose tip is `length` away.
fn grip(center: Vec3, dir: Vec3, length: f32) -> Handle {
    Handle::Arrow {
        origin: center + dir * (length * GRIP_START),
        dir: dir * (length * (1.0 - GRIP_START)),
    }
}

/// The grips of `collider` on a body at `body`.
#[must_use]
pub fn handles_for(collider: &Collider, body: Isometry) -> Vec<ColliderGrip> {
    let frame = shape_frame(collider, body);
    let center = frame.translation;
    let axis = |a: u8| frame.rotation * unit(a);
    // First: the widget gives ties to the earlier handle, and a size grip pointing at the camera
    // lies on top of the center, where it could not be dragged anyway.
    let mut out = vec![ColliderGrip {
        kind: ColliderHandleKind::Offset,
        handle: Handle::Point { pos: center },
    }];
    match collider.shape {
        ColliderShape::Box { half_extents } => {
            for a in 0..3_u8 {
                let half = half_extents[usize::from(a)];
                for positive in [true, false] {
                    let dir = if positive { axis(a) } else { -axis(a) };
                    out.push(ColliderGrip {
                        kind: ColliderHandleKind::Extent { axis: a, positive },
                        handle: grip(center, dir, half),
                    });
                }
            }
        }
        ColliderShape::Sphere { radius } => {
            for a in 0..3_u8 {
                out.push(ColliderGrip {
                    kind: ColliderHandleKind::Radius { axis: a },
                    handle: grip(center, axis(a), radius),
                });
            }
        }
        ColliderShape::Capsule {
            half_height, radius, ..
        } => {
            for positive in [true, false] {
                let dir = if positive { axis(1) } else { -axis(1) };
                out.push(ColliderGrip {
                    kind: ColliderHandleKind::HalfHeight { positive },
                    handle: grip(center, dir, half_height + radius),
                });
            }
            for a in [0_u8, 2] {
                out.push(ColliderGrip {
                    kind: ColliderHandleKind::Radius { axis: a },
                    handle: grip(center, axis(a), radius),
                });
            }
        }
        _ => {}
    }
    out
}

/// `collider` with the grip `kind` moved to the world point `point`. A size is measured from the
/// shape's center along the grip's axis (the other side moves with it) and never goes below
/// [`MIN_COLLIDER_SIZE`]. `None` when the point is not finite or the grip is not one of this shape's.
#[must_use]
pub fn apply_handle(
    collider: &Collider,
    body: Isometry,
    kind: ColliderHandleKind,
    point: Vec3,
) -> Option<Collider> {
    if !point.is_finite() {
        return None;
    }
    let local = shape_frame(collider, body).inverse().transform_point(point);
    let mut next = collider.clone();
    match (kind, &mut next.shape) {
        (ColliderHandleKind::Offset, _) => {
            next.offset.translation = body.inverse().transform_point(point);
        }
        (ColliderHandleKind::Extent { axis, positive }, ColliderShape::Box { half_extents }) => {
            let a = usize::from(axis.min(2));
            let along = if positive { local[a] } else { -local[a] };
            half_extents[a] = along.max(MIN_COLLIDER_SIZE);
        }
        (ColliderHandleKind::Radius { axis }, ColliderShape::Sphere { radius }) => {
            *radius = local[usize::from(axis.min(2))].max(MIN_COLLIDER_SIZE);
        }
        (ColliderHandleKind::Radius { axis }, ColliderShape::Capsule { radius, .. }) if axis != 1 => {
            *radius = local[usize::from(axis.min(2))].max(MIN_COLLIDER_SIZE);
        }
        (
            ColliderHandleKind::HalfHeight { positive },
            ColliderShape::Capsule {
                half_height, radius, ..
            },
        ) => {
            let along = if positive { local.y } else { -local.y };
            *half_height = (along - *radius).max(MIN_COLLIDER_SIZE);
        }
        _ => return None,
    }
    Some(next)
}

/// The line segments of `collider`'s outline, world space (box edges, circles of a sphere, circles
/// and sides of a capsule; other shapes draw none).
#[must_use]
pub fn outline_segments(collider: &Collider, body: Isometry) -> Vec<(Vec3, Vec3)> {
    let frame = shape_frame(collider, body);
    let at = |p: Vec3| frame.transform_point(p);
    let mut out = Vec::new();
    let circle = |out: &mut Vec<(Vec3, Vec3)>, center: Vec3, a: Vec3, b: Vec3, radius: f32| {
        for i in 0..OUTLINE_SEGMENTS {
            let angle = |k: usize| k as f32 / OUTLINE_SEGMENTS as f32 * std::f32::consts::TAU;
            let point = |t: f32| at(center + (a * t.cos() + b * t.sin()) * radius);
            out.push((point(angle(i)), point(angle(i + 1))));
        }
    };
    match collider.shape {
        ColliderShape::Box { half_extents: h } => {
            let corner = |x: f32, y: f32, z: f32| at(Vec3::new(x * h.x, y * h.y, z * h.z));
            for s in [-1.0, 1.0] {
                for t in [-1.0, 1.0] {
                    out.push((corner(-1.0, s, t), corner(1.0, s, t)));
                    out.push((corner(s, -1.0, t), corner(s, 1.0, t)));
                    out.push((corner(s, t, -1.0), corner(s, t, 1.0)));
                }
            }
        }
        ColliderShape::Sphere { radius } => {
            circle(&mut out, Vec3::ZERO, Vec3::X, Vec3::Y, radius);
            circle(&mut out, Vec3::ZERO, Vec3::Y, Vec3::Z, radius);
            circle(&mut out, Vec3::ZERO, Vec3::Z, Vec3::X, radius);
        }
        ColliderShape::Capsule {
            half_height, radius, ..
        } => {
            for y in [half_height, -half_height] {
                let center = Vec3::Y * y;
                circle(&mut out, center, Vec3::X, Vec3::Z, radius);
                circle(&mut out, center, Vec3::X, Vec3::Y, radius);
                circle(&mut out, center, Vec3::Z, Vec3::Y, radius);
            }
            for dir in [Vec3::X, Vec3::NEG_X, Vec3::Z, Vec3::NEG_Z] {
                out.push((
                    at(dir * radius + Vec3::Y * half_height),
                    at(dir * radius - Vec3::Y * half_height),
                ));
            }
        }
        _ => {}
    }
    out
}

/// World points on the outline of `entity`'s collider: what a drop measures for an entity with no
/// mesh. Empty without a collider or for shapes with no outline.
#[must_use]
pub fn collider_outline_points(world: &World, entity: Entity) -> Vec<Vec3> {
    let Some(collider) = world.get::<Collider>(entity) else {
        return Vec::new();
    };
    outline_segments(collider, body_pose(world, entity))
        .into_iter()
        .flat_map(|(a, b)| [a, b])
        .collect()
}

/// One collider edit: the whole collider before and after. Never merges, so a drag is one entry.
pub struct SetCollider {
    /// Target entity (bits).
    pub entity: u64,
    /// The collider before.
    pub old: Collider,
    /// The collider after.
    pub new: Collider,
}

impl SetCollider {
    fn write(&self, world: &mut World, value: &Collider) {
        let Some(entity) = resolve_entity(world, self.entity) else {
            tracing::warn!(target: "regia_editor", "SetCollider: entity {} is gone", self.entity);
            return;
        };
        world.entity_mut(entity).insert(value.clone());
    }
}

impl EditorCommand for SetCollider {
    fn apply(&mut self, world: &mut World) {
        self.write(world, &self.new);
    }

    fn revert(&mut self, world: &mut World) {
        self.write(world, &self.old);
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        "Collider handle".to_owned()
    }
}

/// A grip drag in progress.
#[derive(Debug, Clone, PartialEq)]
struct ColliderDrag {
    entity: Entity,
    kind: ColliderHandleKind,
    old: Collider,
}

/// The collider tool: its toggle and the drag in progress.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct ColliderHandleTool {
    /// The viewport shows the primary selection's collider grips instead of its gizmo.
    pub enabled: bool,
    drag: Option<ColliderDrag>,
}

impl ColliderHandleTool {
    /// Whether a drag is in progress.
    #[must_use]
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }
}

/// Whether the tool has grips for `entity`: it is on and the entity has a collider it may edit.
#[must_use]
pub fn tool_shows(world: &World, entity: Entity) -> bool {
    world
        .get_resource::<ColliderHandleTool>()
        .is_some_and(|tool| tool.enabled)
        && world.get::<Collider>(entity).is_some()
        && world.get::<regia_scene::prelude::FeedOwned>(entity).is_none()
}

/// Starts a drag of the grip `kind` on `entity`. Returns `false` when it has no collider (the drag
/// does not start).
pub fn begin_drag(world: &mut World, entity: Entity, kind: ColliderHandleKind) -> bool {
    let Some(old) = world.get::<Collider>(entity).cloned() else {
        return false;
    };
    world
        .get_resource_or_insert_with(ColliderHandleTool::default)
        .drag = Some(ColliderDrag { entity, kind, old });
    true
}

/// Moves the dragged grip to the world point `point`, previewing on the entity. Returns whether the
/// collider changed.
pub fn update_drag(world: &mut World, point: Vec3) -> bool {
    let Some(drag) = world
        .get_resource::<ColliderHandleTool>()
        .and_then(|tool| tool.drag.clone())
    else {
        return false;
    };
    let body = body_pose(world, drag.entity);
    let Some(current) = world.get::<Collider>(drag.entity).cloned() else {
        return false;
    };
    let Some(next) = apply_handle(&current, body, drag.kind, point) else {
        return false;
    };
    if next == current {
        return false;
    }
    world.entity_mut(drag.entity).insert(next);
    true
}

/// Ends the drag: pushes one undo entry when the collider ended different from where it started.
/// Returns whether it did.
pub fn end_drag(world: &mut World) -> bool {
    let Some(drag) = world
        .get_resource_mut::<ColliderHandleTool>()
        .and_then(|mut tool| tool.drag.take())
    else {
        return false;
    };
    let Some(new) = world.get::<Collider>(drag.entity).cloned() else {
        return false;
    };
    if new == drag.old {
        return false;
    }
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(SetCollider {
            entity: drag.entity.to_bits(),
            old: drag.old,
            new,
        }));
    true
}

/// Draws the primary selection's collider and its grips over `rect` (viewport pane `pane`, whose
/// drag state is its own: each pane has its own camera) and runs their drags. Returns whether the grips own the pointer this frame (one is hovered or dragged), so the viewport does
/// not also box-select or pick.
pub fn viewport_input(
    ui: &mut egui::Ui,
    world: &mut World,
    view: &ViewCamera,
    rect: egui::Rect,
    pane: usize,
) -> bool {
    if crate::blockout::viewport_input(ui, world, view, rect, pane) {
        return true;
    }
    let _span = tracing::info_span!(target: "regia_editor", "collider_handles").entered();
    let primary = world.get_resource::<Selection>().and_then(Selection::primary);
    // The grips went away under a drag (the tool or the selection changed): what the drag previewed
    // stays one undo step instead of an edit nothing can undo.
    let dragged = world
        .get_resource::<ColliderHandleTool>()
        .and_then(|tool| tool.drag.as_ref().map(|drag| drag.entity));
    if dragged.is_some_and(|dragged| Some(dragged) != primary || !tool_shows(world, dragged)) {
        end_drag(world);
    }
    let Some(entity) = primary else {
        return false;
    };
    if !tool_shows(world, entity) {
        return false;
    }
    let body = body_pose(world, entity);
    let Some(collider) = world.get::<Collider>(entity).cloned() else {
        return false;
    };
    let size = regia_core::math::Vec2::new(rect.width(), rect.height());
    let painter = ui.painter_at(rect);
    let stroke = egui::Stroke::new(1.5, OUTLINE_COLOR);
    for (a, b) in outline_segments(&collider, body) {
        if let (Some(a), Some(b)) = (view.project(a, size), view.project(b, size)) {
            painter.line_segment(
                [
                    egui::pos2(rect.min.x + a.x, rect.min.y + a.y),
                    egui::pos2(rect.min.x + b.x, rect.min.y + b.y),
                ],
                stroke,
            );
        }
    }
    let grips = handles_for(&collider, body);
    let shapes: Vec<Handle> = grips.iter().map(|g| g.handle).collect();
    let id = ui.id().with(("collider-handles", entity.to_bits(), pane));
    let Some(event) = crate::handles::handles(ui, view, rect, id, &shapes, &SnapSet::empty()) else {
        return false;
    };
    match event.phase {
        DragPhase::Hover => {}
        DragPhase::Begin => {
            if let Some(grip) = grips.get(event.handle) {
                begin_drag(world, entity, grip.kind);
            }
        }
        DragPhase::Drag => {
            update_drag(world, event.position);
        }
        DragPhase::End => {
            update_drag(world, event.position);
            end_drag(world);
        }
    }
    true
}

// ---------------------------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------------------------

fn shape_name(shape: &ColliderShape) -> &'static str {
    match shape {
        ColliderShape::Box { .. } => "box",
        ColliderShape::Sphere { .. } => "sphere",
        ColliderShape::Capsule { .. } => "capsule",
        _ => "other",
    }
}

fn shape_json(collider: &Collider) -> Value {
    let mut out = json!({
        "shape": shape_name(&collider.shape),
        "offset": collider.offset.translation.to_array(),
    });
    match collider.shape {
        ColliderShape::Box { half_extents } => out["half_extents"] = json!(half_extents.to_array()),
        ColliderShape::Sphere { radius } => out["radius"] = json!(radius),
        ColliderShape::Capsule {
            half_height, radius, ..
        } => {
            out["half_height"] = json!(half_height);
            out["radius"] = json!(radius);
        }
        _ => {}
    }
    out
}

fn refused(reason: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, reason, message, hint)
}

fn collider_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = crate::undo_agent::live_entity(world, params, "entity")?;
    if world.get::<Collider>(entity).is_none() {
        return Err(refused(
            "collider_missing",
            "the entity has no Collider",
            "name an entity with a Collider component",
        ));
    }
    Ok(entity)
}

/// `editor.collider_handles`.
fn collider_handles_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(on) = params.get("enabled").and_then(Value::as_bool) {
        world
            .get_resource_or_insert_with(ColliderHandleTool::default)
            .enabled = on;
    }
    let enabled = world
        .get_resource::<ColliderHandleTool>()
        .is_some_and(|tool| tool.enabled);
    if params.get("entity").is_none() {
        return Ok(json!({ "enabled": enabled }));
    }
    let entity = collider_entity(world, params)?;
    let collider = world.get::<Collider>(entity).cloned().unwrap_or_default();
    let handles: Vec<Value> = handles_for(&collider, body_pose(world, entity))
        .iter()
        .map(|grip| json!({ "name": grip.kind.name(), "position": grip.handle.anchor().to_array() }))
        .collect();
    Ok(json!({
        "enabled": enabled,
        "entity": entity.to_bits(),
        "collider": shape_json(&collider),
        "handles": handles,
    }))
}

/// `editor.collider_handle_drag`.
fn collider_handle_drag_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = collider_entity(world, params)?;
    if world.get::<regia_scene::prelude::FeedOwned>(entity).is_some() {
        return Err(refused(
            "collider_feed_owned",
            "the DCC feed owns this entity",
            "edit it in the DCC tool",
        ));
    }
    let name = params.get("handle").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params("`handle` (a name from editor.collider_handles) is required")
    })?;
    let collider = world.get::<Collider>(entity).cloned().unwrap_or_default();
    let body = body_pose(world, entity);
    let grip = ColliderHandleKind::parse(name)
        .and_then(|kind| handles_for(&collider, body).into_iter().find(|g| g.kind == kind))
        .ok_or_else(|| {
            refused(
                "collider_handle_unknown",
                format!(
                    "this {} collider has no handle {name:?}",
                    shape_name(&collider.shape)
                ),
                "list them with editor.collider_handles",
            )
        })?;
    let to = crate::handles::vec3_param(params, "to")?;
    if !to.is_finite() {
        return Err(RpcError::invalid_params("`to` must be finite"));
    }
    let steps = params
        .get("steps")
        .and_then(Value::as_u64)
        .map_or(4, |s| s.clamp(1, 240)) as u32;
    let start = grip.handle.anchor();
    begin_drag(world, entity, grip.kind);
    for step in 1..=steps {
        update_drag(world, start.lerp(to, step as f32 / steps as f32));
    }
    let changed = end_drag(world);
    let collider = world.get::<Collider>(entity).cloned().unwrap_or_default();
    Ok(json!({
        "entity": entity.to_bits(),
        "changed": changed,
        "collider": shape_json(&collider),
    }))
}

/// The collider handle agent methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.collider_handles",
                tool: "collider_handles",
                description: "Lists a collider's handles with their world positions (box faces, sphere and capsule radii, capsule ends, the offset), and reads or sets the viewport's collider tool.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: false,
                        description: "Entity with a Collider. Omitted: only the tool state.",
                    },
                    ParamSchema {
                        name: "enabled",
                        kind: "boolean",
                        required: false,
                        description: "Turn the viewport's collider tool on or off.",
                    },
                ],
                result: "{ enabled, entity?, collider?: { shape, offset, half_extents?, radius?, half_height? }, handles?: [{ name, position }] }",
                mutating: false,
                permission: Permission::Read,
            },
            collider_handles_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.collider_handle_drag",
                tool: "collider_handle_drag",
                description: "Drags a collider handle to a world point through the viewport's drag code: the size changes about the shape's center (the offset handle moves the shape), and the whole drag is one undo step.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: true,
                        description: "Entity with a Collider.",
                    },
                    ParamSchema {
                        name: "handle",
                        kind: "string",
                        required: true,
                        description: "Handle name: extent_x+, extent_y-, radius_x, half_height+, offset, ...",
                    },
                    ParamSchema {
                        name: "to",
                        kind: "array",
                        required: true,
                        description: "[x, y, z] world point the handle ends on.",
                    },
                    ParamSchema {
                        name: "steps",
                        kind: "number",
                        required: false,
                        description: "Intermediate positions of the drag (default 4, cap 240).",
                    },
                ],
                result: "{ entity, changed, collider: { shape, offset, half_extents?, radius?, half_height? } }",
                mutating: true,
                permission: Permission::EditScene,
            },
            collider_handle_drag_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    use regia_core::math::Quat;

    #[test]
    fn handle_names_round_trip() {
        for kind in [
            ColliderHandleKind::Extent {
                axis: 2,
                positive: false,
            },
            ColliderHandleKind::Extent {
                axis: 0,
                positive: true,
            },
            ColliderHandleKind::Radius { axis: 1 },
            ColliderHandleKind::HalfHeight { positive: true },
            ColliderHandleKind::HalfHeight { positive: false },
            ColliderHandleKind::Offset,
        ] {
            assert_eq!(ColliderHandleKind::parse(&kind.name()), Some(kind));
        }
        for bad in ["", "extent_w+", "extent_x", "radius_", "half_height", "OFFSET"] {
            assert_eq!(ColliderHandleKind::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_grip_of_another_shape_does_nothing() {
        let collider = Collider {
            shape: ColliderShape::Sphere { radius: 1.0 },
            ..Collider::default()
        };
        let kind = ColliderHandleKind::HalfHeight { positive: true };
        assert_eq!(apply_handle(&collider, Isometry::IDENTITY, kind, Vec3::Y), None);
        let radius = ColliderHandleKind::Radius { axis: 0 };
        assert_eq!(
            apply_handle(&collider, Isometry::IDENTITY, radius, Vec3::NAN),
            None
        );
    }

    #[test]
    fn sizes_are_measured_in_the_shape_frame_and_clamped() {
        // A box turned 90 degrees about Y on a body at x = 10: its local +X points at world -Z.
        let collider = Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::ONE,
            },
            ..Collider::default()
        };
        let body = Isometry::new(
            Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
            Vec3::new(10.0, 0.0, 0.0),
        );
        let kind = ColliderHandleKind::Extent {
            axis: 0,
            positive: true,
        };
        let next = apply_handle(&collider, body, kind, Vec3::new(10.0, 0.0, -3.0)).unwrap();
        let ColliderShape::Box { half_extents } = next.shape else {
            panic!("box");
        };
        assert!((half_extents.x - 3.0).abs() < 1e-4, "{half_extents}");
        assert_eq!(half_extents.y, 1.0);
        // Past the center on the other side: the smallest size.
        let flat = apply_handle(&collider, body, kind, Vec3::new(10.0, 0.0, 5.0)).unwrap();
        let ColliderShape::Box { half_extents } = flat.shape else {
            panic!("box");
        };
        assert_eq!(half_extents.x, MIN_COLLIDER_SIZE);
    }

    #[test]
    fn the_capsule_long_axis_decides_the_handles() {
        let collider = Collider {
            shape: ColliderShape::Capsule {
                half_height: 1.0,
                radius: 0.5,
                axis: ShapeAxis::X,
            },
            ..Collider::default()
        };
        let handles = handles_for(&collider, Isometry::IDENTITY);
        let tip = handles
            .iter()
            .find(|g| g.kind == ColliderHandleKind::HalfHeight { positive: true })
            .unwrap()
            .handle
            .anchor();
        // ShapeAxis::X turns +Y onto +X.
        assert!((tip - Vec3::new(1.5, 0.0, 0.0)).length() < 1e-4, "{tip}");
        let next = apply_handle(
            &collider,
            Isometry::IDENTITY,
            ColliderHandleKind::HalfHeight { positive: true },
            Vec3::new(3.5, 0.0, 0.0),
        )
        .unwrap();
        let ColliderShape::Capsule { half_height, .. } = next.shape else {
            panic!("capsule");
        };
        assert!((half_height - 3.0).abs() < 1e-4);
    }
}
