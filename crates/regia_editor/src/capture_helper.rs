// SPDX-License-Identifier: MPL-2.0
//! The viewport helper of a selected reflection capture (`docs/specs/19-editor.md` § 3
//! "reflection probe bounds"; ADR-2860, ADR-7592): its influence box (the parallax box the
//! forward pass reads, `LocalCapture::half_extent` around the entity's world position) in
//! [`CAPTURE_BOX`], and its blend margin (the box grown by `LocalCapture::fade`, where the
//! capture fades out) dashed in [`CAPTURE_MARGIN`].
//!
//! The boxes are axis aligned and unscaled, as the runtime places them (only the entity's world
//! position moves a capture). They draw with the selection's other helpers, under the viewport's
//! **Show ▸ Bounds** flag (`editor.set_viewport { bounds }`), so a frame with the flag off is the
//! no-helper control. Hidden entities draw none.
//!
//! **Drag handles** (ADR-7663): while the viewport's handle tool is on (the collider tool's
//! toggle, [`crate::collider_handles::ColliderHandleTool`]), the primary selection's capture
//! shows grips instead of its transform gizmo, through the same handle widget the collider grips
//! (trigger volumes) use: one on each face of the influence box (`extent_x+` … `extent_z-`,
//! changes `half_extent` on that axis about the center) and one arrow outside each face of the
//! blend margin pointing in (`margin_x+` … `margin_z-`, changes `fade`, the margin is the same
//! on every side). A drag previews on the entity and pushes one [`SetLocalCapture`] undo entry
//! when it ends ([`begin_drag`], [`update_drag`], [`end_drag`]); a begin that egui's second pass
//! of a frame repeats keeps the drag it started. An entity with both a collider and a capture
//! shows its collider grips. The agent route is `editor.capture_handles` and
//! `editor.capture_handle_drag` (`book/agents/reflection-captures.md`), which run the same
//! begin, update and end.

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_render::LocalCapture;
use serde_json::{Value, json};

use crate::command::{EditorCommand, UndoStack, resolve_entity};
use crate::handles::{DragPhase, Handle, SnapSet};
use crate::selection::Selection;
use crate::viewport_agent::Handler;
use crate::views::ViewCamera;

/// The influence box's color.
pub const CAPTURE_BOX: egui::Color32 = egui::Color32::from_rgb(255, 200, 40);
/// The blend margin's color (dashed).
pub const CAPTURE_MARGIN: egui::Color32 = egui::Color32::from_rgb(255, 140, 20);

/// The twelve edges of the axis-aligned box `center ± half`.
#[must_use]
pub fn box_edges(center: Vec3, half: Vec3) -> [(Vec3, Vec3); 12] {
    let corner = |i: usize| {
        center
            + Vec3::new(
                if i & 1 == 0 { -half.x } else { half.x },
                if i & 2 == 0 { -half.y } else { half.y },
                if i & 4 == 0 { -half.z } else { half.z },
            )
    };
    let pairs = [
        (0, 1),
        (2, 3),
        (4, 5),
        (6, 7),
        (0, 2),
        (1, 3),
        (4, 6),
        (5, 7),
        (0, 4),
        (1, 5),
        (2, 6),
        (3, 7),
    ];
    pairs.map(|(a, b)| (corner(a), corner(b)))
}

/// The world-space segments of every selected capture's helper: the influence box's edges, then
/// the blend margin's edges cut in six with every other piece kept (a zero fade draws no margin).
#[must_use]
pub fn capture_helper_segments(world: &World) -> Vec<(Vec3, Vec3, egui::Color32)> {
    let _span = tracing::trace_span!("editor_capture_helper_segments").entered();
    let mut out = Vec::new();
    let Some(selection) = world.get_resource::<Selection>() else {
        return out;
    };
    for &entity in selection.entities() {
        let Some(capture) = world.get::<LocalCapture>(entity) else {
            continue;
        };
        if crate::visibility::is_hidden(world, entity) {
            continue;
        }
        let Some(center) = crate::viewport::world_position(world, entity) else {
            continue;
        };
        let half = Vec3::from_array(capture.half_extent).max(Vec3::ZERO);
        for (a, b) in box_edges(center, half) {
            out.push((a, b, CAPTURE_BOX));
        }
        if capture.fade.is_finite() && capture.fade > 0.0 {
            for (a, b) in box_edges(center, half + Vec3::splat(capture.fade)) {
                for piece in (0..6).step_by(2) {
                    let t0 = piece as f32 / 6.0;
                    let t1 = (piece + 1) as f32 / 6.0;
                    out.push((a.lerp(b, t0), a.lerp(b, t1), CAPTURE_MARGIN));
                }
            }
        }
    }
    out
}

