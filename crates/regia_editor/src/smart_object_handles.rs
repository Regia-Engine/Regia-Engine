// SPDX-License-Identifier: MPL-2.0
//! Slot and entrance handles of the open smart-object definition in the viewport
//! (`docs/specs/39-smart-objects-v2.md` § 14 "viewport gizmo placement of definition slots",
//! `docs/specs/19-editor.md` § 3, ADR-7736).
//!
//! When the definition editor ([`crate::smart_object_def_panel`]) has a definition open and the
//! primary selection is its preview entity, or a level object whose `SmartObject::definition` names
//! the open file, every slot of the definition shows a grab point at its pose in that object's frame
//! and a yaw dial about its own up axis; every entrance shows the same in its slot's frame. A drag
//! previews live on the open definition and pushes one undo step when the pointer lets go, as the
//! collider grips do ([`begin_drag`], [`update_move`], [`update_turn`], [`end_drag`]). A moved
//! handle's world point is taken back into the parent's space, so a world delta becomes the
//! parent-space delta of the offset. The math is free functions ([`handles_for`], [`moved`],
//! [`turned`]) tested without a viewport.
//!
//! The handles replace the transform gizmo on that entity while they show; the panel's "Slot
//! handles" switch turns them off. Agent surface: `editor.smart_object_slot_handles` and
//! `editor.smart_object_slot_handle_drag` (`book/agents/smart-object-editor.md`).

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ai::smart_object::SmartObject;
use regia_ai::smart_object_def::{OffsetDef, SmartObjectDef};
use regia_core::math::{Affine3A, Quat, Vec3};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::command::UndoStack;
use crate::handles::{DragPhase, Handle, SnapSet};
use crate::hierarchy_ops::world_affine;
use crate::selection::Selection;
use crate::smart_object_def_panel::{SmartObjectDefEdit, SmartObjectDefPanel};
use crate::viewport_agent::Handler;
use crate::views::ViewCamera;

/// Radius of a yaw dial, metres (in the object's world scale).
pub const YAW_RADIUS: f32 = 0.35;
/// Length of a slot's drawn facing ray (its local −Z), metres.
const FACING: f32 = 0.5;
const SLOT_COLOR: egui::Color32 = egui::Color32::from_rgb(120, 200, 255);
const ENTRANCE_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 200, 90);

/// What a handle edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlotHandleKind {
    /// The slot's position.
    Slot {
        /// Slot index.
        slot: usize,
    },
    /// The slot's turn about its own up axis.
    SlotYaw {
        /// Slot index.
        slot: usize,
    },
    /// An entrance's position.
    Entrance {
        /// Slot index.
        slot: usize,
        /// Entrance index within the slot.
        entrance: usize,
    },
    /// An entrance's turn about its own up axis.
    EntranceYaw {
        /// Slot index.
        slot: usize,
        /// Entrance index within the slot.
        entrance: usize,
    },
}