/// The smallest half extent a box grip leaves, meters (`LocalCapture` needs one above zero).
pub const MIN_CAPTURE_HALF_EXTENT: f32 = 1.0e-2;
/// The part of the way from the center to a face where a box grip starts.
const GRIP_START: f32 = 0.75;
/// The shortest grip, meters.
const MIN_GRIP: f32 = 0.1;

/// What a capture grip edits. Axes are 0, 1, 2 for world X, Y, Z (the boxes are axis aligned).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureHandleKind {
    /// The influence box's half extent on `axis`, from the face on the positive or negative side.
    Extent {
        /// The axis.
        axis: u8,
        /// The face on the positive side.
        positive: bool,
    },
    /// The blend margin (`fade`), from outside the face on `axis`'s positive or negative side.
    Margin {
        /// The axis.
        axis: u8,
        /// The face on the positive side.
        positive: bool,
    },
}

const AXIS_NAMES: [&str; 3] = ["x", "y", "z"];

impl CaptureHandleKind {
    /// The wire name: `extent_x+`, `extent_y-`, `margin_z+`, ...
    #[must_use]
    pub fn name(self) -> String {
        let (part, axis, positive) = match self {
            Self::Extent { axis, positive } => ("extent", axis, positive),
            Self::Margin { axis, positive } => ("margin", axis, positive),
        };
        format!(
            "{part}_{}{}",
            AXIS_NAMES[usize::from(axis.min(2))],
            if positive { '+' } else { '-' }
        )
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (part, rest) = text.split_once('_')?;
        let (name, positive) = match (rest.strip_suffix('+'), rest.strip_suffix('-')) {
            (Some(name), _) => (name, true),
            (_, Some(name)) => (name, false),
            _ => return None,
        };
        let axis = AXIS_NAMES.iter().position(|a| *a == name)? as u8;
        match part {
            "extent" => Some(Self::Extent { axis, positive }),
            "margin" => Some(Self::Margin { axis, positive }),
            _ => None,
        }
    }
}

/// One grip: what it edits and where it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureGrip {
    /// What it edits.
    pub kind: CaptureHandleKind,
    /// The grip, world space.
    pub handle: Handle,
}

fn unit(axis: u8) -> Vec3 {
    match axis {
        0 => Vec3::X,
        1 => Vec3::Y,
        _ => Vec3::Z,
    }
}

/// The grips of `capture` at the world point `center`: the six box faces, then the six margin
/// arrows. The box grips come first, so where a margin of zero puts both tips on a face the box
/// grip is the one picked there (the margin arrow is still grabbed along its length outside).
#[must_use]
pub fn handles_for(capture: &LocalCapture, center: Vec3) -> Vec<CaptureGrip> {
    let mut out = Vec::with_capacity(12);
    let half = Vec3::from_array(capture.half_extent).max(Vec3::ZERO);
    let fade = if capture.fade.is_finite() {
        capture.fade.max(0.0)
    } else {
        0.0
    };
    for axis in 0..3_u8 {
        let h = half[usize::from(axis)];
        for positive in [true, false] {
            let dir = if positive { unit(axis) } else { -unit(axis) };
            out.push(CaptureGrip {
                kind: CaptureHandleKind::Extent { axis, positive },
                handle: Handle::Arrow {
                    origin: center + dir * (h * GRIP_START),
                    dir: dir * (h * (1.0 - GRIP_START)),
                },
            });
        }
    }
    for axis in 0..3_u8 {
        let h = half[usize::from(axis)];
        let length = (h * (1.0 - GRIP_START)).max(MIN_GRIP);
        for positive in [true, false] {
            let dir = if positive { unit(axis) } else { -unit(axis) };
            // Outside the margin, pointing in: its tip (the anchor a drag moves) is on the margin.
            out.push(CaptureGrip {
                kind: CaptureHandleKind::Margin { axis, positive },
                handle: Handle::Arrow {
                    origin: center + dir * (h + fade + length),
                    dir: -dir * length,
                },
            });
        }
    }
    out
}