impl SlotHandleKind {
    /// The wire name: `slot_0`, `slot_0_yaw`, `entrance_0_1`, `entrance_0_1_yaw`.
    #[must_use]
    pub fn name(self) -> String {
        match self {
            Self::Slot { slot } => format!("slot_{slot}"),
            Self::SlotYaw { slot } => format!("slot_{slot}_yaw"),
            Self::Entrance { slot, entrance } => format!("entrance_{slot}_{entrance}"),
            Self::EntranceYaw { slot, entrance } => format!("entrance_{slot}_{entrance}_yaw"),
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (body, yaw) = match text.strip_suffix("_yaw") {
            Some(body) => (body, true),
            None => (text, false),
        };
        let number = |s: &str| -> Option<usize> {
            (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                .then(|| s.parse().ok())
                .flatten()
        };
        if let Some(rest) = body.strip_prefix("slot_") {
            let slot = number(rest)?;
            return Some(if yaw {
                Self::SlotYaw { slot }
            } else {
                Self::Slot { slot }
            });
        }
        let rest = body.strip_prefix("entrance_")?;
        let (slot, entrance) = rest.split_once('_')?;
        let (slot, entrance) = (number(slot)?, number(entrance)?);
        Some(if yaw {
            Self::EntranceYaw { slot, entrance }
        } else {
            Self::Entrance { slot, entrance }
        })
    }

    /// Whether the handle turns rather than moves.
    #[must_use]
    pub fn is_yaw(self) -> bool {
        matches!(self, Self::SlotYaw { .. } | Self::EntranceYaw { .. })
    }
}

/// One handle: what it edits and where it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlotGrip {
    /// What it edits.
    pub kind: SlotHandleKind,
    /// The handle, world space.
    pub handle: Handle,
}

/// `q` normalized, or the identity when it cannot be (zero or not finite).
fn unit_rotation(q: Quat) -> Quat {
    let length_squared = q.length_squared();
    if length_squared.is_finite() && length_squared > 1e-12 {
        q.normalize()
    } else {
        Quat::IDENTITY
    }
}

/// An offset as an affine (its rotation normalized; one that cannot be normalized counts as none,
/// which the validator reports).
#[must_use]
pub fn offset_affine(offset: &OffsetDef) -> Affine3A {
    let (x, y, z) = offset.translation;
    let (qx, qy, qz, qw) = offset.rotation;
    let rotation = unit_rotation(Quat::from_xyzw(qx, qy, qz, qw));
    Affine3A::from_rotation_translation(rotation, Vec3::new(x, y, z))
}

/// Slot `slot`'s frame in the world, for an object at `object`.
#[must_use]
pub fn slot_frame(def: &SmartObjectDef, object: Affine3A, slot: usize) -> Option<Affine3A> {
    def.slots.get(slot).map(|s| object * offset_affine(&s.offset))
}

/// Entrance `entrance` of slot `slot`'s frame in the world.
#[must_use]
pub fn entrance_frame(
    def: &SmartObjectDef,
    object: Affine3A,
    slot: usize,
    entrance: usize,
) -> Option<Affine3A> {
    let frame = slot_frame(def, object, slot)?;
    def.slots[slot]
        .entrances
        .get(entrance)
        .map(|e| frame * offset_affine(&e.offset))
}

fn dial(frame: Affine3A) -> Handle {
    Handle::Arc {
        center: Vec3::from(frame.translation),
        axis: frame.transform_vector3(Vec3::Y).normalize_or(Vec3::Y),
        from: -std::f32::consts::PI,
        to: std::f32::consts::PI,
        radius: YAW_RADIUS,
    }
}

/// The handles of `def` on an object at `object`: every grab point first (the widget gives ties to
/// the earlier handle), then the dials.
#[must_use]
pub fn handles_for(def: &SmartObjectDef, object: Affine3A) -> Vec<SlotGrip> {
    let mut points = Vec::new();
    let mut dials = Vec::new();
    for (i, slot) in def.slots.iter().enumerate() {
        let frame = object * offset_affine(&slot.offset);
        points.push(SlotGrip {
            kind: SlotHandleKind::Slot { slot: i },
            handle: Handle::Point {
                pos: Vec3::from(frame.translation),
            },
        });
        dials.push(SlotGrip {
            kind: SlotHandleKind::SlotYaw { slot: i },
            handle: dial(frame),
        });
        for (j, entrance) in slot.entrances.iter().enumerate() {
            let at = frame * offset_affine(&entrance.offset);
            points.push(SlotGrip {
                kind: SlotHandleKind::Entrance { slot: i, entrance: j },
                handle: Handle::Point {
                    pos: Vec3::from(at.translation),
                },
            });
            dials.push(SlotGrip {
                kind: SlotHandleKind::EntranceYaw { slot: i, entrance: j },
                handle: dial(at),
            });
        }
    }
    points.extend(dials);
    points
}

/// `def` with the position handle `kind` moved to the world point `point`, for an object at
/// `object`: the point is taken into the parent's space (the object's for a slot, the slot's for an
/// entrance). `None` for a dial, a missing slot or entrance, or a point that is not finite there.
#[must_use]
pub fn moved(
    def: &SmartObjectDef,
    object: Affine3A,
    kind: SlotHandleKind,
    point: Vec3,
) -> Option<SmartObjectDef> {
    if !point.is_finite() {
        return None;
    }
    let mut next = def.clone();
    let (parent, target) = match kind {
        SlotHandleKind::Slot { slot } => (object, &mut next.slots.get_mut(slot)?.offset),
        SlotHandleKind::Entrance { slot, entrance } => (
            slot_frame(def, object, slot)?,
            &mut next.slots.get_mut(slot)?.entrances.get_mut(entrance)?.offset,
        ),
        SlotHandleKind::SlotYaw { .. } | SlotHandleKind::EntranceYaw { .. } => return None,
    };
    let local = parent.inverse().transform_point3(point);
    if !local.is_finite() {
        return None;
    }
    target.translation = (local.x, local.y, local.z);
    Some(next)
}

/// `def` with the dial `kind` turned by `radians` about the slot's or entrance's own up axis.
/// `None` for a position handle or a missing slot or entrance.
#[must_use]
pub fn turned(def: &SmartObjectDef, kind: SlotHandleKind, radians: f32) -> Option<SmartObjectDef> {
    if !radians.is_finite() {
        return None;
    }
    let mut next = def.clone();
    let target = match kind {
        SlotHandleKind::SlotYaw { slot } => &mut next.slots.get_mut(slot)?.offset,
        SlotHandleKind::EntranceYaw { slot, entrance } => {
            &mut next.slots.get_mut(slot)?.entrances.get_mut(entrance)?.offset
        }
        SlotHandleKind::Slot { .. } | SlotHandleKind::Entrance { .. } => return None,
    };
    let (x, y, z, w) = target.rotation;
    let current = unit_rotation(Quat::from_xyzw(x, y, z, w));
    let q = (current * Quat::from_rotation_y(radians)).normalize();
    target.rotation = (q.x, q.y, q.z, q.w);
    Some(next)
}

fn plain(path: &str) -> &str {
    let path = path.trim_start_matches("./");
    path.strip_prefix("assets/").unwrap_or(path)
}

/// Whether `entity`'s `SmartObject::definition` names the file `path` (through the asset server's
/// manifest or the loaded definitions' paths).
fn names_file(world: &World, entity: Entity, path: &str) -> bool {
    let Some(id) = world
        .get::<SmartObject>(entity)
        .and_then(|o| o.definition.as_ref())
        .map(|r| r.id)
    else {
        return false;
    };
    let from_server = world
        .get_resource::<regia_asset::AssetServer>()
        .and_then(|server| server.path_for_id(id).map(str::to_owned));
    let from_defs = world
        .get_resource::<regia_ai::smart_object_load::SmartObjectDefs>()
        .and_then(|defs| defs.slot(id))
        .and_then(|slot| slot.path.clone());
    [from_server, from_defs]
        .iter()
        .flatten()
        .any(|named| plain(named) == plain(path))
}

/// Whether the slot handles show on `entity`: a definition is open, the handles are on, and the
/// entity is the preview entity or an object naming the open file (and the DCC feed does not own it).
#[must_use]
pub fn handles_show(world: &World, entity: Entity) -> bool {
    let Some(panel) = world.get_resource::<SmartObjectDefPanel>() else {
        return false;
    };
    let Some(doc) = panel.doc() else {
        return false;
    };
    !panel.handles_off
        && world.get_entity(entity).is_ok()
        && world.get::<regia_scene::prelude::FeedOwned>(entity).is_none()
        && (panel.preview == Some(entity) || names_file(world, entity, &doc.path))
}

/// A handle drag in progress.
#[derive(Debug, Clone, PartialEq)]
struct ActiveDrag {
    entity: Entity,
    kind: SlotHandleKind,
    before: SmartObjectDef,
}

/// The drag in progress (a resource of the edit world).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct SlotHandleDrag {
    drag: Option<ActiveDrag>,
}

impl SlotHandleDrag {
    /// Whether a drag is in progress.
    #[must_use]
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }
}

fn open_def(world: &World) -> Option<SmartObjectDef> {
    world
        .get_resource::<SmartObjectDefPanel>()
        .and_then(SmartObjectDefPanel::doc)
        .map(|d| d.def.clone())
}

fn set_def(world: &mut World, def: SmartObjectDef) {
    if let Some(mut panel) = world.get_resource_mut::<SmartObjectDefPanel>()
        && let Some(doc) = panel.doc_mut()
    {
        doc.def = def;
    }
}

/// Starts a drag of `kind` on `entity`. `false` when no definition is open.
pub fn begin_drag(world: &mut World, entity: Entity, kind: SlotHandleKind) -> bool {
    let Some(before) = open_def(world) else {
        return false;
    };
    world.get_resource_or_insert_with(SlotHandleDrag::default).drag =
        Some(ActiveDrag { entity, kind, before });
    true
}

fn active(world: &World) -> Option<ActiveDrag> {
    world
        .get_resource::<SlotHandleDrag>()
        .and_then(|d| d.drag.clone())
}

/// Moves the dragged position handle to the world point `point`, previewing on the open
/// definition. Returns whether it changed.
pub fn update_move(world: &mut World, point: Vec3) -> bool {
    let (Some(drag), Some(current)) = (active(world), open_def(world)) else {
        return false;
    };
    let object = world_affine(world, drag.entity);
    match moved(&current, object, drag.kind, point) {
        Some(next) if next != current => {
            set_def(world, next);
            true
        }
        _ => false,
    }
}