/// `capture` with the grip `kind` moved to the world point `point` (`center` is the capture's
/// world position). A size is measured from the center along the grip's axis: a box grip sets
/// that half extent (never below [`MIN_CAPTURE_HALF_EXTENT`]), a margin grip sets `fade` to how
/// far the point is outside the box on that axis (never below 0). `None` when the point is not
/// finite.
#[must_use]
pub fn apply_handle(
    capture: &LocalCapture,
    center: Vec3,
    kind: CaptureHandleKind,
    point: Vec3,
) -> Option<LocalCapture> {
    if !point.is_finite() {
        return None;
    }
    let local = point - center;
    let mut next = *capture;
    match kind {
        CaptureHandleKind::Extent { axis, positive } => {
            let a = usize::from(axis.min(2));
            let along = if positive { local[a] } else { -local[a] };
            next.half_extent[a] = along.max(MIN_CAPTURE_HALF_EXTENT);
        }
        CaptureHandleKind::Margin { axis, positive } => {
            let a = usize::from(axis.min(2));
            let along = if positive { local[a] } else { -local[a] };
            next.fade = (along - capture.half_extent[a]).max(0.0);
        }
    }
    Some(next)
}

/// One capture handle edit: the whole capture before and after. Never merges, so a drag is one
/// undo entry.
pub struct SetLocalCapture {
    /// Target entity (bits).
    pub entity: u64,
    /// The capture before.
    pub old: LocalCapture,
    /// The capture after.
    pub new: LocalCapture,
}

impl SetLocalCapture {
    fn write(&self, world: &mut World, value: LocalCapture) {
        let Some(entity) = resolve_entity(world, self.entity) else {
            tracing::warn!(target: "regia_editor", "SetLocalCapture: entity {} is gone", self.entity);
            return;
        };
        world.entity_mut(entity).insert(value);
    }
}

impl EditorCommand for SetLocalCapture {
    fn apply(&mut self, world: &mut World) {
        self.write(world, self.new);
    }

    fn revert(&mut self, world: &mut World) {
        self.write(world, self.old);
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        "Reflection capture handle".to_owned()
    }
}

/// A capture grip drag in progress (an edit-world resource).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct CaptureHandleDrag {
    drag: Option<(Entity, CaptureHandleKind, LocalCapture)>,
}

impl CaptureHandleDrag {
    /// Whether a drag is in progress.
    #[must_use]
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }
}

/// Whether the viewport shows capture grips for `entity`: the handle tool is on, the entity has a
/// capture (and no collider, whose grips win), is not hidden and is not the DCC feed's.
#[must_use]
pub fn handles_show(world: &World, entity: Entity) -> bool {
    world
        .get_resource::<crate::collider_handles::ColliderHandleTool>()
        .is_some_and(|tool| tool.enabled)
        && world.get::<LocalCapture>(entity).is_some()
        && world.get::<regia::physics::Collider>(entity).is_none()
        && world.get::<regia_scene::prelude::FeedOwned>(entity).is_none()
        && !crate::visibility::is_hidden(world, entity)
}

/// `entity`'s world position, as the helper draws it.
fn center_of(world: &World, entity: Entity) -> Vec3 {
    crate::viewport::world_position(world, entity).unwrap_or_default()
}

/// Starts a drag of the grip `kind` on `entity`. Returns `false` when it has no capture. A begin
/// of the drag already in progress (egui's second pass of a frame) keeps it, so the undo entry
/// still holds the capture from before the drag.
pub fn begin_drag(world: &mut World, entity: Entity, kind: CaptureHandleKind) -> bool {
    let Some(old) = world.get::<LocalCapture>(entity).copied() else {
        return false;
    };
    let mut state = world.get_resource_or_insert_with(CaptureHandleDrag::default);
    if state
        .drag
        .as_ref()
        .is_some_and(|(e, k, _)| *e == entity && *k == kind)
    {
        return true;
    }
    state.drag = Some((entity, kind, old));
    true
}

/// Moves the dragged grip to the world point `point`, previewing on the entity. Returns whether
/// the capture changed.
pub fn update_drag(world: &mut World, point: Vec3) -> bool {
    let Some((entity, kind, _)) = world.get_resource::<CaptureHandleDrag>().and_then(|d| d.drag) else {
        return false;
    };
    let Some(current) = world.get::<LocalCapture>(entity).copied() else {
        return false;
    };
    let Some(next) = apply_handle(&current, center_of(world, entity), kind, point) else {
        return false;
    };
    if next == current {
        return false;
    }
    world.entity_mut(entity).insert(next);
    true
}