/// Turns the dragged dial by `radians`, previewing on the open definition. Returns whether it changed.
pub fn update_turn(world: &mut World, radians: f32) -> bool {
    let (Some(drag), Some(current)) = (active(world), open_def(world)) else {
        return false;
    };
    match turned(&current, drag.kind, radians) {
        Some(next) if next != current => {
            set_def(world, next);
            true
        }
        _ => false,
    }
}

/// Ends the drag: pushes one undo step when the definition ended different from where it started.
/// Returns whether it did.
pub fn end_drag(world: &mut World) -> bool {
    let Some(drag) = world
        .get_resource_mut::<SlotHandleDrag>()
        .and_then(|mut d| d.drag.take())
    else {
        return false;
    };
    let Some(after) = open_def(world) else {
        return false;
    };
    if after == drag.before {
        return false;
    }
    let label = if drag.kind.is_yaw() {
        "turn slot handle"
    } else {
        "move slot handle"
    };
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(SmartObjectDefEdit::single(drag.before, after, label)));
    true
}

/// Draws the primary selection's slot and entrance handles over `rect` (viewport pane `pane`) and
/// runs their drags. Returns whether the handles own the pointer this frame (one is hovered or
/// dragged), so the viewport does not also box-select or pick.
pub fn viewport_input(
    ui: &mut egui::Ui,
    world: &mut World,
    view: &ViewCamera,
    rect: egui::Rect,
    pane: usize,
) -> bool {
    let _span = tracing::info_span!(target: "regia_editor", "smart_object_handles").entered();
    let primary = world.get_resource::<Selection>().and_then(Selection::primary);
    // The handles went away under a drag (the selection, the switch or the open file changed): what
    // the drag previewed stays one undo step.
    if let Some(drag) = active(world)
        && (Some(drag.entity) != primary || !handles_show(world, drag.entity))
    {
        end_drag(world);
    }
    let Some(entity) = primary else {
        return false;
    };
    if !handles_show(world, entity) {
        return false;
    }
    let Some(def) = open_def(world) else {
        return false;
    };
    let object = world_affine(world, entity);
    let size = regia_core::math::Vec2::new(rect.width(), rect.height());
    let painter = ui.painter_at(rect);
    let line = |a: Vec3, b: Vec3, color: egui::Color32| {
        if let (Some(a), Some(b)) = (view.project(a, size), view.project(b, size)) {
            painter.line_segment(
                [
                    egui::pos2(rect.min.x + a.x, rect.min.y + a.y),
                    egui::pos2(rect.min.x + b.x, rect.min.y + b.y),
                ],
                egui::Stroke::new(1.5, color),
            );
        }
    };
    for (i, slot) in def.slots.iter().enumerate() {
        let Some(frame) = slot_frame(&def, object, i) else {
            continue;
        };
        let at = Vec3::from(frame.translation);
        let facing = frame.transform_vector3(Vec3::NEG_Z).normalize_or_zero() * FACING;
        line(at, at + facing, SLOT_COLOR);
        for j in 0..slot.entrances.len() {
            if let Some(e) = entrance_frame(&def, object, i, j) {
                line(Vec3::from(e.translation), at, ENTRANCE_COLOR);
            }
        }
    }
    let grips = handles_for(&def, object);
    let shapes: Vec<Handle> = grips.iter().map(|g| g.handle).collect();
    let id = ui
        .id()
        .with(("smart-object-slot-handles", entity.to_bits(), pane));
    let Some(event) = crate::handles::handles(ui, view, rect, id, &shapes, &SnapSet::empty()) else {
        return false;
    };
    let yaw = grips.get(event.handle).is_some_and(|g| g.kind.is_yaw());
    match event.phase {
        DragPhase::Hover => {}
        DragPhase::Begin => {
            if let Some(grip) = grips.get(event.handle) {
                begin_drag(world, entity, grip.kind);
            }
        }
        DragPhase::Drag | DragPhase::End => {
            if yaw {
                update_turn(world, event.angle_delta);
            } else {
                update_move(world, event.position);
            }
            if event.phase == DragPhase::End {
                end_drag(world);
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------------------------

const HINT: &str = "see book/agents/smart-object-editor.md";

fn refused(reason: &str, message: impl Into<String>) -> RpcError {
    crate::data_panels::PanelError::new(reason, message).rpc(HINT)
}

/// The entity the handles show on, from `entity`.
fn shown_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = crate::undo_agent::live_entity(world, params, "entity")?;
    if open_def(world).is_none() {
        return Err(refused(
            "smart_object_handles_no_definition",
            "no definition is open in the definition editor",
        ));
    }
    if !handles_show(world, entity) {
        return Err(refused(
            "smart_object_handles_not_shown",
            "the slot handles do not show on this entity: it is neither the preview entity nor an object \
             naming the open definition, or the handles are off",
        ));
    }
    Ok(entity)
}

/// `editor.smart_object_slot_handles`.
fn slot_handles_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(on) = params.get("enabled").and_then(Value::as_bool) {
        world
            .get_resource_or_insert_with(SmartObjectDefPanel::default)
            .handles_off = !on;
    }
    match params.get("preview") {
        None => {}
        Some(Value::Null) => {
            world
                .get_resource_or_insert_with(SmartObjectDefPanel::default)
                .preview = None;
        }
        Some(_) => {
            let entity = crate::undo_agent::live_entity(world, params, "preview")?;
            world
                .get_resource_or_insert_with(SmartObjectDefPanel::default)
                .preview = Some(entity);
        }
    }
    let panel = world.get_resource::<SmartObjectDefPanel>();
    let mut out = json!({
        "enabled": panel.is_none_or(|p| !p.handles_off),
        "preview": panel.and_then(|p| p.preview).map(Entity::to_bits),
        "open": panel.and_then(SmartObjectDefPanel::doc).is_some(),
    });
    if params.get("entity").is_none() {
        return Ok(out);
    }
    let entity = shown_entity(world, params)?;
    let def = open_def(world).unwrap_or_default();
    let handles: Vec<Value> = handles_for(&def, world_affine(world, entity))
        .iter()
        .map(|grip| json!({ "name": grip.kind.name(), "position": grip.handle.anchor().to_array() }))
        .collect();
    out["entity"] = json!(entity.to_bits());
    out["handles"] = json!(handles);
    Ok(out)
}

/// `editor.smart_object_slot_handle_drag`.
fn slot_handle_drag_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = shown_entity(world, params)?;
    let name = params.get("handle").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params("`handle` (a name from editor.smart_object_slot_handles) is required")
    })?;
    let def = open_def(world).unwrap_or_default();
    let object = world_affine(world, entity);
    let grip = SlotHandleKind::parse(name)
        .and_then(|kind| handles_for(&def, object).into_iter().find(|g| g.kind == kind))
        .ok_or_else(|| {
            refused(
                "smart_object_handle_unknown",
                format!("the open definition has no handle {name:?}"),
            )
        })?;
    let steps = params
        .get("steps")
        .and_then(Value::as_u64)
        .map_or(4, |s| s.clamp(1, 240)) as u32;
    if grip.kind.is_yaw() {
        let angle = params
            .get("angle")
            .and_then(Value::as_f64)
            .filter(|a| a.is_finite())
            .ok_or_else(|| RpcError::invalid_params("a dial needs `angle` (radians, finite)"))?
            as f32;
        begin_drag(world, entity, grip.kind);
        for _ in 0..steps {
            update_turn(world, angle / steps as f32);
        }
    } else {
        let to = crate::handles::vec3_param(params, "to")?;
        if !to.is_finite() {
            return Err(RpcError::invalid_params("`to` must be finite"));
        }
        let start = grip.handle.anchor();
        begin_drag(world, entity, grip.kind);
        for step in 1..=steps {
            update_move(world, start.lerp(to, step as f32 / steps as f32));
        }
    }
    let changed = end_drag(world);
    let mut out = crate::smart_object_def_panel::doc_json(world);
    out["entity"] = json!(entity.to_bits());
    out["changed"] = json!(changed);
    out["undo_depth"] = json!(world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth));
    Ok(out)
}