/// Ends the drag: pushes one undo entry when the capture ended different from where it started.
/// Returns whether it did (a second end in the same frame finds no drag and does nothing).
pub fn end_drag(world: &mut World) -> bool {
    let Some((entity, _, old)) = world
        .get_resource_mut::<CaptureHandleDrag>()
        .and_then(|mut d| d.drag.take())
    else {
        return false;
    };
    let Some(new) = world.get::<LocalCapture>(entity).copied() else {
        return false;
    };
    if new == old {
        return false;
    }
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(SetLocalCapture {
            entity: entity.to_bits(),
            old,
            new,
        }));
    true
}

/// Draws the primary selection's capture grips over `rect` (viewport pane `pane`) and runs their
/// drags. Returns whether the grips own the pointer this frame (one is hovered or dragged), so the
/// viewport does not also box-select or pick.
pub fn viewport_input(
    ui: &mut egui::Ui,
    world: &mut World,
    view: &ViewCamera,
    rect: egui::Rect,
    pane: usize,
) -> bool {
    let _span = tracing::info_span!(target: "regia_editor", "capture_handles").entered();
    let primary = world.get_resource::<Selection>().and_then(Selection::primary);
    // The grips went away under a drag (tool or selection changed): what the drag previewed stays
    // one undo step instead of an edit nothing can undo.
    let dragged = world
        .get_resource::<CaptureHandleDrag>()
        .and_then(|d| d.drag.map(|(e, _, _)| e));
    if dragged.is_some_and(|dragged| Some(dragged) != primary || !handles_show(world, dragged)) {
        end_drag(world);
    }
    let Some(entity) = primary else {
        return false;
    };
    if !handles_show(world, entity) {
        return false;
    }
    let Some(capture) = world.get::<LocalCapture>(entity).copied() else {
        return false;
    };
    let center = center_of(world, entity);
    let size = regia_core::math::Vec2::new(rect.width(), rect.height());
    let painter = ui.painter_at(rect);
    let half = Vec3::from_array(capture.half_extent).max(Vec3::ZERO);
    for (a, b) in box_edges(center, half) {
        if let (Some(a), Some(b)) = (view.project(a, size), view.project(b, size)) {
            painter.line_segment(
                [
                    egui::pos2(rect.min.x + a.x, rect.min.y + a.y),
                    egui::pos2(rect.min.x + b.x, rect.min.y + b.y),
                ],
                egui::Stroke::new(1.5, CAPTURE_BOX),
            );
        }
    }
    let grips = handles_for(&capture, center);
    let shapes: Vec<Handle> = grips.iter().map(|g| g.handle).collect();
    let id = ui.id().with(("capture-handles", entity.to_bits(), pane));
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

const HINT: &str = "see book/agents/reflection-captures.md";

/// `invalid_params` with the reason in `data.reason`, as the bake's refusals.
fn refused(reason: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code: INVALID_PARAMS,
        message: message.into(),
        data: Some(json!({ "code": "F0002", "reason": reason, "hint": HINT })),
    }
}

fn capture_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = crate::undo_agent::live_entity(world, params, "entity")?;
    if world.get::<LocalCapture>(entity).is_none() {
        return Err(refused(
            "capture_missing",
            "the entity has no LocalCapture (create one with Create ▸ Reflection capture)",
        ));
    }
    Ok(entity)
}

fn capture_json(capture: &LocalCapture) -> Value {
    json!({ "id": capture.id, "half_extent": capture.half_extent, "fade": capture.fade, "weight": capture.weight })
}

/// `editor.capture_handles`.
fn capture_handles_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(on) = params.get("enabled").and_then(Value::as_bool) {
        world
            .get_resource_or_insert_with(crate::collider_handles::ColliderHandleTool::default)
            .enabled = on;
    }
    let enabled = world
        .get_resource::<crate::collider_handles::ColliderHandleTool>()
        .is_some_and(|tool| tool.enabled);
    if params.get("entity").is_none_or(Value::is_null) {
        return Ok(json!({ "enabled": enabled }));
    }
    let entity = capture_entity(world, params)?;
    let capture = world.get::<LocalCapture>(entity).copied().unwrap_or_default();
    let center = center_of(world, entity);
    let handles: Vec<Value> = handles_for(&capture, center)
        .iter()
        .map(|grip| json!({ "name": grip.kind.name(), "position": grip.handle.anchor().to_array() }))
        .collect();
    Ok(json!({
        "enabled": enabled,
        "entity": entity.to_bits(),
        "shown": handles_show(world, entity),
        "center": center.to_array(),
        "capture": capture_json(&capture),
        "handles": handles,
    }))
}