/// The slot handle agent methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.smart_object_slot_handles",
                tool: "smart_object_slot_handles",
                description: "Lists the viewport handles of the open smart-object definition's slots and entrances on \
                    an entity (positions and yaw dials, world space), and reads or sets the handle switch and the \
                    definition editor's preview entity.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: false,
                        description: "The preview entity or an object naming the open definition. Omitted: only the state.",
                    },
                    ParamSchema {
                        name: "enabled",
                        kind: "boolean",
                        required: false,
                        description: "Turn the slot handles on or off.",
                    },
                    ParamSchema {
                        name: "preview",
                        kind: "number",
                        required: false,
                        description: "The entity the open definition's handles show on (null clears it).",
                    },
                ],
                result: "{ enabled, preview, open, entity?, handles?: [{ name, position }] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            slot_handles_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.smart_object_slot_handle_drag",
                tool: "smart_object_slot_handle_drag",
                description: "Drags a slot or entrance handle through the viewport's drag code: a position handle to a \
                    world point (taken into the slot's parent space), a dial by an angle about the slot's or \
                    entrance's up axis. The whole drag is one undo step on the open definition.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: true,
                        description: "The preview entity or an object naming the open definition.",
                    },
                    ParamSchema {
                        name: "handle",
                        kind: "string",
                        required: true,
                        description: "Handle name: slot_0, slot_0_yaw, entrance_0_1, entrance_0_1_yaw.",
                    },
                    ParamSchema {
                        name: "to",
                        kind: "array",
                        required: false,
                        description: "Position handles: [x, y, z] world point the handle ends on.",
                    },
                    ParamSchema {
                        name: "angle",
                        kind: "number",
                        required: false,
                        description: "Dials: radians to turn (counter-clockwise looking down the up axis).",
                    },
                    ParamSchema {
                        name: "steps",
                        kind: "number",
                        required: false,
                        description: "Intermediate positions of the drag (default 4, cap 240).",
                    },
                ],
                result: "editor.smart_object_def_state's result plus { entity, changed }",
                mutating: true,
                permission: Permission::Assets,
            },
            slot_handle_drag_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;