/// `editor.capture_handle_drag`.
fn capture_handle_drag_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = capture_entity(world, params)?;
    if world.get::<regia_scene::prelude::FeedOwned>(entity).is_some() {
        return Err(refused("capture_feed_owned", "the DCC feed owns this entity"));
    }
    let name = params.get("handle").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params("`handle` (a name from editor.capture_handles) is required")
    })?;
    let kind = CaptureHandleKind::parse(name).ok_or_else(|| {
        refused(
            "capture_handle_unknown",
            format!(
                "a reflection capture has no handle {name:?}: extent_x+ … extent_z-, margin_x+ … margin_z-"
            ),
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
    let capture = world.get::<LocalCapture>(entity).copied().unwrap_or_default();
    let start = handles_for(&capture, center_of(world, entity))
        .into_iter()
        .find(|g| g.kind == kind)
        .map_or(to, |g| g.handle.anchor());
    // The viewport's drag: one begin, the pointer's updates, one end (one undo step).
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .seal();
    begin_drag(world, entity, kind);
    for step in 1..=steps {
        update_drag(world, start.lerp(to, step as f32 / steps as f32));
    }
    let changed = end_drag(world);
    let capture = world.get::<LocalCapture>(entity).copied().unwrap_or_default();
    Ok(json!({
        "entity": entity.to_bits(),
        "changed": changed,
        "capture": capture_json(&capture),
    }))
}

/// The capture handle agent methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.capture_handles",
                tool: "capture_handles",
                description: "Lists a reflection capture's viewport handles with their world positions (extent_x+ … \
                    extent_z- on the influence box faces, margin_x+ … margin_z- on the blend margin), and reads or \
                    sets the viewport's handle tool (shared with editor.collider_handles). Refusal: capture_missing.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: false,
                        description: "Entity with a LocalCapture. Omitted: only the tool state.",
                    },
                    ParamSchema {
                        name: "enabled",
                        kind: "boolean",
                        required: false,
                        description: "Turn the viewport's handle tool on or off.",
                    },
                ],
                result: "{ enabled, entity?, shown?, center?, capture?: { id, half_extent, fade, weight }, handles?: [{ name, position }] }",
                mutating: false,
                permission: Permission::Read,
            },
            capture_handles_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.capture_handle_drag",
                tool: "capture_handle_drag",
                description: "Drags a reflection capture handle to a world point through the viewport's drag code: an \
                    extent handle sets the box's half extent on its axis about the center, a margin handle sets the \
                    blend margin (fade); the whole drag is one undo step. Refusals: capture_missing, \
                    capture_handle_unknown, capture_feed_owned.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: true,
                        description: "Entity with a LocalCapture.",
                    },
                    ParamSchema {
                        name: "handle",
                        kind: "string",
                        required: true,
                        description: "Handle name: extent_x+, extent_y-, margin_z+, ...",
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
                result: "{ entity, changed, capture: { id, half_extent, fade, weight } }",
                mutating: true,
                permission: Permission::EditScene,
            },
            capture_handle_drag_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::SelectMode;
    use crate::views::{ShowFlags, ViewCamera, ViewKind};
    use regia_core::math::Vec2;

    fn world_with_capture(selected: bool) -> (World, Entity) {
        let mut world = World::new();
        let entity = world
            .spawn((
                regia_scene::prelude::Transform::from_translation(Vec3::new(1.0, 2.0, -3.0)),
                // A bare world runs no propagation: the world position the helper reads.
                regia_scene::GlobalTransform(regia_core::math::Affine3A::from_translation(Vec3::new(
                    1.0, 2.0, -3.0,
                ))),
                LocalCapture {
                    id: 4,
                    half_extent: [2.0, 1.0, 3.0],
                    weight: 1.0,
                    fade: 0.5,
                },
            ))
            .id();
        let mut selection = Selection::default();
        if selected {
            selection.select(entity, SelectMode::Replace);
        }
        world.insert_resource(selection);
        (world, entity)
    }

    #[test]
    fn capture_helper_draws_the_box_and_the_dashed_margin_of_a_selected_capture() {
        let (world, _) = world_with_capture(true);
        let segments = capture_helper_segments(&world);
        let boxes: Vec<_> = segments.iter().filter(|s| s.2 == CAPTURE_BOX).collect();
        let margin: Vec<_> = segments.iter().filter(|s| s.2 == CAPTURE_MARGIN).collect();
        assert_eq!(boxes.len(), 12);
        assert_eq!(margin.len(), 12 * 3, "every margin edge drawn in three dashes");
        let center = Vec3::new(1.0, 2.0, -3.0);
        // The box reaches exactly the half extents; the margin the half extents plus the fade.
        let reach = |set: &[&(Vec3, Vec3, egui::Color32)]| {
            set.iter()
                .flat_map(|s| [s.0, s.1])
                .fold(Vec3::ZERO, |m, p| m.max((p - center).abs()))
        };
        assert!(reach(&boxes).abs_diff_eq(Vec3::new(2.0, 1.0, 3.0), 1e-5));
        assert!(reach(&margin).abs_diff_eq(Vec3::new(2.5, 1.5, 3.5), 1e-5));
    }

    #[test]
    fn capture_helper_is_only_for_the_selection() {
        let (world, _) = world_with_capture(false);
        assert!(capture_helper_segments(&world).is_empty());
    }

    /// The overlays draw the helper under Show ▸ Bounds, and nothing of it with the flag off.
    #[test]
    fn capture_helper_overlay_follows_the_bounds_flag() {
        let (world, _) = world_with_capture(true);
        let camera = crate::viewport::EditorCamera {
            target: Vec3::new(1.0, 2.0, -3.0),
            ..Default::default()
        };
        let view = ViewCamera::of(ViewKind::Perspective, &camera);
        let size = Vec2::new(320.0, 240.0);
        let lines = |show: ShowFlags| {
            crate::overlays::build_overlays(&world, &view, ViewKind::Perspective, size, show)
                .into_iter()
                .filter(|shape| {
                    matches!(shape, crate::overlays::OverlayShape::Line { color, .. }
                        if *color == CAPTURE_BOX || *color == CAPTURE_MARGIN)
                })
                .count()
        };
        let off = ShowFlags {
            bounds: false,
            grid: false,
            ..ShowFlags::default()
        };
        let on = ShowFlags { bounds: true, ..off };
        assert_eq!(lines(off), 0);
        assert!(lines(on) >= 12, "the box shows with Bounds on: {}", lines(on));
    }

    const CENTER: Vec3 = Vec3::new(1.0, 2.0, -3.0);

    fn call(world: &mut World, name: &str, params: Value) -> Result<Value, RpcError> {
        let (_, handler) = methods()
            .into_iter()
            .find(|(schema, _)| schema.name == name)
            .unwrap_or_else(|| panic!("{name}"));
        handler(world, &params)
    }

    fn reason(err: &RpcError) -> Option<&str> {
        err.data.as_ref().and_then(|d| d["reason"].as_str())
    }

    fn tool_on(world: &mut World) {
        world
            .get_resource_or_insert_with(crate::collider_handles::ColliderHandleTool::default)
            .enabled = true;
    }

    #[test]
    fn capture_handle_names_round_trip() {
        for kind in handles_for(&LocalCapture::default(), Vec3::ZERO)
            .iter()
            .map(|g| g.kind)
        {
            assert_eq!(CaptureHandleKind::parse(&kind.name()), Some(kind));
        }
        assert_eq!(handles_for(&LocalCapture::default(), Vec3::ZERO).len(), 12);
        for bad in [
            "",
            "extent_x",
            "extent_w+",
            "margin_",
            "offset",
            "radius_x",
            "EXTENT_X+",
        ] {
            assert_eq!(CaptureHandleKind::parse(bad), None, "{bad}");
        }
    }

    /// The grips sit on the box faces and outside the margin, and a drag point sets the half
    /// extent (box) or the fade (margin) along the grip's axis, clamped.
    #[test]
    fn capture_grips_sit_on_the_faces_and_set_extent_or_fade() {
        let capture = LocalCapture {
            id: 4,
            half_extent: [2.0, 1.0, 3.0],
            weight: 1.0,
            fade: 0.5,
        };
        let grips = handles_for(&capture, CENTER);
        let anchor = |kind| {
            grips
                .iter()
                .find(|g| g.kind == kind)
                .map(|g| g.handle.anchor())
                .expect("grip")
        };
        let y_minus = CaptureHandleKind::Extent {
            axis: 1,
            positive: false,
        };
        assert!(anchor(y_minus).abs_diff_eq(CENTER - Vec3::Y, 1e-5));
        let z_margin = CaptureHandleKind::Margin {
            axis: 2,
            positive: true,
        };
        assert!(anchor(z_margin).abs_diff_eq(CENTER + Vec3::Z * 3.5, 1e-5));
        let wider = apply_handle(&capture, CENTER, y_minus, CENTER - Vec3::Y * 4.0).expect("edit");
        assert_eq!(wider.half_extent, [2.0, 4.0, 3.0]);
        assert_eq!(wider.fade, 0.5);
        let flat = apply_handle(&capture, CENTER, y_minus, CENTER + Vec3::Y).expect("edit");
        assert_eq!(flat.half_extent[1], MIN_CAPTURE_HALF_EXTENT);
        let margin = apply_handle(&capture, CENTER, z_margin, CENTER + Vec3::Z * 4.25).expect("edit");
        assert_eq!((margin.half_extent, margin.fade), (capture.half_extent, 1.25));
        let inside = apply_handle(&capture, CENTER, z_margin, CENTER + Vec3::Z).expect("edit");
        assert_eq!(inside.fade, 0.0, "a margin never goes inside the box");
        assert!(wider.desc(CENTER.to_array()).validate().is_ok());
        assert!(flat.desc(CENTER.to_array()).validate().is_ok());
        assert_eq!(apply_handle(&capture, CENTER, y_minus, Vec3::NAN), None);
    }

    /// A viewport drag (the handle widget's state machine, as `viewport_input` feeds it) of the
    /// +X box face is one undo step, and `editor.capture_handle_drag` to the same point gives the
    /// same capture: the agent route equals the handle.
    #[test]
    fn capture_handle_drag_matches_the_viewport_drag_as_one_undo_step() {
        use crate::handles::{HandleState, PointerInput};
        let (mut world, entity) = world_with_capture(true);
        tool_on(&mut world);
        assert!(handles_show(&world, entity));
        let start = *world.get::<LocalCapture>(entity).expect("capture");
        let camera = crate::viewport::EditorCamera {
            target: CENTER,
            ..Default::default()
        };
        let view = ViewCamera::of(ViewKind::Perspective, &camera);
        let size = Vec2::new(640.0, 480.0);
        let grips = handles_for(&start, CENTER);
        let kind = CaptureHandleKind::Extent {
            axis: 0,
            positive: true,
        };
        let index = grips.iter().position(|g| g.kind == kind).expect("grip");
        let shapes: Vec<Handle> = grips.iter().map(|g| g.handle).collect();
        let from = view.project(grips[index].handle.anchor(), size).expect("visible");
        let target = CENTER + Vec3::X * 3.0;
        let to = view.project(target, size).expect("visible");
        let mut state = HandleState::default();
        let none = SnapSet::empty();
        let input = |pos: Vec2, pressed: bool, down: bool| PointerInput {
            pos: Some(pos),
            pressed,
            down,
        };
        let radius = crate::handles::DEFAULT_PICK_RADIUS;
        let begin = state
            .update(&view, size, input(from, true, true), &shapes, &none, radius)
            .expect("grabbed");
        assert_eq!((begin.phase, begin.handle), (DragPhase::Begin, index));
        assert!(begin_drag(&mut world, entity, kind));
        for step in 1..=6 {
            let event = state
                .update(
                    &view,
                    size,
                    input(from.lerp(to, step as f32 / 6.0), false, true),
                    &shapes,
                    &none,
                    radius,
                )
                .expect("drag");
            update_drag(&mut world, event.position);
        }
        let end = state
            .update(&view, size, input(to, false, false), &shapes, &none, radius)
            .expect("end");
        assert_eq!(end.phase, DragPhase::End);
        update_drag(&mut world, end.position);
        assert!(end_drag(&mut world));
        let dragged = *world.get::<LocalCapture>(entity).expect("capture");
        assert!((dragged.half_extent[0] - 3.0).abs() < 1e-3, "{dragged:?}");
        assert_eq!(&dragged.half_extent[1..], &start.half_extent[1..]);
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1, "one undo step");
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(
            world.get::<LocalCapture>(entity),
            Some(&start),
            "undo restores the box"
        );

        let reply = call(
            &mut world,
            "editor.capture_handle_drag",
            json!({ "entity": entity.to_bits(), "handle": "extent_x+", "to": target.to_array(), "steps": 6 }),
        )
        .expect("agent drag");
        assert_eq!(reply["changed"], true);
        let agent = *world.get::<LocalCapture>(entity).expect("capture");
        assert!(
            (agent.half_extent[0] - dragged.half_extent[0]).abs() < 1e-3,
            "{agent:?} vs {dragged:?}"
        );
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            1,
            "the agent drag is one undo step"
        );
        // The margin through the agent: fade is how far outside the box the point is.
        call(
            &mut world,
            "editor.capture_handle_drag",
            json!({ "entity": entity.to_bits(), "handle": "margin_y-", "to": (CENTER - Vec3::Y * 2.5).to_array() }),
        )
        .expect("margin drag");
        let margin = *world.get::<LocalCapture>(entity).expect("capture");
        assert!((margin.fade - 1.5).abs() < 1e-5, "{margin:?}");
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(
            world.get::<LocalCapture>(entity).map(|c| c.fade),
            Some(start.fade)
        );
    }

    /// egui can run a frame's UI twice: a repeated begin keeps the drag (and its undo's "before")
    /// and a repeated end pushes nothing more.
    #[test]
    fn capture_handle_drag_survives_a_second_egui_pass() {
        let (mut world, entity) = world_with_capture(true);
        let start = *world.get::<LocalCapture>(entity).expect("capture");
        let kind = CaptureHandleKind::Margin {
            axis: 0,
            positive: true,
        };
        assert!(begin_drag(&mut world, entity, kind));
        assert!(update_drag(&mut world, CENTER + Vec3::X * 5.0));
        assert!(begin_drag(&mut world, entity, kind), "second pass");
        assert!(update_drag(&mut world, CENTER + Vec3::X * 6.0));
        assert!(end_drag(&mut world));
        assert!(!end_drag(&mut world), "second pass: no drag left");
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(
            world.get::<LocalCapture>(entity),
            Some(&start),
            "undo goes back to before the drag"
        );
    }

    /// The grips show only under the handle tool, for a capture without a collider, visible and not
    /// the DCC feed's; the agent lists the twelve handles and names its refusals.
    #[test]
    fn capture_handles_show_under_the_tool_and_refuse_with_reasons() {
        let (mut world, entity) = world_with_capture(true);
        assert!(!handles_show(&world, entity), "tool off");
        let reply = call(&mut world, "editor.capture_handles", json!({ "enabled": true })).expect("tool");
        assert_eq!(reply["enabled"], true);
        assert!(handles_show(&world, entity));
        let listed = call(
            &mut world,
            "editor.capture_handles",
            json!({ "entity": entity.to_bits() }),
        )
        .expect("list");
        assert_eq!(listed["handles"].as_array().map(Vec::len), Some(12), "{listed}");
        assert_eq!(listed["shown"], true);
        assert_eq!(listed["capture"]["id"], 4);
        world.entity_mut(entity).insert(crate::visibility::EditorHidden);
        assert!(!handles_show(&world, entity), "hidden");
        world
            .entity_mut(entity)
            .remove::<crate::visibility::EditorHidden>();
        world
            .entity_mut(entity)
            .insert(regia::physics::Collider::default());
        assert!(!handles_show(&world, entity), "the collider's grips win");
        let plain = world.spawn(regia_scene::prelude::Transform::default()).id();
        let err = call(
            &mut world,
            "editor.capture_handles",
            json!({ "entity": plain.to_bits() }),
        )
        .expect_err("none");
        assert_eq!(reason(&err), Some("capture_missing"));
        let err = call(
            &mut world,
            "editor.capture_handle_drag",
            json!({ "entity": entity.to_bits(), "handle": "radius_x", "to": [0.0, 0.0, 0.0] }),
        )
        .expect_err("unknown");
        assert_eq!(reason(&err), Some("capture_handle_unknown"));
        world.entity_mut(entity).insert(regia_scene::prelude::FeedOwned {
            key: "dcc".to_string(),
        });
        let err = call(
            &mut world,
            "editor.capture_handle_drag",
            json!({ "entity": entity.to_bits(), "handle": "extent_x+", "to": [0.0, 0.0, 0.0] }),
        )
        .expect_err("feed");
        assert_eq!(reason(&err), Some("capture_feed_owned"));
    }

    /// ADR-7663: the capture box and margin handles have an agent route, installed with the
    /// editor's methods (the dispatcher's list).
    #[test]
    fn capture_handle_drag_is_an_agent_route() {
        let names: Vec<&str> = crate::agent_ext::all_methods()
            .iter()
            .map(|(schema, _)| schema.name)
            .collect();
        for name in ["editor.capture_handles", "editor.capture_handle_drag"] {
            assert!(names.contains(&name), "{name} is installed");
        }
    }
}
