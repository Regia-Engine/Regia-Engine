// SPDX-License-Identifier: MPL-2.0
//! Multi-select inspector editing (ADR-3948, `docs/specs/19-editor.md` § 2).
//!
//! Equal fields show the sample value. Differing fields are mixed paths the drawers mark, and an
//! edit of one path is written onto every selected entity that has the component. Vector lanes,
//! Euler axes and color channels are separate paths, so editing one leaves the others as each
//! entity had them. One user action is one [`CommandGroup`] (or one
//! command, when a single non-prefab write is enough), so one undo restores every entity.
//!
//! A component present on only some of the selection stays editable on those entities. Add and
//! remove apply to every selected entity.

use std::collections::{BTreeMap, BTreeSet};

use glam::{
    BVec2, BVec3, BVec4, DVec2, DVec3, DVec4, IVec2, IVec3, IVec4, Quat, UVec2, UVec3, UVec4, Vec2, Vec3,
    Vec3A, Vec4,
};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_ecs::reflect::{
    AppTypeRegistry, GetPath, PartialReflect, ReflectComponent, ReflectPath, ReflectRef, TypeRegistry,
};
use regia_ecs::serialize::ReflectCodec;
use regia_scene::format::{SceneEntity, SceneFile};
use regia_scene::prefab::{
    BakedValue, OverrideEntry, PrefabInstance, PrefabLibrary, PrefabLink, PrefabTemplate,
};
use regia_scene::prelude::Transform;
use serde::de::DeserializeSeed as _;
use serde_json::{Value, json};

use super::reflect_ui::{self, Change, InspectorContext, InspectorUi};
use super::{euler_deg_to_quat, is_color_name, quat_to_euler_deg};
use crate::command::{
    AddComponent, CommandError, CommandGroup, EditorCommand, RemoveComponent, SetPrefabOverrides,
    SetProperty, SetTransform, UndoStack, read_property,
};
use crate::dnd::ContentKind;

/// How deep the mixed-value walk descends into nested reflected values.
const MIXED_DEPTH: usize = 8;
/// Component tolerance for matching a unit quaternion or its negation.
const QUAT_COMPONENT_EPS: f32 = 1e-5;
/// Euler degrees closer than this are the same axis.
const EULER_EPS: f32 = 0.01;

/// A multi-edit that did not run.
#[derive(Debug)]
pub struct EditError {
    /// Stable lowercase reason (`entity_and_entities`, `component_on_none`, …).
    pub code: &'static str,
    /// What went wrong, for logs and the protocol message.
    pub message: String,
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for EditError {}

impl EditError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// What a multi-edit wrote, and the undo depth afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditReport {
    /// Entities whose component was written, as entity bits.
    pub edited: Vec<u64>,
    /// Selected entities that do not have the component (left unchanged).
    pub skipped: Vec<u64>,
    /// Undo entries after the edit (unchanged when nothing was written).
    pub undo_depth: usize,
}

/// Header suffix when a component is on `present` of `selected` entities (`selected` > 1 and
/// `present` is in `1..selected`). `None` when it is on all of them, on none, or the selection
/// is a single entity.
#[must_use]
pub fn presence_suffix(present: usize, selected: usize) -> Option<String> {
    if selected > 1 && present > 0 && present < selected {
        Some(format!("on {present} of {selected}"))
    } else {
        None
    }
}

fn join_field(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}.{name}")
    }
}

fn join_index(path: &str, index: usize) -> String {
    format!("{path}[{index}]")
}

/// `.intensity` for a field path; empty stays empty (the whole component).
fn dot_path(path: &str) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!(".{path}")
    }
}

fn strip_dot(path: &str) -> &str {
    path.strip_prefix('.').unwrap_or(path)
}

fn same_rotation(left: &Quat, right: &Quat) -> bool {
    left.abs_diff_eq(*right, QUAT_COMPONENT_EPS) || left.abs_diff_eq(-*right, QUAT_COMPONENT_EPS)
}

fn same_value(left: &dyn PartialReflect, right: &dyn PartialReflect) -> bool {
    left.reflect_partial_eq(right) == Some(true)
}

/// A write that leaves the stored value as it is. Quaternions compare as rotations, so an Euler
/// round trip that flips the sign is not a new edit.
fn same_written(current: &dyn PartialReflect, next: &dyn PartialReflect) -> bool {
    if let (Some(left), Some(right)) = (
        current.try_downcast_ref::<Quat>(),
        next.try_downcast_ref::<Quat>(),
    ) {
        return same_rotation(left, right);
    }
    same_value(current, next)
}

fn f64_lanes(value: &dyn PartialReflect) -> Option<Vec<(&'static str, f64)>> {
    fn f32s(values: impl IntoIterator<Item = (&'static str, f32)>) -> Vec<(&'static str, f64)> {
        values
            .into_iter()
            .map(|(name, lane)| (name, f64::from(lane)))
            .collect()
    }
    fn i32s(values: impl IntoIterator<Item = (&'static str, i32)>) -> Vec<(&'static str, f64)> {
        values
            .into_iter()
            .map(|(name, lane)| (name, f64::from(lane)))
            .collect()
    }
    fn u32s(values: impl IntoIterator<Item = (&'static str, u32)>) -> Vec<(&'static str, f64)> {
        values
            .into_iter()
            .map(|(name, lane)| (name, f64::from(lane)))
            .collect()
    }
    if let Some(v) = value.try_downcast_ref::<Vec2>() {
        return Some(f32s([("x", v.x), ("y", v.y)]));
    }
    if let Some(v) = value.try_downcast_ref::<Vec3>() {
        return Some(f32s([("x", v.x), ("y", v.y), ("z", v.z)]));
    }
    if let Some(v) = value.try_downcast_ref::<Vec3A>() {
        return Some(f32s([("x", v.x), ("y", v.y), ("z", v.z)]));
    }
    if let Some(v) = value.try_downcast_ref::<Vec4>() {
        return Some(f32s([("x", v.x), ("y", v.y), ("z", v.z), ("w", v.w)]));
    }
    if let Some(v) = value.try_downcast_ref::<DVec2>() {
        return Some(vec![("x", v.x), ("y", v.y)]);
    }
    if let Some(v) = value.try_downcast_ref::<DVec3>() {
        return Some(vec![("x", v.x), ("y", v.y), ("z", v.z)]);
    }
    if let Some(v) = value.try_downcast_ref::<DVec4>() {
        return Some(vec![("x", v.x), ("y", v.y), ("z", v.z), ("w", v.w)]);
    }
    if let Some(v) = value.try_downcast_ref::<IVec2>() {
        return Some(i32s([("x", v.x), ("y", v.y)]));
    }
    if let Some(v) = value.try_downcast_ref::<IVec3>() {
        return Some(i32s([("x", v.x), ("y", v.y), ("z", v.z)]));
    }
    if let Some(v) = value.try_downcast_ref::<IVec4>() {
        return Some(i32s([("x", v.x), ("y", v.y), ("z", v.z), ("w", v.w)]));
    }
    if let Some(v) = value.try_downcast_ref::<UVec2>() {
        return Some(u32s([("x", v.x), ("y", v.y)]));
    }
    if let Some(v) = value.try_downcast_ref::<UVec3>() {
        return Some(u32s([("x", v.x), ("y", v.y), ("z", v.z)]));
    }
    if let Some(v) = value.try_downcast_ref::<UVec4>() {
        return Some(u32s([("x", v.x), ("y", v.y), ("z", v.z), ("w", v.w)]));
    }
    None
}

fn bool_lanes(value: &dyn PartialReflect) -> Option<Vec<(&'static str, bool)>> {
    if let Some(v) = value.try_downcast_ref::<BVec2>() {
        return Some(vec![("x", v.x), ("y", v.y)]);
    }
    if let Some(v) = value.try_downcast_ref::<BVec3>() {
        return Some(vec![("x", v.x), ("y", v.y), ("z", v.z)]);
    }
    if let Some(v) = value.try_downcast_ref::<BVec4>() {
        return Some(vec![("x", v.x), ("y", v.y), ("z", v.z), ("w", v.w)]);
    }
    None
}

fn mark_f32_array(path: &str, left: &[f32], right: &[f32], out: &mut BTreeSet<String>) {
    for (index, (a, b)) in left.iter().zip(right).enumerate() {
        if a != b {
            out.insert(join_index(path, index));
        }
    }
}

/// Reflect paths where `left` and `right` differ. Lanes of one vector, quaternion or color stay
/// independent: a difference in `x` does not mark `y`.
fn diff_into(
    left: &dyn PartialReflect,
    right: &dyn PartialReflect,
    path: &str,
    out: &mut BTreeSet<String>,
    depth: usize,
) {
    if depth > MIXED_DEPTH {
        if !same_value(left, right) {
            out.insert(path.to_string());
        }
        return;
    }
    if let (Some(a), Some(b)) = (left.try_downcast_ref::<Quat>(), right.try_downcast_ref::<Quat>()) {
        if same_rotation(a, b) {
            return;
        }
        let ea = quat_to_euler_deg(a);
        let eb = quat_to_euler_deg(b);
        for (index, lane) in ["rx", "ry", "rz"].iter().enumerate() {
            if (ea[index] - eb[index]).abs() > EULER_EPS {
                out.insert(join_field(path, lane));
            }
        }
        return;
    }
    if let (Some(a), Some(b)) = (f64_lanes(left), f64_lanes(right)) {
        if a.len() != b.len() {
            out.insert(path.to_string());
            return;
        }
        for ((name, av), (_, bv)) in a.iter().zip(b.iter()) {
            if av != bv {
                out.insert(join_field(path, name));
            }
        }
        return;
    }
    if let (Some(a), Some(b)) = (bool_lanes(left), bool_lanes(right)) {
        if a.len() != b.len() {
            out.insert(path.to_string());
            return;
        }
        for ((name, av), (_, bv)) in a.iter().zip(b.iter()) {
            if av != bv {
                out.insert(join_field(path, name));
            }
        }
        return;
    }
    if let (Some(a), Some(b)) = (
        left.try_downcast_ref::<[f32; 3]>(),
        right.try_downcast_ref::<[f32; 3]>(),
    ) {
        mark_f32_array(path, a, b, out);
        return;
    }
    if let (Some(a), Some(b)) = (
        left.try_downcast_ref::<[f32; 4]>(),
        right.try_downcast_ref::<[f32; 4]>(),
    ) {
        mark_f32_array(path, a, b, out);
        return;
    }
    match (left.reflect_ref(), right.reflect_ref()) {
        (ReflectRef::Struct(a), ReflectRef::Struct(b)) if a.field_len() == b.field_len() => {
            for index in 0..a.field_len() {
                let Some(name) = a.name_at(index) else {
                    continue;
                };
                match (a.field_at(index), b.field(name)) {
                    (Some(av), Some(bv)) => {
                        diff_into(av, bv, &join_field(path, name), out, depth + 1);
                    }
                    _ => {
                        out.insert(join_field(path, name));
                    }
                }
            }
        }
        (ReflectRef::TupleStruct(a), ReflectRef::TupleStruct(b)) if a.field_len() == b.field_len() => {
            for index in 0..a.field_len() {
                if let (Some(av), Some(bv)) = (a.field(index), b.field(index)) {
                    diff_into(av, bv, &join_field(path, &index.to_string()), out, depth + 1);
                }
            }
        }
        (ReflectRef::Tuple(a), ReflectRef::Tuple(b)) if a.field_len() == b.field_len() => {
            for index in 0..a.field_len() {
                if let (Some(av), Some(bv)) = (a.field(index), b.field(index)) {
                    diff_into(av, bv, &join_field(path, &index.to_string()), out, depth + 1);
                }
            }
        }
        (ReflectRef::Array(a), ReflectRef::Array(b)) => {
            if a.len() != b.len() {
                out.insert(path.to_string());
                return;
            }
            for index in 0..a.len() {
                if let (Some(av), Some(bv)) = (a.get(index), b.get(index)) {
                    diff_into(av, bv, &join_index(path, index), out, depth + 1);
                }
            }
        }
        (ReflectRef::List(a), ReflectRef::List(b)) => {
            if a.len() != b.len() {
                out.insert(path.to_string());
                return;
            }
            for index in 0..a.len() {
                if let (Some(av), Some(bv)) = (a.get(index), b.get(index)) {
                    diff_into(av, bv, &join_index(path, index), out, depth + 1);
                }
            }
        }
        (ReflectRef::Enum(a), ReflectRef::Enum(b)) => {
            if a.variant_name() != b.variant_name() || a.field_len() != b.field_len() {
                out.insert(path.to_string());
                return;
            }
            for index in 0..a.field_len() {
                // A tuple variant's fields have no names: they are `.0`, `.1`, ... as the
                // drawer and the reflect path name them (every field used to be `.0`).
                let position = index.to_string();
                let name = a.name_at(index).unwrap_or(&position);
                if let (Some(av), Some(bv)) = (a.field_at(index), b.field_at(index)) {
                    diff_into(av, bv, &join_field(path, name), out, depth + 1);
                }
            }
        }
        _ => {
            if !same_value(left, right) {
                out.insert(path.to_string());
            }
        }
    }
}

/// The edits an extension drawer made to a multi-selection's sample: one discrete change per
/// differing leaf path (`""` when the value differs as a whole). Corr editor-a finding 8: the
/// whole component used to be written onto every holder, so a one-field edit overwrote every
/// field that differed between the selected entities.
pub(crate) fn extension_changes(original: &dyn PartialReflect, edited: &dyn PartialReflect) -> Vec<Change> {
    let mut changed = BTreeSet::new();
    diff_into(original, edited, "", &mut changed, 0);
    changed
        .into_iter()
        .map(|path| Change { path, discrete: true })
        .collect()
}

fn mixed_paths(sample: &dyn PartialReflect, others: &[&dyn PartialReflect]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for other in others {
        diff_into(sample, *other, "", &mut out, 0);
    }
    out
}

fn reflect_json(value: &dyn PartialReflect, registry: &TypeRegistry) -> Value {
    let serializer = regia_ecs::reflect::serde::TypedReflectSerializer::new(value, registry);
    serde_json::to_value(&serializer).unwrap_or(Value::Null)
}

fn axis_json(name: &str, mixed: bool, value: Value) -> Value {
    json!({
        "axis": name,
        "mixed": mixed,
        "value": if mixed { Value::Null } else { value },
    })
}

fn field_json(path: &str, mixed: bool, value: Value, axes: Vec<Value>) -> Value {
    if axes.is_empty() {
        json!({ "path": path, "mixed": mixed, "value": if mixed { Value::Null } else { value } })
    } else {
        let any = mixed || axes.iter().any(|axis| axis["mixed"].as_bool() == Some(true));
        json!({
            "path": path,
            "mixed": any,
            "value": if any { Value::Null } else { value },
            "axes": axes,
        })
    }
}

fn color_axis_name(index: usize) -> &'static str {
    ["r", "g", "b", "a"][index]
}

fn push_f64_axes(
    path: &str,
    lanes: &[(&'static str, f64)],
    names: &[&str],
    mixed: &BTreeSet<String>,
    whole: &dyn PartialReflect,
    registry: &TypeRegistry,
    out: &mut Vec<Value>,
) {
    let axes: Vec<Value> = lanes
        .iter()
        .enumerate()
        .map(|(index, (lane, value))| {
            let reflect_path = join_field(path, lane);
            let shown = names.get(index).copied().unwrap_or(lane);
            axis_json(shown, mixed.contains(&reflect_path), json!(value))
        })
        .collect();
    out.push(field_json(
        path,
        mixed.contains(path),
        reflect_json(whole, registry),
        axes,
    ));
}

fn consider_field(
    path: &str,
    value: &dyn PartialReflect,
    color: bool,
    mixed: &BTreeSet<String>,
    registry: &TypeRegistry,
    out: &mut Vec<Value>,
) {
    if let Some(quat) = value.try_downcast_ref::<Quat>() {
        let euler = quat_to_euler_deg(quat);
        let axes: Vec<Value> = ["rx", "ry", "rz"]
            .iter()
            .enumerate()
            .map(|(index, lane)| {
                let reflect_path = join_field(path, lane);
                axis_json(lane, mixed.contains(&reflect_path), json!(euler[index]))
            })
            .collect();
        out.push(field_json(
            path,
            mixed.contains(path),
            reflect_json(value, registry),
            axes,
        ));
        return;
    }
    if let Some(lanes) = f64_lanes(value) {
        let names: Vec<&str> = if color {
            lanes
                .iter()
                .enumerate()
                .map(|(index, _)| color_axis_name(index))
                .collect()
        } else {
            lanes.iter().map(|(name, _)| *name).collect()
        };
        push_f64_axes(path, &lanes, &names, mixed, value, registry, out);
        return;
    }
    if let Some(lanes) = bool_lanes(value) {
        let axes: Vec<Value> = lanes
            .iter()
            .map(|(lane, bit)| axis_json(lane, mixed.contains(&join_field(path, lane)), json!(bit)))
            .collect();
        out.push(field_json(
            path,
            mixed.contains(path),
            reflect_json(value, registry),
            axes,
        ));
        return;
    }
    if let Some(channels) = value
        .try_downcast_ref::<[f32; 3]>()
        .map(|c| c.as_slice())
        .or_else(|| value.try_downcast_ref::<[f32; 4]>().map(|c| c.as_slice()))
    {
        let axes: Vec<Value> = channels
            .iter()
            .enumerate()
            .map(|(index, channel)| {
                axis_json(
                    color_axis_name(index),
                    mixed.contains(&join_index(path, index)),
                    json!(channel),
                )
            })
            .collect();
        out.push(field_json(
            path,
            mixed.contains(path),
            reflect_json(value, registry),
            axes,
        ));
        return;
    }
    match value.reflect_ref() {
        ReflectRef::Struct(fields) => {
            for index in 0..fields.field_len() {
                let Some(name) = fields.name_at(index) else {
                    continue;
                };
                let Some(field) = fields.field_at(index) else {
                    continue;
                };
                consider_field(
                    &join_field(path, name),
                    field,
                    is_color_name(name),
                    mixed,
                    registry,
                    out,
                );
            }
        }
        ReflectRef::Enum(variant) => {
            if mixed.contains(path) || variant.field_len() == 0 {
                out.push(field_json(
                    path,
                    mixed.contains(path),
                    reflect_json(value, registry),
                    Vec::new(),
                ));
            } else {
                for index in 0..variant.field_len() {
                    let position = index.to_string();
                    let name = variant.name_at(index).unwrap_or(&position);
                    let Some(field) = variant.field_at(index) else {
                        continue;
                    };
                    consider_field(
                        &join_field(path, name),
                        field,
                        is_color_name(name),
                        mixed,
                        registry,
                        out,
                    );
                }
            }
        }
        _ => out.push(field_json(
            path,
            mixed.contains(path),
            reflect_json(value, registry),
            Vec::new(),
        )),
    }
}

fn registration<'a>(
    registry: &'a TypeRegistry,
    name: &str,
) -> Option<&'a regia_ecs::reflect::TypeRegistration> {
    registry
        .get_with_type_path(name)
        .or_else(|| registry.get_with_short_type_path(name))
        .or_else(|| regia_core::brand::lookup_type_path(name, |path| registry.get_with_type_path(path)))
}

fn component_value(
    world: &World,
    registry: &TypeRegistry,
    entity: Entity,
    component: &str,
) -> Option<Box<dyn PartialReflect>> {
    let data = registration(registry, component)?.data::<ReflectComponent>()?;
    let entity_ref = world.get_entity(entity).ok()?;
    let value = data.reflect(entity_ref)?;
    reflect_ui::clone_concrete(registry, value)
}

fn has_component(world: &World, entity: Entity, component: &str) -> bool {
    read_property(world, entity, component, "").is_ok()
}

/// The entities that have `component`, in input order, each once (an entity listed twice is
/// one target: a second remove or add of it would fail after the first had applied).
fn holders_of(world: &World, entities: &[Entity], component: &str) -> Vec<Entity> {
    let mut seen = BTreeSet::new();
    entities
        .iter()
        .copied()
        .filter(|entity| seen.insert(*entity) && has_component(world, *entity, component))
        .collect()
}

/// Reverts `applied` (last first) after a later capture failed, so a refused structural edit
/// leaves the world as it was instead of half-applied with no undo entry.
fn roll_back(world: &mut World, applied: Vec<Box<dyn EditorCommand>>) {
    for mut command in applied.into_iter().rev() {
        command.revert(world);
    }
}

fn full_type_path(world: &World, component: &str) -> Option<String> {
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    registration(&registry, component).map(|found| found.type_info().type_path().to_string())
}

/// A stored override entry matches the edited component. A short spelling (an older file, or a
/// hand edit) names a component whose full path may carry a crate; two full paths match only
/// when they are the same path, so `a::Prop` and `b::Prop` keep their own override entries.
fn components_match(stored: &str, wanted: &str) -> bool {
    if stored == wanted {
        return true;
    }
    if !stored.contains("::") || !wanted.contains("::") {
        return short_name(stored) == short_name(wanted);
    }
    false
}

fn short_name(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

fn paths_match(stored: &str, wanted: &str) -> bool {
    strip_dot(stored) == strip_dot(wanted)
}

fn map_command(err: CommandError) -> EditError {
    match err {
        CommandError::NoSuchEntity(bits) => {
            EditError::new("not_found", format!("entity {bits} does not exist"))
        }
        CommandError::NoSuchComponent(name) => {
            EditError::new("component_on_none", format!("no reflected component {name}"))
        }
        CommandError::MissingComponent(_, name) => {
            EditError::new("component_on_none", format!("no {name} on the entity"))
        }
        CommandError::NoSuchPath(component, path) => {
            EditError::new("path_unknown", format!("no field {path} on {component}"))
        }
        CommandError::Reflect(_, message) => EditError::new("value_mismatch", message),
        CommandError::Scene(err) => EditError::new("prefab_apply_failed", err.to_string()),
    }
}

fn must_clone(value: &dyn PartialReflect) -> Result<Box<dyn PartialReflect>, EditError> {
    reflect_ui::clone_value(value)
        .ok_or_else(|| EditError::new("value_mismatch", "the value could not be cloned"))
}

fn as_f32(value: &dyn PartialReflect) -> Option<f32> {
    if let Some(number) = value.try_downcast_ref::<f32>() {
        return Some(*number);
    }
    value.try_downcast_ref::<f64>().map(|number| *number as f32)
}

fn euler_axis(path: &str) -> Option<(&str, usize)> {
    let (parent, lane) = match path.rsplit_once('.') {
        Some(split) => split,
        None => ("", path),
    };
    let axis = match lane {
        "rx" => 0,
        "ry" => 1,
        "rz" => 2,
        _ => return None,
    };
    Some((parent, axis))
}

fn read_quat(world: &World, entity: Entity, component: &str, parent: &str) -> Result<Quat, EditError> {
    let value = read_property(world, entity, component, parent).map_err(map_command)?;
    value
        .try_downcast_ref::<Quat>()
        .copied()
        .ok_or_else(|| EditError::new("path_unknown", format!("`{parent}` is not a quaternion")))
}

fn parent_is_quat(world: &World, entity: Entity, component: &str, parent: &str) -> bool {
    read_property(world, entity, component, parent)
        .ok()
        .is_some_and(|value| value.try_downcast_ref::<Quat>().is_some())
}

fn reflect_at<'a>(value: &'a dyn PartialReflect, path: &'a str) -> Result<&'a dyn PartialReflect, EditError> {
    if path.is_empty() {
        return Ok(value);
    }
    // `ReflectPath::reflect_element` ties the borrow to the path string.
    path.reflect_element(value)
        .map_err(|_| EditError::new("path_unknown", format!("no field {path}")))
}

fn reflect_at_mut<'a>(
    value: &'a mut dyn PartialReflect,
    path: &'a str,
) -> Result<&'a mut dyn PartialReflect, EditError> {
    if path.is_empty() {
        return Ok(value);
    }
    path.reflect_element_mut(value)
        .map_err(|_| EditError::new("path_unknown", format!("no field {path}")))
}

fn quat_at(value: &dyn PartialReflect, parent: &str) -> Option<Quat> {
    let field = reflect_at(value, parent).ok()?;
    field.try_downcast_ref::<Quat>().copied()
}

fn value_at(value: &dyn PartialReflect, path: &str) -> Result<Box<dyn PartialReflect>, EditError> {
    must_clone(reflect_at(value, path)?)
}

/// `translation.r` → `translation.x`, and `tint.b` → `tint[2]`, when the parent is a vector or a
/// color array. Other paths are unchanged.
fn rewrite_color_alias(world: &World, entity: Entity, component: &str, path: &str) -> String {
    let Some((parent, lane)) = path.rsplit_once('.') else {
        return path.to_string();
    };
    let index = match lane {
        "r" => 0,
        "g" => 1,
        "b" => 2,
        "a" => 3,
        _ => return path.to_string(),
    };
    let Ok(parent_value) = read_property(world, entity, component, parent) else {
        return path.to_string();
    };
    let vec_lanes = ["x", "y", "z", "w"];
    if parent_value.try_downcast_ref::<Vec3>().is_some() || parent_value.try_downcast_ref::<Vec3A>().is_some()
    {
        if index == 3 {
            return path.to_string();
        }
        return join_field(parent, vec_lanes[index]);
    }
    if parent_value.try_downcast_ref::<Vec4>().is_some() {
        return join_field(parent, vec_lanes[index]);
    }
    // `[f32; N]` has no `reflect_clone`, so `read_property` returns a dynamic array or list.
    if color_array_len(parent_value.as_ref()).is_some_and(|len| index < len) {
        return join_index(parent, index);
    }
    path.to_string()
}

/// Length of a color array, including the dynamic clone `read_property` returns for `[f32; N]`.
fn color_array_len(value: &dyn PartialReflect) -> Option<usize> {
    if value.try_downcast_ref::<[f32; 3]>().is_some() {
        return Some(3);
    }
    if value.try_downcast_ref::<[f32; 4]>().is_some() {
        return Some(4);
    }
    let (len, first) = match value.reflect_ref() {
        ReflectRef::Array(array) => (array.len(), array.get(0)),
        ReflectRef::List(list) => (list.len(), list.get(0)),
        _ => return None,
    };
    if (len == 3 || len == 4) && first.is_some_and(|item| item.try_downcast_ref::<f32>().is_some()) {
        Some(len)
    } else {
        None
    }
}

fn refuse_feed_owned(world: &World, entities: &[Entity]) -> Result<(), EditError> {
    for entity in entities {
        if let Err(err) = regia_agent::require_not_feed_owned(world, *entity) {
            return Err(EditError::new("feed_owned", err.message));
        }
    }
    Ok(())
}

fn require_alive(world: &World, entities: &[Entity]) -> Result<(), EditError> {
    for entity in entities {
        if world.get_entity(*entity).is_err() {
            return Err(EditError::new(
                "not_found",
                format!("entity {} does not exist", entity.to_bits()),
            ));
        }
    }
    Ok(())
}

struct Write {
    entity: Entity,
    path: String,
    value: Box<dyn PartialReflect>,
    coalesce: bool,
}

fn instance_root(world: &World, entity: Entity) -> Option<Entity> {
    world.get::<PrefabLink>(entity)?;
    let mut current = entity;
    for _ in 0..64 {
        let parent = world.get::<ChildOf>(current)?.parent();
        if world.get::<PrefabInstance>(parent).is_some() {
            return Some(parent);
        }
        current = parent;
    }
    None
}

fn value_ron(world: &World, value: &dyn PartialReflect) -> Result<String, EditError> {
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    ReflectCodec::new(&registry)
        .to_ron_typed(value)
        .map_err(|err| EditError::new("value_mismatch", err.to_string()))
}

fn upsert_override(
    overrides: &mut Vec<OverrideEntry>,
    target: u128,
    component: &str,
    path: &str,
    value: String,
) {
    if let Some(existing) = overrides.iter_mut().find(|entry| {
        entry.target == target
            && components_match(&entry.component, component)
            && paths_match(&entry.path, path)
    }) {
        existing.component = component.to_string();
        existing.path = path.to_string();
        existing.value = value;
    } else {
        overrides.push(OverrideEntry {
            target,
            component: component.to_string(),
            path: path.to_string(),
            value,
        });
    }
}

fn note_override(
    world: &mut World,
    entity: Entity,
    component: &str,
    path: &str,
    value: &dyn PartialReflect,
) -> Result<(), EditError> {
    let Some(link) = world.get::<PrefabLink>(entity).copied() else {
        return Ok(());
    };
    let Some(root) = instance_root(world, entity) else {
        return Ok(());
    };
    let ron = value_ron(world, value)?;
    let override_path = dot_path(path);
    let Some(mut instance) = world.get_mut::<PrefabInstance>(root) else {
        return Ok(());
    };
    upsert_override(
        &mut instance.overrides,
        link.local_id,
        component,
        &override_path,
        ron,
    );
    Ok(())
}

fn snapshot_roots(world: &World, entities: &[Entity]) -> BTreeMap<u64, Vec<OverrideEntry>> {
    let mut roots = BTreeMap::new();
    for entity in entities {
        let Some(root) = instance_root(world, *entity) else {
            continue;
        };
        roots.entry(root.to_bits()).or_insert_with(|| {
            world
                .get::<PrefabInstance>(root)
                .map(|instance| instance.overrides.clone())
                .unwrap_or_default()
        });
    }
    roots
}

fn undo_depth_of(world: &World) -> usize {
    world
        .get_resource::<UndoStack>()
        .map(UndoStack::undo_depth)
        .unwrap_or(0)
}

/// One frame of a continuous edit over several commands (the same field on several entities,
/// plus the prefab override lists it touches): applied in order, reverted in reverse order, like a
/// [`CommandGroup`]. Unlike a group it merges the next frame of the same drag, command by command,
/// so a drag over a multi-selection is one undo step. Corr editor-a finding 3: every frame used to
/// be its own entry, so one undo only stepped back one frame of the drag.
struct MultiFieldEdit {
    label: String,
    commands: Vec<Box<dyn EditorCommand>>,
}

impl MultiFieldEdit {
    /// The value `theirs` brings to a merge into `mine`, or `None` when the pair does not merge.
    fn merged_value(mine: &dyn EditorCommand, theirs: &dyn EditorCommand) -> Option<MergedValue> {
        use crate::command::downcast_to;
        if let (Some(a), Some(b)) = (
            downcast_to::<SetProperty>(mine),
            downcast_to::<SetProperty>(theirs),
        ) {
            let same = a.coalesce
                && b.coalesce
                && a.entity == b.entity
                && a.component == b.component
                && a.path == b.path;
            return same
                .then(|| must_clone(b.new.as_ref()).ok())
                .flatten()
                .map(MergedValue::Property);
        }
        if let (Some(a), Some(b)) = (
            downcast_to::<SetPrefabOverrides>(mine),
            downcast_to::<SetPrefabOverrides>(theirs),
        ) {
            return (a.entity == b.entity).then(|| MergedValue::Overrides(b.new.clone()));
        }
        None
    }
}

/// What one command of a [`MultiFieldEdit`] takes from the next frame.
enum MergedValue {
    Property(Box<dyn PartialReflect>),
    Overrides(Vec<regia_scene::prefab::OverrideEntry>),
}

impl EditorCommand for MultiFieldEdit {
    fn apply(&mut self, world: &mut World) {
        for command in &mut self.commands {
            command.apply(world);
        }
    }

    fn revert(&mut self, world: &mut World) {
        for command in self.commands.iter_mut().rev() {
            command.revert(world);
        }
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = crate::command::downcast_to::<MultiFieldEdit>(next) else {
            return false;
        };
        if next.commands.len() != self.commands.len() {
            return false;
        }
        // Every pair must merge before any is touched: a partial merge would leave the entry
        // reverting to a mix of frames.
        let mut values = Vec::with_capacity(self.commands.len());
        for (mine, theirs) in self.commands.iter().zip(&next.commands) {
            match Self::merged_value(mine.as_ref(), theirs.as_ref()) {
                Some(value) => values.push(value),
                None => return false,
            }
        }
        for (mine, value) in self.commands.iter_mut().zip(values) {
            let mine: &mut dyn std::any::Any = &mut **mine;
            match value {
                MergedValue::Property(new) => {
                    if let Some(command) = mine.downcast_mut::<SetProperty>() {
                        command.new = new;
                    }
                }
                MergedValue::Overrides(new) => {
                    if let Some(command) = mine.downcast_mut::<SetPrefabOverrides>() {
                        command.new = new;
                    }
                }
            }
        }
        true
    }

    fn description(&self) -> String {
        self.label.clone()
    }
}

fn push_applied(
    world: &mut World,
    commands: Vec<Box<dyn EditorCommand>>,
    label: String,
    discrete: bool,
) -> usize {
    if commands.is_empty() {
        return undo_depth_of(world);
    }
    let mut commands = commands.into_iter();
    let Some(first) = commands.next() else {
        return undo_depth_of(world);
    };
    let command: Box<dyn EditorCommand> = match commands.next() {
        None => first,
        // One frame of a continuous edit (a drag over several entities): the next frame of the
        // same drag merges into it, so the whole drag is one undo step, as for one entity.
        Some(second) if !discrete => {
            let mut all = vec![first, second];
            all.extend(commands);
            Box::new(MultiFieldEdit { label, commands: all })
        }
        Some(second) => {
            let mut group = CommandGroup::new(label);
            group.push(first);
            group.push(second);
            for command in commands {
                group.push(command);
            }
            Box::new(group)
        }
    };
    if discrete {
        crate::undo_agent::push_discrete(world, command)
    } else {
        let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
        stack.push(command);
        stack.undo_depth()
    }
}

fn commit_writes(
    world: &mut World,
    component: &str,
    full: &str,
    writes: Vec<Write>,
    discrete: bool,
) -> Result<EditReport, EditError> {
    if writes.is_empty() {
        return Ok(EditReport {
            edited: Vec::new(),
            skipped: Vec::new(),
            undo_depth: undo_depth_of(world),
        });
    }
    let edited: Vec<u64> = {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for write in &writes {
            if seen.insert(write.entity.to_bits()) {
                out.push(write.entity.to_bits());
            }
        }
        out
    };
    let entities: Vec<Entity> = writes.iter().map(|write| write.entity).collect();
    let before = snapshot_roots(world, &entities);
    let label = {
        let paths: BTreeSet<&str> = writes.iter().map(|write| write.path.as_str()).collect();
        if paths.len() == 1 {
            let path = paths.iter().next().copied().unwrap_or("");
            if path.is_empty() {
                format!("Edit {component}")
            } else {
                format!("Set {component}.{path}")
            }
        } else {
            "Edit selection".to_string()
        }
    };
    // Validate every holder and encode every prefab override before touching the world. An
    // enum field may exist only on some variants, or have a different type on another variant.
    let mut prepared = Vec::with_capacity(writes.len());
    for write in writes {
        let mut command = SetProperty::capture(world, write.entity, component, &write.path, write.value)
            .map_err(map_command)?;
        let mut trial = must_clone(command.old.as_ref())?;
        trial
            .try_apply(command.new.as_ref())
            .map_err(|err| EditError::new("value_mismatch", err.to_string()))?;
        command.coalesce = write.coalesce && !discrete;
        prepared.push((write.entity, command));
    }
    let mut after = before.clone();
    for (entity, command) in &prepared {
        let Some(link) = world.get::<PrefabLink>(*entity) else {
            continue;
        };
        let Some(root) = instance_root(world, *entity) else {
            continue;
        };
        let value = value_ron(world, command.new.as_ref())?;
        upsert_override(
            after.entry(root.to_bits()).or_default(),
            link.local_id,
            full,
            &dot_path(&command.path),
            value,
        );
    }
    let mut commands: Vec<Box<dyn EditorCommand>> = Vec::new();
    for (_, mut command) in prepared {
        command.apply(world);
        commands.push(Box::new(command));
    }
    for (bits, old) in before {
        let Some(entity) = crate::command::resolve_entity(world, bits) else {
            continue;
        };
        let new = after.remove(&bits).unwrap_or_default();
        if new != old {
            if let Some(mut instance) = world.get_mut::<PrefabInstance>(entity) {
                instance.overrides.clone_from(&new);
            }
            commands.push(Box::new(SetPrefabOverrides {
                entity: bits,
                old,
                new,
            }));
        }
    }
    let undo_depth = push_applied(world, commands, label, discrete);
    Ok(EditReport {
        edited,
        skipped: Vec::new(),
        undo_depth,
    })
}

/// Writes `new_value` at `path` on every entity in `entities` that has `component`.
///
/// One call is one undo entry. Vector and color lanes are ordinary reflect paths (`offset.x`,
/// `tint[2]`). `rotation.ry` is degrees about Y: each entity keeps its own X and Z. `.r` / `.g` /
/// `.b` / `.a` on a vector or a color array map to the reflect lane. An entity without the
/// component is skipped. A feed-owned target fails the whole call.
///
/// # Errors
/// [`EditError`] with `entities_empty`, `not_found`, `feed_owned`, `component_on_none`,
/// `path_unknown` or `value_mismatch`.
#[tracing::instrument(
    target = "regia_editor",
    skip(world, entities, new_value),
    name = "inspector_multi_edit"
)]
pub fn edit_value(
    world: &mut World,
    entities: &[Entity],
    component: &str,
    path: &str,
    new_value: Box<dyn PartialReflect>,
) -> Result<EditReport, EditError> {
    if entities.is_empty() {
        return Err(EditError::new("entities_empty", "entities is empty"));
    }
    require_alive(world, entities)?;
    let holders = holders_of(world, entities, component);
    if holders.is_empty() {
        return Err(EditError::new(
            "component_on_none",
            format!("none of the entities has {component}"),
        ));
    }
    refuse_feed_owned(world, &holders)?;
    let path = rewrite_color_alias(world, holders[0], component, path);
    let full = full_type_path(world, component).unwrap_or_else(|| component.to_string());
    let skipped = entities
        .iter()
        .filter(|entity| !holders.contains(entity))
        .map(|entity| entity.to_bits())
        .collect();
    if let Some((parent, axis)) = euler_axis(&path)
        && parent_is_quat(world, holders[0], component, parent)
    {
        let degrees = as_f32(new_value.as_ref())
            .ok_or_else(|| EditError::new("value_mismatch", "an Euler axis takes a number of degrees"))?;
        if !degrees.is_finite() {
            return Err(EditError::new(
                "value_mismatch",
                "an Euler axis takes a finite number of degrees",
            ));
        }
        let mut writes = Vec::with_capacity(holders.len());
        for entity in &holders {
            let current = read_quat(world, *entity, component, parent)?;
            let mut euler = quat_to_euler_deg(&current);
            euler[axis] = degrees;
            let rotation = euler_deg_to_quat(euler);
            if !rotation.is_finite() {
                return Err(EditError::new(
                    "value_mismatch",
                    "an Euler axis must stay a finite rotation",
                ));
            }
            writes.push(Write {
                entity: *entity,
                path: parent.to_string(),
                value: Box::new(rotation),
                coalesce: false,
            });
        }
        let mut report = commit_writes(world, component, &full, writes, true)?;
        report.skipped = skipped;
        return Ok(report);
    }
    let current = read_property(world, holders[0], component, &path).map_err(map_command)?;
    let mut trial = must_clone(current.as_ref())?;
    trial
        .try_apply(new_value.as_ref())
        .map_err(|err| EditError::new("value_mismatch", err.to_string()))?;
    let mut writes = Vec::with_capacity(holders.len());
    for entity in &holders {
        writes.push(Write {
            entity: *entity,
            path: path.clone(),
            value: must_clone(new_value.as_ref())?,
            coalesce: false,
        });
    }
    let mut report = commit_writes(world, component, &full, writes, true)?;
    report.skipped = skipped;
    Ok(report)
}

/// Whether a change names a synthetic lane of a quaternion in this value.
pub(super) fn has_euler_lane_changes(edited: &dyn PartialReflect, changes: &[Change]) -> bool {
    changes
        .iter()
        .any(|change| euler_axis(&change.path).is_some_and(|(parent, _)| quat_at(edited, parent).is_some()))
}

/// Applies one frame of drawer changes to every holder. Euler `.rx`/`.ry`/`.rz` patches that axis
/// of each entity's own quaternion; every other path is written as recorded.
///
/// With one holder, `edited` is that entity's own value: the axis is patched onto whichever of
/// its two Euler readings reproduces the edited quaternion, so a `ry` past 90 degrees survives.
/// With several, the others get the axis angle read back from the edited quaternion, which for
/// `ry` is always within -90..=90 degrees; the drawer passes the typed angle through
/// `commit_drawer_changes` instead.
pub fn commit_widget_changes(
    world: &mut World,
    entities: &[Entity],
    component: &str,
    edited: &dyn PartialReflect,
    changes: &[Change],
) -> Result<EditReport, EditError> {
    commit_drawer_changes(
        world,
        entities,
        component,
        edited,
        changes,
        None,
        &BTreeMap::new(),
    )
}

/// [`commit_widget_changes`] with what the drawer knows: `sample` is the entity whose value
/// `edited` was cloned from (its axis is patched onto the Euler reading the drawer showed), and
/// `lane_values` are the
/// Euler degrees the widget showed ([`InspectorContext::lane_values`]), patched onto every other
/// holder's own angles. Without them a typed `ry = 120` read back as `ry = 60` (two Euler readings
/// of one rotation) and was stored as 60 degrees.
pub(crate) fn commit_drawer_changes(
    world: &mut World,
    entities: &[Entity],
    component: &str,
    edited: &dyn PartialReflect,
    changes: &[Change],
    sample: Option<Entity>,
    lane_values: &BTreeMap<String, f32>,
) -> Result<EditReport, EditError> {
    commit_drawer_changes_linked(
        world,
        entities,
        component,
        edited,
        changes,
        sample,
        lane_values,
        &BTreeSet::new(),
    )
}

/// A lane value of the element type of `like` (`f32`, `f64`, `i32` or `u32`) holding `value`.
fn lane_box(like: &dyn PartialReflect, value: f64) -> Option<Box<dyn PartialReflect>> {
    if like.try_downcast_ref::<f32>().is_some() {
        return Some(Box::new(value as f32));
    }
    if like.try_downcast_ref::<f64>().is_some() {
        return Some(Box::new(value));
    }
    if like.try_downcast_ref::<i32>().is_some() {
        return Some(Box::new(
            value.round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32,
        ));
    }
    if like.try_downcast_ref::<u32>().is_some() {
        return Some(Box::new(value.round().clamp(0.0, f64::from(u32::MAX)) as u32));
    }
    None
}

/// The writes of a linked vector edit on every holder (ADR-8106): the edited lane takes the typed
/// value on each entity and that entity's other lanes follow by its own ratio
/// ([`super::impls::link_components`]), as the single-entity drawer does for one.
fn linked_writes(
    world: &World,
    holders: &[Entity],
    component: &str,
    parent: &str,
    edited_lane: &str,
    typed: f64,
) -> Result<Vec<Write>, EditError> {
    let mut writes = Vec::new();
    for entity in holders {
        let current = read_property(world, *entity, component, parent).map_err(map_command)?;
        let Some(lanes) = f64_lanes(current.as_ref()) else {
            continue;
        };
        let Some(edited) = lanes.iter().position(|(name, _)| *name == edited_lane) else {
            continue;
        };
        let before: Vec<f64> = lanes.iter().map(|(_, value)| *value).collect();
        let mut after = before.clone();
        after[edited] = typed;
        super::impls::link_components(&before, &mut after, edited);
        for ((name, was), next) in lanes.iter().zip(&after) {
            if next == was {
                continue;
            }
            let path = join_field(parent, name);
            let like = read_property(world, *entity, component, &path).map_err(map_command)?;
            let Some(value) = lane_box(like.as_ref(), *next) else {
                continue;
            };
            writes.push(Write {
                entity: *entity,
                path,
                value,
                coalesce: true,
            });
        }
    }
    Ok(writes)
}

/// [`commit_drawer_changes`] where `linked` holds the link keys (`Transform.scale`) of the vector
/// fields whose lanes are linked: their lane edits scale each holder's own vector
/// ([`linked_writes`]) in the same undo step as the frame's other edits.
#[expect(
    clippy::too_many_arguments,
    reason = "the drawer's commit inputs, passed through"
)]
pub(crate) fn commit_drawer_changes_linked(
    world: &mut World,
    entities: &[Entity],
    component: &str,
    edited: &dyn PartialReflect,
    changes: &[Change],
    sample: Option<Entity>,
    lane_values: &BTreeMap<String, f32>,
    linked: &BTreeSet<String>,
) -> Result<EditReport, EditError> {
    if changes.is_empty() {
        return Ok(EditReport {
            edited: Vec::new(),
            skipped: Vec::new(),
            undo_depth: undo_depth_of(world),
        });
    }
    let holders = holders_of(world, entities, component);
    refuse_feed_owned(world, &holders)?;
    let full = full_type_path(world, component).unwrap_or_else(|| component.to_string());
    // The entity `edited` came from: with one holder it can only be that one.
    let only = match holders.as_slice() {
        [only] => Some(*only),
        _ => None,
    };
    let sample = sample.filter(|entity| holders.contains(entity)).or(only);
    let mut euler: BTreeMap<String, [Option<f32>; 3]> = BTreeMap::new();
    let mut exact: BTreeMap<String, Quat> = BTreeMap::new();
    let mut plain: Vec<(String, Box<dyn PartialReflect>, bool)> = Vec::new();
    // Linked vectors: the first recorded lane of a linked field is the one the user edited.
    let short = short_name(component);
    let mut linked_edits: BTreeMap<String, (String, f64)> = BTreeMap::new();
    let mut linked_fields: BTreeSet<String> = BTreeSet::new();
    for change in changes {
        if let Some((parent, lane)) = change.path.rsplit_once('.')
            && linked.contains(&format!("{short}.{parent}"))
            && let Ok(vector) = value_at(edited, parent)
            && let Some(lanes) = f64_lanes(vector.as_ref())
            && let Some((_, typed)) = lanes.iter().find(|(name, _)| *name == lane)
        {
            linked_fields.insert(parent.to_owned());
            linked_edits
                .entry(parent.to_owned())
                .or_insert_with(|| (lane.to_owned(), *typed));
        }
    }
    for change in changes {
        if let Some((parent, _)) = change.path.rsplit_once('.')
            && linked_fields.contains(parent)
        {
            continue;
        }
        if let Some((parent, axis)) = euler_axis(&change.path)
            && let Some(quat) = quat_at(edited, parent)
        {
            let degrees = lane_values
                .get(&change.path)
                .copied()
                .unwrap_or_else(|| quat_to_euler_deg(&quat)[axis]);
            euler.entry(parent.to_string()).or_default()[axis] = Some(degrees);
            exact.insert(parent.to_string(), quat);
            continue;
        }
        match value_at(edited, &change.path) {
            Ok(value) => plain.push((change.path.clone(), value, !change.discrete)),
            Err(err) => {
                tracing::warn!(target: "regia_editor", "inspector edit: {err}");
            }
        }
    }
    let mut writes = Vec::new();
    for (path, value, coalesce) in plain {
        for entity in &holders {
            // A sizing pass, or a second pass of the same frame, can report the value the entity
            // already has. That is not an edit.
            if let Ok(current) = read_property(world, *entity, component, &path)
                && same_written(current.as_ref(), value.as_ref())
            {
                continue;
            }
            writes.push(Write {
                entity: *entity,
                path: path.clone(),
                value: must_clone(value.as_ref())?,
                coalesce,
            });
        }
    }
    for (parent, axes) in euler {
        let patch = |mut degrees: [f32; 3]| {
            for (index, axis) in axes.iter().enumerate() {
                if let Some(degree) = axis {
                    degrees[index] = *degree;
                }
            }
            euler_deg_to_quat(degrees)
        };
        // The drawer showed one of the two Euler readings of the sample's rotation (`(x, y, z)`
        // or `(x + 180, 180 - y, z + 180)`) and built the edited quaternion from it. Decide once
        // which reading that was and patch every holder on the same one: with the flipped
        // reading on screen, patching a holder's canonical reading would turn it by 180 degrees
        // on the edited axis.
        let flipped = sample.is_some_and(|entity| {
            let Some(drawn) = exact.get(&parent) else {
                return false;
            };
            let Ok(current) = read_quat(world, entity, component, &parent) else {
                return false;
            };
            let own = quat_to_euler_deg(&current);
            let matches = |q: &Quat| drawn.abs_diff_eq(*q, 1.0e-4) || drawn.abs_diff_eq(-*q, 1.0e-4);
            !matches(&patch(own)) && matches(&patch([own[0] + 180.0, 180.0 - own[1], own[2] + 180.0]))
        });
        for entity in &holders {
            let current = read_quat(world, *entity, component, &parent)?;
            let own = quat_to_euler_deg(&current);
            let base = if flipped {
                [own[0] + 180.0, 180.0 - own[1], own[2] + 180.0]
            } else {
                own
            };
            let value = patch(base);
            if !value.is_finite() {
                return Err(EditError::new(
                    "value_mismatch",
                    "an Euler axis must stay a finite rotation",
                ));
            }
            if same_rotation(&current, &value) {
                continue;
            }
            writes.push(Write {
                entity: *entity,
                path: parent.clone(),
                value: Box::new(value),
                coalesce: true,
            });
        }
    }
    for (parent, (lane, typed)) in &linked_edits {
        writes.extend(linked_writes(world, &holders, component, parent, lane, *typed)?);
    }
    let discrete = changes.iter().any(|change| change.discrete);
    let skipped = entities
        .iter()
        .filter(|entity| !holders.contains(entity))
        .map(|entity| entity.to_bits())
        .collect();
    let mut report = commit_writes(world, component, &full, writes, discrete)?;
    report.skipped = skipped;
    Ok(report)
}

/// Merged component values for `entities`. `primary` is the display sample when it has the
/// component; otherwise the first holder is. Empty input returns `count: 0`.
#[tracing::instrument(
    target = "regia_editor",
    skip(world, entities),
    name = "inspector_inspect_selection"
)]
#[must_use]
pub fn inspect_entities(world: &World, entities: &[Entity], primary: Option<Entity>) -> Value {
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let mut present: BTreeMap<String, (String, Vec<Entity>)> = BTreeMap::new();
    for entity in entities {
        let Ok(infos) = world.inspect_entity(*entity) else {
            continue;
        };
        for (_, info) in infos {
            let full = info.name().to_string();
            let entry = present.entry(full.clone()).or_insert_with(|| {
                let short = registry
                    .get_with_type_path(&full)
                    .map(|found| found.type_info().type_path_table().short_path().to_string())
                    .unwrap_or_else(|| full.rsplit("::").next().unwrap_or(&full).to_string());
                (short, Vec::new())
            });
            entry.1.push(*entity);
        }
    }
    let mut components = Vec::new();
    for (full, (short, holders)) in &present {
        let sample_entity = primary
            .filter(|entity| holders.contains(entity))
            .or_else(|| holders.first().copied());
        let Some(sample_entity) = sample_entity else {
            continue;
        };
        let Some(sample) = component_value(world, &registry, sample_entity, full) else {
            continue;
        };
        let mut others = Vec::new();
        for entity in holders {
            if *entity == sample_entity {
                continue;
            }
            if let Some(value) = component_value(world, &registry, *entity, full) {
                others.push(value);
            }
        }
        let other_refs: Vec<&dyn PartialReflect> = others.iter().map(|value| value.as_ref()).collect();
        let mixed = mixed_paths(sample.as_ref(), &other_refs);
        let mut fields = Vec::new();
        consider_field("", sample.as_ref(), false, &mixed, &registry, &mut fields);
        components.push(json!({
            "component": short,
            "type_path": full,
            "present": holders.len(),
            "selected": entities.len(),
            "editable": !holders.is_empty(),
            "fields": fields,
        }));
    }
    json!({
        "count": entities.len(),
        "entities": entities.iter().map(|entity| entity.to_bits()).collect::<Vec<_>>(),
        "primary": primary.map(|entity| entity.to_bits()),
        "components": components,
    })
}

/// Draws `component` for every holder in `entities` and writes the frame's edits as one undo step.
/// `primary` supplies the values that are not mixed. The Inspector's search and **Modified only**
/// ([`super::search::InspectorSearch`]) filter the rows ([`multi_row_filter`]).
pub fn component_ui_multi(
    ui: &mut egui::Ui,
    world: &mut World,
    entities: &[Entity],
    primary: Option<Entity>,
    component: &str,
    assets: std::sync::Arc<[(String, ContentKind)]>,
) -> Vec<Change> {
    let search = world
        .get_resource::<super::search::InspectorSearch>()
        .cloned()
        .unwrap_or_default();
    let filter = multi_row_filter(world, entities, primary, component, &search);
    component_ui_multi_filtered(ui, world, entities, primary, component, assets, filter)
}

/// Reflect paths of the leaves of `component` that differ from their baseline on any holder: the
/// union over `values` (one `(entity, value)` per holder). With `fallback_all`, a holder without a
/// baseline counts every leaf (as **Modified only** does); without it, none (as the reset arrows
/// do).
fn modified_union(
    world: &World,
    component: &str,
    values: &[(Entity, Box<dyn PartialReflect>)],
    fallback_all: bool,
) -> BTreeSet<String> {
    let _span = tracing::trace_span!(target: "regia_editor", "inspector_multi_modified").entered();
    let mut union = BTreeSet::new();
    for (entity, value) in values {
        match super::defaults::baseline(world, *entity, component) {
            Some((_, base)) => union.extend(super::defaults::modified_paths(value.as_ref(), base.as_ref())),
            None if fallback_all => union.extend(super::defaults::all_leaf_paths(value.as_ref())),
            None => {}
        }
    }
    union
}

/// The search / **Modified only** row filter of `component` on a multi-selection (P-110, P-116):
/// search hits on the shown sample (`primary` when it has the component), and a field counts as
/// modified when it differs from its baseline on any selected entity. `None` when no filter is on
/// or no entity has the component.
#[must_use]
pub fn multi_row_filter(
    world: &World,
    entities: &[Entity],
    primary: Option<Entity>,
    component: &str,
    search: &super::search::InspectorSearch,
) -> Option<super::search::RowFilter> {
    if !search.filtering() {
        return None;
    }
    let holders = holders_of(world, entities, component);
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let values: Vec<(Entity, Box<dyn PartialReflect>)> = holders
        .iter()
        .filter_map(|entity| Some((*entity, component_value(world, &registry, *entity, component)?)))
        .collect();
    drop(registry);
    let sample_at = primary
        .and_then(|entity| values.iter().position(|(holder, _)| *holder == entity))
        .unwrap_or(0);
    let (sample, value) = values.get(sample_at)?;
    let mut filter = super::search::row_filter(world, *sample, component, value.as_ref(), search)?;
    if search.modified_only {
        filter.modified = Some(super::search::PathSet::new(modified_union(
            world, component, &values, true,
        )));
    }
    Some(filter)
}

/// The frame component actions last ran in on a multi-selection (a click runs once per frame even
/// when egui draws the panel twice).
#[derive(Resource)]
struct MultiActionsRanFrame(u64);

/// Runs each of `actions` of `component` on every holder (`@Button` fields and the action row on a
/// multi-selection, P-130): one undo step per action covering every entity it changed. A refusal
/// on one entity restores that entity and the others still run.
pub fn run_actions_on(world: &mut World, holders: &[Entity], component: &str, actions: &[String]) {
    for action in actions {
        let mut group = CommandGroup::new(format!("{action} on {} entities", holders.len()));
        for entity in holders {
            let depth = undo_depth_of(world);
            match super::actions::run_component_action(world, *entity, component, action) {
                Ok(_) => {
                    if undo_depth_of(world) > depth
                        && let Some(command) = world
                            .get_resource_mut::<UndoStack>()
                            .and_then(|mut stack| stack.take_top())
                    {
                        group.commands.push(command);
                    }
                }
                Err(err) => {
                    tracing::warn!(target: "regia_editor", "component action {component}.{action}: {}", err.message);
                }
            }
        }
        if !group.commands.is_empty() {
            let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
            stack.seal();
            stack.push(Box::new(group));
            stack.seal();
        }
    }
}

/// [`component_ui_multi`] with the row filter given ([`multi_row_filter`]); the Inspector computes
/// it once to skip components without a matching row.
#[tracing::instrument(target = "regia_editor", skip_all, name = "inspector_component_multi")]
pub fn component_ui_multi_filtered(
    ui: &mut egui::Ui,
    world: &mut World,
    entities: &[Entity],
    primary: Option<Entity>,
    component: &str,
    assets: std::sync::Arc<[(String, ContentKind)]>,
    filter: Option<super::search::RowFilter>,
) -> Vec<Change> {
    let holders = holders_of(world, entities, component);
    if holders.is_empty() {
        return Vec::new();
    }
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let mut clones = Vec::new();
    for entity in &holders {
        if let Some(value) = component_value(world, &registry, *entity, component) {
            clones.push((*entity, value));
        }
    }
    if clones.is_empty() {
        return Vec::new();
    }
    let sample_at = primary
        .and_then(|entity| clones.iter().position(|(holder, _)| *holder == entity))
        .unwrap_or(0);
    let mixed = {
        let sample = clones[sample_at].1.as_ref();
        let others: Vec<&dyn PartialReflect> = clones
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != sample_at)
            .map(|(_, (_, value))| value.as_ref())
            .collect();
        mixed_paths(sample, &others)
    };
    // Reset arrows show where any holder differs from its baseline (P-114).
    let reset_modified = modified_union(world, component, &clones, false);
    let (sample, mut edited) = clones.swap_remove(sample_at);
    if let Some((owner, draw)) = crate::extension::panel::inspector_for(world, component) {
        drop(registry);
        ui.small(format!("drawn by {owner}"));
        let original = reflect_ui::clone_value(edited.as_ref());
        if !draw(ui, edited.as_mut()) {
            prefab_override_controls(ui, world, &holders, component, &BTreeSet::new());
            return Vec::new();
        }
        // The extension edits the sample's whole value. Write only the fields it changed, so the
        // other holders keep their own values everywhere else.
        let changes = match original {
            Some(original) => extension_changes(original.as_ref(), edited.as_ref()),
            None => vec![Change {
                path: String::new(),
                discrete: true,
            }],
        };
        if changes.is_empty() {
            prefab_override_controls(ui, world, &holders, component, &BTreeSet::new());
            return changes;
        }
        // egui discards the first pass of a new grid and runs the closure again. Committing the
        // discarded pass would push a second undo for the same edit.
        if ui.ctx().will_discard() {
            return changes;
        }
        if let Err(err) = commit_widget_changes(world, &holders, component, edited.as_ref(), &changes) {
            tracing::warn!(target: "regia_editor", "inspector edit: {err}");
        }
        prefab_override_controls(ui, world, &holders, component, &BTreeSet::new());
        return changes;
    }
    // Registered actions without a `@Button` field: a row of buttons, run on every holder (P-130).
    let type_info = registry
        .get_with_type_path(component)
        .or_else(|| registry.get_with_short_type_path(component))
        .map(|registration| registration.type_info());
    let row_action = super::actions::action_row(ui, world, component, type_info);
    let mut tools = super::clipboard::FieldTools::for_component(world, sample, component, edited.as_ref());
    tools.modified = reset_modified;
    let mut context = InspectorContext::new(component)
        .with_assets(assets)
        .with_mixed(mixed)
        .with_filter(filter)
        .with_linked(super::linked_vectors(world))
        .with_field_tools(tools)
        .with_thumbnails(world.remove_resource::<crate::thumb_cache::ThumbCache>())
        .with_pickers(super::picker_links::links(world, Some(sample), component));
    InspectorUi::new(&registry, &mut context).ui_for_reflect_with_options(
        edited.as_mut(),
        ui,
        egui::Id::new(("inspector", component, "multi")),
        &(),
    );
    drop(registry);
    if let Some(thumbs) = context.take_thumbnails() {
        world.insert_resource(thumbs);
    }
    let changes = context.take_changes();
    // Field menu picks (copy, paste, reset) on the selection (inspector::clipboard): input events
    // arrive in one pass only, so they run before the discarded-pass guard below.
    super::clipboard::run_multi_requests(
        ui.ctx(),
        world,
        &holders,
        component,
        context.take_field_requests(),
    );
    // See the extension path above: a discarded layout pass must not touch the world.
    if ui.ctx().will_discard() {
        return changes;
    }
    // Picker requests (ADR-7786) act on the sampled entity, as the field shows its value; the
    // color eyedropper writes every holder (ADR-8106).
    let mut requests = context.pickers.take_requests();
    requests.retain(|request| match request {
        super::picker_links::PickerRequest::ArmColorEyedropper(path) => {
            super::color_eyedropper::arm(world, holders.clone(), component, path);
            false
        }
        _ => true,
    });
    super::picker_links::run_requests(world, sample, component, requests);
    // Linked vectors (ADR-7785): the toggle is remembered, and a linked edit scales each entity.
    super::store_link_toggles(world, context.take_link_toggles());
    let linked: BTreeSet<String> = super::linked_vectors(world).into_iter().collect();
    if let Err(err) = commit_drawer_changes_linked(
        world,
        &holders,
        component,
        edited.as_ref(),
        &changes,
        Some(sample),
        context.lane_values(),
        &linked,
    ) {
        tracing::warn!(target: "regia_editor", "inspector edit: {err}");
    }
    // `@Button` fields and the action row run on every holder, once per frame (P-130).
    let mut actions = context.take_action_requests();
    actions.extend(row_action);
    let frame = ui.ctx().cumulative_frame_nr();
    let ran = world.get_resource::<MultiActionsRanFrame>().map(|ran| ran.0);
    if !actions.is_empty() && ran != Some(frame) {
        world.insert_resource(MultiActionsRanFrame(frame));
        run_actions_on(world, &holders, component, &actions);
    }
    prefab_override_controls(ui, world, &holders, component, &BTreeSet::new());
    changes
}

fn structural_commands(world: &mut World, commands: Vec<Box<dyn EditorCommand>>, label: String) -> usize {
    // Structural edits do not seal the stack: they never merge (`AddComponent` / `RemoveComponent`
    // refuse it), matching the single-entity add and remove buttons.
    push_applied(world, commands, label, false)
}

/// Inserts the reflected default of `type_path` on every entity in `entities` that lacks it.
/// Entities that already have it are skipped. Nothing is pushed when every entity has it.
///
/// # Errors
/// [`EditError`] with `entities_empty`, `not_found`, `feed_owned` or `path_unknown`.
pub fn add_component_to(
    world: &mut World,
    entities: &[Entity],
    type_path: &str,
) -> Result<EditReport, EditError> {
    if entities.is_empty() {
        return Err(EditError::new("entities_empty", "entities is empty"));
    }
    require_alive(world, entities)?;
    let (full, prototype) = {
        let registry = world.resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        let found = registration(&registry, type_path)
            .ok_or_else(|| EditError::new("path_unknown", format!("no reflected component {type_path}")))?;
        let prototype = found
            .data::<regia_ecs::reflect::std_traits::ReflectDefault>()
            .map(regia_ecs::reflect::std_traits::ReflectDefault::default)
            .ok_or_else(|| EditError::new("path_unknown", format!("{type_path} has no reflected default")))?;
        if found.data::<ReflectComponent>().is_none() {
            return Err(EditError::new(
                "path_unknown",
                format!("{type_path} is not a component"),
            ));
        }
        (found.type_info().type_path().to_string(), prototype)
    };
    let mut seen = BTreeSet::new();
    let targets: Vec<Entity> = entities
        .iter()
        .copied()
        .filter(|entity| seen.insert(*entity) && !has_component(world, *entity, &full))
        .collect();
    let skipped = entities
        .iter()
        .filter(|entity| !targets.contains(entity))
        .map(|entity| entity.to_bits())
        .collect();
    if targets.is_empty() {
        return Ok(EditReport {
            edited: Vec::new(),
            skipped,
            undo_depth: undo_depth_of(world),
        });
    }
    refuse_feed_owned(world, &targets)?;
    let mut commands = Vec::new();
    for entity in &targets {
        let captured = must_clone(prototype.as_ref())
            .and_then(|value| AddComponent::capture(world, *entity, &full, value).map_err(map_command));
        let mut command = match captured {
            Ok(command) => command,
            Err(err) => {
                roll_back(world, commands);
                return Err(err);
            }
        };
        command.apply(world);
        commands.push(Box::new(command) as Box<dyn EditorCommand>);
    }
    let undo_depth = structural_commands(world, commands, format!("Add {full}"));
    Ok(EditReport {
        edited: targets.iter().map(|entity| entity.to_bits()).collect(),
        skipped,
        undo_depth,
    })
}

/// Removes `component` from every entity in `entities` that has it.
///
/// # Errors
/// [`EditError`] with `entities_empty`, `not_found`, `feed_owned` or `component_on_none`.
pub fn remove_component_from(
    world: &mut World,
    entities: &[Entity],
    component: &str,
) -> Result<EditReport, EditError> {
    if entities.is_empty() {
        return Err(EditError::new("entities_empty", "entities is empty"));
    }
    require_alive(world, entities)?;
    let holders = holders_of(world, entities, component);
    if holders.is_empty() {
        return Err(EditError::new(
            "component_on_none",
            format!("none of the entities has {component}"),
        ));
    }
    refuse_feed_owned(world, &holders)?;
    let name = full_type_path(world, component).unwrap_or_else(|| component.to_string());
    let mut commands = Vec::new();
    for entity in &holders {
        let mut command = match RemoveComponent::capture(world, *entity, &name) {
            Ok(command) => command,
            Err(err) => {
                roll_back(world, commands);
                return Err(map_command(err));
            }
        };
        command.apply(world);
        commands.push(Box::new(command) as Box<dyn EditorCommand>);
    }
    let skipped = entities
        .iter()
        .filter(|entity| !holders.contains(entity))
        .map(|entity| entity.to_bits())
        .collect();
    let undo_depth = structural_commands(world, commands, format!("Remove {name}"));
    Ok(EditReport {
        edited: holders.iter().map(|entity| entity.to_bits()).collect(),
        skipped,
        undo_depth,
    })
}

/// Sets `Transform` on each entity. Omitted channels keep that entity's current value. One call is
/// one undo entry, and a prefab instance records an override for each channel that was passed.
///
/// # Errors
/// [`EditError`] with `entities_empty`, `not_found`, `feed_owned` or `component_on_none`.
pub fn set_transforms(
    world: &mut World,
    entities: &[Entity],
    translation: Option<Vec3>,
    rotation: Option<Quat>,
    scale: Option<Vec3>,
) -> Result<EditReport, EditError> {
    if entities.is_empty() {
        return Err(EditError::new("entities_empty", "entities is empty"));
    }
    require_alive(world, entities)?;
    let holders: Vec<Entity> = entities
        .iter()
        .copied()
        .filter(|entity| world.get::<Transform>(*entity).is_some())
        .collect();
    if holders.is_empty() {
        return Err(EditError::new(
            "component_on_none",
            "none of the entities has a Transform",
        ));
    }
    refuse_feed_owned(world, &holders)?;
    let full =
        full_type_path(world, "Transform").unwrap_or_else(|| "regia_scene::transform::Transform".to_string());
    let before = snapshot_roots(world, &holders);
    let mut commands: Vec<Box<dyn EditorCommand>> = Vec::new();
    for entity in &holders {
        let current = world
            .get::<Transform>(*entity)
            .copied()
            .unwrap_or(Transform::IDENTITY);
        let new = Transform {
            translation: translation.unwrap_or(current.translation),
            rotation: rotation.unwrap_or(current.rotation),
            scale: scale.unwrap_or(current.scale),
        };
        let mut command = SetTransform::capture(world, *entity, new);
        command.apply(world);
        if let Some(value) = translation {
            note_override(world, *entity, &full, "translation", &value)?;
        }
        if let Some(value) = rotation {
            note_override(world, *entity, &full, "rotation", &value)?;
        }
        if let Some(value) = scale {
            note_override(world, *entity, &full, "scale", &value)?;
        }
        commands.push(Box::new(command));
    }
    for (bits, old) in before {
        let Some(entity) = crate::command::resolve_entity(world, bits) else {
            continue;
        };
        let new = world
            .get::<PrefabInstance>(entity)
            .map(|instance| instance.overrides.clone())
            .unwrap_or_default();
        if new != old {
            commands.push(Box::new(SetPrefabOverrides {
                entity: bits,
                old,
                new,
            }));
        }
    }
    let undo_depth = push_applied(world, commands, "Set Transform".to_string(), true);
    Ok(EditReport {
        edited: holders.iter().map(|entity| entity.to_bits()).collect(),
        skipped: entities
            .iter()
            .filter(|entity| !holders.contains(entity))
            .map(|entity| entity.to_bits())
            .collect(),
        undo_depth,
    })
}

fn link_and_root(world: &World, entity: Entity) -> Result<(PrefabLink, Entity), EditError> {
    let link = world.get::<PrefabLink>(entity).copied().ok_or_else(|| {
        EditError::new(
            "not_prefab_instance",
            "the entity is not inside a prefab instance",
        )
    })?;
    let root = instance_root(world, entity)
        .ok_or_else(|| EditError::new("not_prefab_instance", "the entity has no prefab instance root"))?;
    Ok((link, root))
}

fn find_override<'a>(
    instance: &'a PrefabInstance,
    target: u128,
    component: &str,
    path: &str,
) -> Option<&'a OverrideEntry> {
    instance.overrides.iter().find(|entry| {
        entry.target == target
            && components_match(&entry.component, component)
            && paths_match(&entry.path, path)
    })
}

fn template_field(
    template: &PrefabTemplate,
    local_id: u128,
    component: &str,
    path: &str,
) -> Result<Box<dyn PartialReflect>, EditError> {
    let entity = template
        .entities
        .iter()
        .find(|entity| entity.local_id.as_u128() == local_id)
        .ok_or_else(|| EditError::new("prefab_template_missing", "the prefab has no such entity"))?;
    let (_, baked) = entity
        .components
        .iter()
        .find(|(type_path, _)| components_match(type_path, component))
        .ok_or_else(|| {
            EditError::new(
                "prefab_template_missing",
                format!("the prefab entity has no {component}"),
            )
        })?;
    let BakedValue::Reflected(value) = baked else {
        return Err(EditError::new(
            "prefab_apply_failed",
            "the template component is not a reflected value",
        ));
    };
    let normalized = strip_dot(path);
    if normalized.is_empty() {
        return must_clone(value.as_partial_reflect());
    }
    let field = value
        .reflect_path(normalized)
        .map_err(|_| EditError::new("path_unknown", format!("no field {normalized} on the template")))?;
    must_clone(field)
}

/// Drops one override on `entity`'s instance and writes the template value back. Other instances
/// are left alone. One undo restores the value and the override.
///
/// # Errors
/// [`EditError`] with `not_prefab_instance`, `override_absent`, `prefab_template_missing`,
/// `path_unknown`, `feed_owned` or `prefab_apply_failed`.
pub fn revert_prefab_override(
    world: &mut World,
    entity: Entity,
    component: &str,
    path: &str,
) -> Result<EditReport, EditError> {
    require_alive(world, &[entity])?;
    refuse_feed_owned(world, &[entity])?;
    let (link, root) = link_and_root(world, entity)?;
    let instance = world
        .get::<PrefabInstance>(root)
        .cloned()
        .ok_or_else(|| EditError::new("not_prefab_instance", "the instance root lost PrefabInstance"))?;
    let entry = find_override(&instance, link.local_id, component, path)
        .cloned()
        .ok_or_else(|| EditError::new("override_absent", "that field has no override on this instance"))?;
    let library = world.resource::<PrefabLibrary>().clone();
    let template = library
        .get(regia_core::id::Guid::from_u128(instance.prefab))
        .cloned()
        .ok_or_else(|| EditError::new("prefab_template_missing", "the prefab template is not loaded"))?;
    let baked = template_field(&template, link.local_id, component, &entry.path)?;
    let reflect_path = strip_dot(&entry.path).to_string();
    let full = full_type_path(world, component).unwrap_or(entry.component.clone());
    let name = if full.is_empty() { component } else { full.as_str() };
    let mut property =
        SetProperty::capture(world, entity, name, &reflect_path, baked).map_err(map_command)?;
    property.coalesce = false;
    property.apply(world);
    let old = instance.overrides.clone();
    let mut new = old.clone();
    new.retain(|candidate| {
        !(candidate.target == link.local_id
            && components_match(&candidate.component, component)
            && paths_match(&candidate.path, path))
    });
    if let Some(mut live) = world.get_mut::<PrefabInstance>(root) {
        live.overrides.clone_from(&new);
    }
    let undo_depth = push_applied(
        world,
        vec![
            Box::new(property),
            Box::new(SetPrefabOverrides {
                entity: root.to_bits(),
                old,
                new,
            }),
        ],
        format!("Revert {component}.{path}"),
        true,
    );
    Ok(EditReport {
        edited: vec![entity.to_bits()],
        skipped: Vec::new(),
        undo_depth,
    })
}

fn baked_ron(baked: &BakedValue, registry: &TypeRegistry) -> Result<String, EditError> {
    match baked {
        BakedValue::Reflected(value) => ReflectCodec::new(registry)
            .to_ron_typed(value.as_partial_reflect())
            .map_err(|err| EditError::new("prefab_apply_failed", err.to_string())),
        BakedValue::Raw(raw, _) => Ok(raw.clone()),
    }
}

fn raw_ron(text: &str) -> Result<Box<ron::value::RawValue>, EditError> {
    ron::value::RawValue::from_ron(text)
        .map(ToOwned::to_owned)
        .map_err(|err| EditError::new("prefab_apply_failed", err.to_string()))
}

/// Clears one override on `entity`'s instance and writes that one live field into the prefab
/// template. Other live fields, and other instances, stay as they are. One undo restores both
/// the previous template and the instance override marker.
///
/// # Errors
/// [`EditError`] with `not_prefab_instance`, `override_absent`, `prefab_template_missing`,
/// `path_unknown`, `feed_owned` or `prefab_apply_failed`.
pub fn apply_prefab_override(
    world: &mut World,
    entity: Entity,
    component: &str,
    path: &str,
) -> Result<EditReport, EditError> {
    require_alive(world, &[entity])?;
    refuse_feed_owned(world, &[entity])?;
    let (link, root) = link_and_root(world, entity)?;
    let instance = world
        .get::<PrefabInstance>(root)
        .cloned()
        .ok_or_else(|| EditError::new("not_prefab_instance", "the instance root lost PrefabInstance"))?;
    let entry = find_override(&instance, link.local_id, component, path)
        .cloned()
        .ok_or_else(|| EditError::new("override_absent", "that field has no override on this instance"))?;
    // A prefab with a file in the open project: write the outermost instance's file (ADR-7203).
    let outer = outermost_root(world, root);
    let outer_asset = world
        .get::<PrefabInstance>(outer)
        .map_or(instance.prefab_guid(), PrefabInstance::prefab_guid);
    // A variant with a file in the open project: the override goes into its `.variant.ron`
    // (ADR-7263); the base file is not touched.
    if let Some(variant_path) = variant_file_of(world, outer_asset) {
        if root != outer {
            return Err(EditError::new(
                "prefab_apply_failed",
                "the override is on a prefab nested inside a variant instance: apply it from the nested prefab's own instance",
            ));
        }
        return apply_override_to_variant(
            world,
            FileApply {
                entity,
                root,
                outer,
                local_id: link.local_id,
                entry: &entry,
                component,
                path,
                file_path: &variant_path,
                asset: None,
            },
            outer_asset,
        );
    }
    if let Some(file_path) = prefab_file_of(world, outer_asset) {
        return apply_override_to_file(
            world,
            FileApply {
                entity,
                root,
                outer,
                local_id: link.local_id,
                entry: &entry,
                component,
                path,
                file_path: &file_path,
                asset: None,
            },
        );
    }
    let registry_handle = world.resource::<AppTypeRegistry>().clone();
    let library = world.resource::<PrefabLibrary>().clone();
    let template = library
        .get(regia_core::id::Guid::from_u128(instance.prefab))
        .cloned()
        .ok_or_else(|| EditError::new("prefab_template_missing", "the prefab template is not loaded"))?;
    if template.entities.iter().any(|entity| entity.nested.is_some()) {
        return Err(EditError::new(
            "prefab_apply_failed",
            "applying an override onto a prefab that nests another prefab is not supported",
        ));
    }
    let reflect_path = strip_dot(&entry.path).to_string();
    let full = full_type_path(world, component).unwrap_or(entry.component.clone());
    let live_field = read_property(world, entity, &full, &reflect_path).map_err(map_command)?;
    let registry = registry_handle.read();
    let mut patched = String::new();
    let mut patched_type = String::new();
    let template_entity = template
        .entities
        .iter()
        .find(|item| item.local_id.as_u128() == link.local_id)
        .ok_or_else(|| EditError::new("prefab_template_missing", "the prefab has no such entity"))?;
    for (type_path, baked) in &template_entity.components {
        if components_match(type_path, component) {
            let BakedValue::Reflected(value) = baked else {
                return Err(EditError::new(
                    "prefab_apply_failed",
                    "the template component is not a reflected value",
                ));
            };
            // A component that holds a fixed-size array (`[f32; 3]`) has no `reflect_clone`.
            // The dynamic fallback's RON does not bake back into the component, so restore the
            // registered type before serializing.
            let mut cloned =
                reflect_ui::clone_concrete(&registry, value.as_partial_reflect()).ok_or_else(|| {
                    EditError::new(
                        "prefab_apply_failed",
                        "the template component could not be cloned",
                    )
                })?;
            if reflect_path.is_empty() {
                cloned = reflect_ui::clone_concrete(&registry, live_field.as_ref()).ok_or_else(|| {
                    EditError::new("prefab_apply_failed", "the live component could not be cloned")
                })?;
            } else {
                reflect_at_mut(cloned.as_mut(), &reflect_path)?
                    .try_apply(live_field.as_ref())
                    .map_err(|err| EditError::new("value_mismatch", err.to_string()))?;
            }
            patched = ReflectCodec::new(&registry)
                .to_ron_typed(cloned.as_ref())
                .map_err(|err| EditError::new("prefab_apply_failed", err.to_string()))?;
            patched_type = type_path.clone();
        }
    }
    if patched.is_empty() {
        return Err(EditError::new(
            "prefab_template_missing",
            format!("the prefab entity has no {component}"),
        ));
    }
    let mut file = SceneFile::new();
    let mut old_file = SceneFile::new();
    for item in &template.entities {
        let parent = item.parent.and_then(|parent| {
            template
                .entities
                .get(parent as usize)
                .map(|parent| parent.local_id)
        });
        let mut components = BTreeMap::new();
        let mut old_components = BTreeMap::new();
        for (type_path, baked) in &item.components {
            old_components.insert(type_path.clone(), raw_ron(&baked_ron(baked, &registry)?)?);
            let text = if item.local_id.as_u128() == link.local_id && type_path == &patched_type {
                patched.clone()
            } else {
                baked_ron(baked, &registry)?
            };
            components.insert(type_path.clone(), raw_ron(&text)?);
        }
        old_file.entities.push(SceneEntity {
            guid: item.local_id,
            name: None,
            parent,
            components: old_components,
            prefab_instance: None,
        });
        file.entities.push(SceneEntity {
            guid: item.local_id,
            name: None,
            parent,
            components,
            prefab_instance: None,
        });
    }
    let asset = template.asset;
    let new_template = PrefabTemplate::from_scene_file(asset, &file, &registry)
        .map_err(|err| EditError::new("prefab_apply_failed", err.to_string()))?;
    if !new_template.warnings.is_empty() {
        return Err(EditError::new(
            "prefab_apply_failed",
            new_template.warnings.join("; "),
        ));
    }
    drop(registry);
    drop(library);
    let old = instance.overrides.clone();
    let mut new = old.clone();
    new.retain(|candidate| {
        !(candidate.target == link.local_id
            && components_match(&candidate.component, component)
            && paths_match(&candidate.path, path))
    });
    if let Some(mut live) = world.get_mut::<PrefabInstance>(root) {
        live.overrides.clone_from(&new);
    }
    world.resource_mut::<PrefabLibrary>().insert(new_template);
    let undo_depth = push_applied(
        world,
        vec![
            Box::new(SetPrefabTemplate {
                asset,
                old: old_file,
                new: file,
            }),
            Box::new(SetPrefabOverrides {
                entity: root.to_bits(),
                old,
                new,
            }),
        ],
        format!("Apply {component}.{path}"),
        true,
    );
    Ok(EditReport {
        edited: vec![entity.to_bits()],
        skipped: Vec::new(),
        undo_depth,
    })
}

/// The `.prefab.ron` of `asset` in the open project, when there is one (a variant is not: its
/// Apply stays with the loaded template).
fn prefab_file_of(world: &World, asset: regia_core::id::Guid) -> Option<std::path::PathBuf> {
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()?
        .root
        .clone()?;
    let path = crate::prefab_variant_edit::find_prefab_by_id(&root, asset)?;
    (!crate::prefab_variant_edit::is_variant_path(&path)).then_some(path)
}

/// The `.variant.ron` of `asset` in the open project, when `asset` is a variant with a file.
fn variant_file_of(world: &World, asset: regia_core::id::Guid) -> Option<std::path::PathBuf> {
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()?
        .root
        .clone()?;
    let path = crate::prefab_variant_edit::find_prefab_by_id(&root, asset)?;
    crate::prefab_variant_edit::is_variant_path(&path).then_some(path)
}

/// The prefab or variant file an Apply on `entity` writes, when the open project has one (what
/// [`apply_prefab_override`] picks; `None` for the session-only Apply).
#[must_use]
pub fn prefab_apply_file(world: &World, entity: Entity) -> Option<std::path::PathBuf> {
    let (_, root) = link_and_root(world, entity).ok()?;
    let outer = outermost_root(world, root);
    let asset = world.get::<PrefabInstance>(outer)?.prefab_guid();
    variant_file_of(world, asset).or_else(|| prefab_file_of(world, asset))
}

/// Where an Apply writes an override (spec 19, the Overrides window's "Apply to" choice).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ApplyTarget {
    /// The prefab the entity's own instance is of, when that instance is nested inside another
    /// prefab: the entity's record in that prefab's `.prefab.ron`.
    Nested,
    /// The outermost instance's `.prefab.ron` (an entity of a nested prefab gets an outer-wins
    /// override on the nested instance record there).
    Outer,
    /// The outermost instance's `.variant.ron`.
    Variant,
    /// The `.prefab.ron` at the end of the outermost instance's variant chain.
    Base,
}

impl ApplyTarget {
    /// Every target, in the order the picker lists them.
    pub const ALL: [ApplyTarget; 4] = [Self::Nested, Self::Outer, Self::Variant, Self::Base];

    /// The protocol name (`nested`, `outer`, `variant`, `base`).
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Nested => "nested",
            Self::Outer => "outer",
            Self::Variant => "variant",
            Self::Base => "base",
        }
    }

    /// The target named `text` (see [`ApplyTarget::id`]).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|target| target.id() == text)
    }

    /// What the picker shows.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Nested => "Nested prefab",
            Self::Outer => "Outer prefab",
            Self::Variant => "Variant",
            Self::Base => "Base prefab",
        }
    }
}

/// One valid Apply target of an override and the file it writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyChoice {
    /// The target.
    pub target: ApplyTarget,
    /// The file an Apply to it writes.
    pub file: std::path::PathBuf,
}

/// Whether a variant step of `steps` overrides `component.path` (or the whole component) of the
/// prefab entity `target`: an Apply below those steps would not show through them.
fn shadowed_by_steps(
    library: &PrefabLibrary,
    steps: &[regia_core::id::Guid],
    target: regia_core::id::Guid,
    component: &str,
    path: &str,
) -> bool {
    steps.iter().any(|step| {
        library.variant(*step).is_some_and(|derived| {
            derived.edits.overrides.iter().any(|existing| {
                existing.target == target
                    && components_match(&existing.component, component)
                    && (paths_match(&existing.path, path) || strip_dot(&existing.path).is_empty())
            })
        })
    })
}

/// Whether `overrides` holds an override of `component.path` (or of the whole component) on the
/// prefab entity `local_id`.
fn overrides_field(
    overrides: &[regia_scene::format::PrefabOverride],
    local_id: u128,
    component: &str,
    path: &str,
) -> bool {
    overrides.iter().any(|existing| {
        existing.target.as_u128() == local_id
            && components_match(&existing.component, component)
            && (paths_match(&existing.path, path) || strip_dot(&existing.path).is_empty())
    })
}

/// Whether the added-child record `nested` (searched through `children` and the added children
/// of their own instance records) instantiates a prefab with an override of `component.path` on
/// `local_id`: a variant that adds the nested instance as a child and overrides the field there.
fn added_child_overrides(
    children: &[SceneEntity],
    nested: u128,
    local_id: u128,
    component: &str,
    path: &str,
) -> bool {
    let mut stack: Vec<&SceneEntity> = children.iter().collect();
    // A bounded walk: records are data from files.
    for _ in 0..4096 {
        let Some(child) = stack.pop() else {
            return false;
        };
        if let Some(instance) = &child.prefab_instance {
            if child.guid.as_u128() == nested
                && overrides_field(&instance.overrides, local_id, component, path)
            {
                return true;
            }
            stack.extend(instance.added_children.iter());
        }
    }
    false
}

/// Whether a prefab that nests the instance `root` (up to `outer`) overrides `component.path` of
/// `local_id`: a value written into `root`'s own prefab would not show through it. Each nesting
/// instance is resolved through its variant chain: a variant step's edit list that overrides the
/// field (the shadow [`shadowed_by_steps`] finds for the base), a variant step's added child that
/// is this nested instance and overrides the field, or the template at the bottom of the chain
/// whose nested instance record overrides it.
fn nested_record_overrides(
    world: &World,
    root: Entity,
    outer: Entity,
    local_id: u128,
    component: &str,
    path: &str,
) -> bool {
    let Some(library) = world.get_resource::<PrefabLibrary>() else {
        return false;
    };
    let mut cursor = root;
    for _ in 0..64 {
        if cursor == outer {
            return false;
        }
        let (Some(link), Some(parent)) = (
            world.get::<PrefabLink>(cursor).copied(),
            instance_root(world, cursor),
        ) else {
            return false;
        };
        let chain = world
            .get::<PrefabInstance>(parent)
            .map(|instance| library.variant_chain(instance.prefab_guid()))
            .unwrap_or_default();
        let by_variant = chain.iter().any(|step| {
            library.variant(*step).is_some_and(|variant| {
                overrides_field(&variant.edits.overrides, local_id, component, path)
                    || added_child_overrides(
                        &variant.edits.added_children,
                        link.local_id,
                        local_id,
                        component,
                        path,
                    )
            })
        });
        let by_template = chain
            .last()
            .and_then(|base| library.get(*base))
            .and_then(|template| {
                template
                    .entities
                    .iter()
                    .find(|item| item.local_id.as_u128() == link.local_id)
            })
            .and_then(|item| item.nested.as_ref())
            .is_some_and(|nested| overrides_field(&nested.instance.overrides, local_id, component, path));
        if by_variant || by_template {
            return true;
        }
        cursor = parent;
    }
    false
}

/// Whether the live entity `member` takes its value from `asset`: an instance above it is of
/// `asset`, or of a variant built on `asset` whose steps leave the field alone.
fn follows_asset(
    world: &World,
    library: &PrefabLibrary,
    member: Entity,
    asset: regia_core::id::Guid,
    component: &str,
    path: &str,
) -> bool {
    let Some(link) = world.get::<PrefabLink>(member) else {
        return false;
    };
    ancestors(world, member).into_iter().any(|at| {
        world.get::<PrefabInstance>(at).is_some_and(|instance| {
            let chain = library.variant_chain(instance.prefab_guid());
            chain
                .iter()
                .position(|step| *step == asset)
                .is_some_and(|at| !shadowed_by_steps(library, &chain[..at], link.guid(), component, path))
        })
    })
}

/// The valid Apply targets of the override of `component.path` on `entity`, in picker order,
/// and the one [`apply_prefab_override`] picks by itself (`None` when it picks none of them: the
/// session-only Apply, or an override nested inside a variant instance).
#[must_use]
pub fn prefab_apply_targets(
    world: &World,
    entity: Entity,
    component: &str,
    path: &str,
) -> (Vec<ApplyChoice>, Option<ApplyTarget>) {
    let mut choices = Vec::new();
    let Ok((link, root)) = link_and_root(world, entity) else {
        return (choices, None);
    };
    let outer = outermost_root(world, root);
    let (Some(inner_asset), Some(outer_asset)) = (
        world.get::<PrefabInstance>(root).map(PrefabInstance::prefab_guid),
        world
            .get::<PrefabInstance>(outer)
            .map(PrefabInstance::prefab_guid),
    ) else {
        return (choices, None);
    };
    if root != outer
        && !nested_record_overrides(world, root, outer, link.local_id, component, path)
        && let Some(file) = prefab_file_of(world, inner_asset)
    {
        choices.push(ApplyChoice {
            target: ApplyTarget::Nested,
            file,
        });
    }
    let mut default = None;
    if let Some(variant) = variant_file_of(world, outer_asset) {
        if root == outer {
            choices.push(ApplyChoice {
                target: ApplyTarget::Variant,
                file: variant,
            });
            default = Some(ApplyTarget::Variant);
        }
        if let Some(library) = world.get_resource::<PrefabLibrary>() {
            let chain = library.variant_chain(outer_asset);
            if let Some((base, steps)) = chain.split_last()
                && *base != outer_asset
                && !shadowed_by_steps(library, steps, link.guid(), component, path)
                && let Some(file) = prefab_file_of(world, *base)
            {
                choices.push(ApplyChoice {
                    target: ApplyTarget::Base,
                    file,
                });
            }
        }
    } else if let Some(file) = prefab_file_of(world, outer_asset) {
        choices.push(ApplyChoice {
            target: ApplyTarget::Outer,
            file,
        });
        default = Some(ApplyTarget::Outer);
    }
    choices.sort_by_key(|choice| {
        ApplyTarget::ALL
            .iter()
            .position(|target| *target == choice.target)
    });
    (choices, default)
}

/// [`apply_prefab_override`] into an explicit `target` (the Overrides window's "Apply to"):
/// `None`, or the target [`apply_prefab_override`] picks by itself, is exactly that Apply.
/// [`ApplyTarget::Nested`] writes the entity's record into the nested prefab's own file (the
/// outer file is untouched; every live copy of the nested prefab that keeps no override of its
/// own, and is not overridden by the prefab that nests it, takes the value).
/// [`ApplyTarget::Base`] writes the record into the variant chain's base file; instances of the
/// base and of every variant whose steps leave the field alone take the value. One undo step.
///
/// # Errors
/// As [`apply_prefab_override`], plus `apply_target_invalid` when `target` is not one of
/// [`prefab_apply_targets`].
pub fn apply_prefab_override_to(
    world: &mut World,
    entity: Entity,
    component: &str,
    path: &str,
    target: Option<ApplyTarget>,
) -> Result<EditReport, EditError> {
    let (choices, default) = prefab_apply_targets(world, entity, component, path);
    let Some(target) = target.filter(|target| Some(*target) != default) else {
        return apply_prefab_override(world, entity, component, path);
    };
    require_alive(world, &[entity])?;
    refuse_feed_owned(world, &[entity])?;
    let (link, root) = link_and_root(world, entity)?;
    let instance = world
        .get::<PrefabInstance>(root)
        .cloned()
        .ok_or_else(|| EditError::new("not_prefab_instance", "the instance root lost PrefabInstance"))?;
    let entry = find_override(&instance, link.local_id, component, path)
        .cloned()
        .ok_or_else(|| EditError::new("override_absent", "that field has no override on this instance"))?;
    let Some(choice) = choices.into_iter().find(|choice| choice.target == target) else {
        return Err(EditError::new(
            "apply_target_invalid",
            format!(
                "this override cannot be applied to the {} (editor.prefab_overrides lists the valid targets)",
                target.label().to_lowercase()
            ),
        ));
    };
    let outer = outermost_root(world, root);
    let (outer, asset) = match target {
        ApplyTarget::Nested => (root, None),
        ApplyTarget::Base => {
            let outer_asset = world
                .get::<PrefabInstance>(outer)
                .map_or(instance.prefab_guid(), PrefabInstance::prefab_guid);
            let base = world
                .get_resource::<PrefabLibrary>()
                .and_then(|library| library.variant_chain(outer_asset).last().copied());
            (outer, base)
        }
        // Each is the default whenever it is valid (handled above).
        ApplyTarget::Outer | ApplyTarget::Variant => {
            return apply_prefab_override(world, entity, component, path);
        }
    };
    apply_override_to_file(
        world,
        FileApply {
            entity,
            root,
            outer,
            local_id: link.local_id,
            entry: &entry,
            component,
            path,
            file_path: &choice.file,
            asset,
        },
    )
}

/// Apply override into a variant file (spec 14 § A3, ADR-7263): the instance's override entry
/// goes, as it is, into the variant's own edit list (`base.overrides`, replacing an override of
/// the same field), the file is written atomically in the current format and the variant is
/// registered again. This instance's override goes; every other live instance of the variant
/// (or of a variant built on it) that neither overrides the field itself nor sits on a variant
/// step that does takes the value in place. The base file is not touched. One undo puts the
/// variant bytes, the override and the values back.
fn apply_override_to_variant(
    world: &mut World,
    apply: FileApply<'_>,
    variant: regia_core::id::Guid,
) -> Result<EditReport, EditError> {
    let FileApply {
        entity,
        root,
        local_id,
        entry,
        component,
        path,
        file_path,
        ..
    } = apply;
    let failed = |message: String| EditError::new("prefab_apply_failed", message);
    let old_text = std::fs::read_to_string(file_path)
        .map_err(|err| failed(format!("cannot read {}: {err}", file_path.display())))?;
    let mut file = regia_scene::prefab_variant::PrefabVariantFile::from_ron_str(&old_text)
        .map_err(|err| failed(format!("cannot parse {}: {err}", file_path.display())))?;
    let target = regia_core::id::Guid::from_u128(local_id);
    file.base.overrides.retain(|existing| {
        !(existing.target == target
            && components_match(&existing.component, component)
            && paths_match(&existing.path, path))
    });
    file.base.overrides.push(regia_scene::prelude::PrefabOverride {
        target,
        component: entry.component.clone(),
        path: entry.path.clone(),
        value: raw_ron(entry.value.trim())?,
    });
    // Canonical text, current format version (`to_ron_string` writes what this build reads).
    file.format_version = regia_scene::prefab_variant::PREFAB_VARIANT_FORMAT_VERSION;
    let new_text = file.to_ron_string().map_err(|err| failed(err.to_string()))?;
    let reflect_path = strip_dot(&entry.path).to_string();
    let full = full_type_path(world, component).unwrap_or(entry.component.clone());
    let live_field = read_property(world, entity, &full, &reflect_path).map_err(map_command)?;
    let mut write = WriteVariantFile {
        path: file_path.to_path_buf(),
        asset: variant,
        old: old_text,
        new: new_text,
    };
    write.try_apply(world).map_err(failed)?;

    let instance = world
        .get::<PrefabInstance>(root)
        .cloned()
        .ok_or_else(|| EditError::new("not_prefab_instance", "the instance root lost PrefabInstance"))?;
    let old = instance.overrides.clone();
    let mut new = old.clone();
    new.retain(|candidate| {
        !(candidate.target == local_id
            && components_match(&candidate.component, component)
            && paths_match(&candidate.path, path))
    });
    if let Some(mut live) = world.get_mut::<PrefabInstance>(root) {
        live.overrides.clone_from(&new);
    }
    let mut commands: Vec<Box<dyn EditorCommand>> = vec![
        Box::new(write),
        Box::new(SetPrefabOverrides {
            entity: root.to_bits(),
            old,
            new,
        }),
    ];
    let library = world.resource::<PrefabLibrary>().clone();
    let mut members: Vec<Entity> = {
        let mut query = world.query::<(Entity, &PrefabLink)>();
        query
            .iter(world)
            .filter(|(member, link)| link.local_id == local_id && *member != entity)
            .map(|(member, _)| member)
            .collect()
    };
    members.sort_unstable();
    for member in members {
        let Some(holder) = ancestors(world, member)
            .into_iter()
            .find_map(|at| world.get::<PrefabInstance>(at).map(PrefabInstance::prefab_guid))
        else {
            continue;
        };
        // An instance of the variant, or of a variant built on it whose own steps above this
        // one leave the field alone.
        let chain = library.variant_chain(holder);
        let Some(at) = chain.iter().position(|step| *step == variant) else {
            continue;
        };
        let shadowed = chain[..at].iter().any(|step| {
            library.variant(*step).is_some_and(|derived| {
                derived.edits.overrides.iter().any(|existing| {
                    existing.target == target
                        && components_match(&existing.component, component)
                        && (paths_match(&existing.path, path) || strip_dot(&existing.path).is_empty())
                })
            })
        });
        if shadowed || overridden_above(world, member, local_id, component, path) {
            continue;
        }
        let value = must_clone(live_field.as_ref())?;
        match SetProperty::capture(world, member, &full, &reflect_path, value) {
            Ok(mut property) => {
                property.coalesce = false;
                property.apply(world);
                commands.push(Box::new(property));
            }
            Err(err) => {
                tracing::warn!(target: "regia_editor", "apply override: variant instance {member:?} not updated: {err}");
            }
        }
    }
    let undo_depth = push_applied(
        world,
        commands,
        format!("Apply {component}.{path} to {}", file_path.display()),
        true,
    );
    Ok(EditReport {
        edited: vec![entity.to_bits()],
        skipped: Vec::new(),
        undo_depth,
    })
}

/// Writes a variant file and registers the variant again; undo writes the old bytes back. The
/// instances are not rebuilt (the Apply updates them in place, with their own undo steps).
struct WriteVariantFile {
    path: std::path::PathBuf,
    asset: regia_core::id::Guid,
    old: String,
    new: String,
}

impl WriteVariantFile {
    fn write(
        world: &mut World,
        path: &std::path::Path,
        asset: regia_core::id::Guid,
        text: &str,
    ) -> Result<(), String> {
        let file = regia_scene::prefab_variant::PrefabVariantFile::from_ron_str(text)
            .map_err(|err| err.to_string())?;
        let variant = regia_scene::prefab_variant::PrefabVariant::from_file(asset, &file)
            .map_err(|err| err.to_string())?;
        crate::atomic_write::write_atomic(path, text.as_bytes())?;
        world
            .get_resource_or_insert_with(crate::prefab_link::PrefabWriteEcho::default)
            .note(path, text);
        world
            .get_resource_or_insert_with::<PrefabLibrary>(PrefabLibrary::default)
            .insert_variant(variant);
        Ok(())
    }

    fn try_apply(&mut self, world: &mut World) -> Result<(), String> {
        Self::write(world, &self.path, self.asset, &self.new)
    }
}

impl EditorCommand for WriteVariantFile {
    fn apply(&mut self, world: &mut World) {
        if let Err(err) = Self::write(world, &self.path, self.asset, &self.new) {
            tracing::error!(target: "regia_editor", "redo apply to {}: {err}", self.path.display());
        }
    }

    fn revert(&mut self, world: &mut World) {
        if let Err(err) = Self::write(world, &self.path, self.asset, &self.old) {
            tracing::error!(target: "regia_editor", "undo apply to {}: {err}", self.path.display());
        }
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn changes_level_state(&self) -> bool {
        false
    }

    fn description(&self) -> String {
        format!("Write {}", self.path.display())
    }
}

/// The outermost instance root above (or at) `root`: the first entity, going up, that carries
/// [`PrefabInstance`] and is not itself prefab-internal. A nested instance's root is internal to
/// the prefab that nests it.
fn outermost_root(world: &World, root: Entity) -> Entity {
    ancestors(world, root)
        .into_iter()
        .find(|at| world.get::<PrefabInstance>(*at).is_some() && world.get::<PrefabLink>(*at).is_none())
        .unwrap_or(root)
}

/// One file-backed Apply's inputs.
struct FileApply<'a> {
    entity: Entity,
    /// The instance root that holds the override.
    root: Entity,
    /// The outermost instance root: its prefab's file is written.
    outer: Entity,
    local_id: u128,
    entry: &'a OverrideEntry,
    component: &'a str,
    path: &'a str,
    file_path: &'a std::path::Path,
    /// The prefab the file holds, when it is not the outer root's own (a variant's base).
    asset: Option<regia_core::id::Guid>,
}

/// The ancestors of `entity` (itself first) up to the outermost instance root.
fn ancestors(world: &World, entity: Entity) -> Vec<Entity> {
    let mut out = vec![entity];
    let mut cursor = entity;
    while let Some(parent) = world.get::<ChildOf>(cursor).map(ChildOf::parent) {
        if out.len() > 256 || out.contains(&parent) {
            break;
        }
        out.push(parent);
        cursor = parent;
    }
    out
}

/// Whether an instance above `member` (its own nested root or any enclosing one) overrides
/// `component.path` of `local_id`, or the whole component.
fn overridden_above(world: &World, member: Entity, local_id: u128, component: &str, path: &str) -> bool {
    ancestors(world, member).into_iter().any(|at| {
        world.get::<PrefabInstance>(at).is_some_and(|instance| {
            instance.overrides.iter().any(|entry| {
                entry.target == local_id
                    && components_match(&entry.component, component)
                    && (paths_match(&entry.path, path) || strip_dot(&entry.path).is_empty())
            })
        })
    })
}

/// Apply override into the prefab file: the value goes into the file (the entity's record, or
/// for an entity of a nested prefab an outer-wins override on the nested instance record), the
/// file is written atomically in the current format, the template is baked again, this
/// instance's override goes, and every other live instance that does not override the field
/// takes the new value. One undo puts the file bytes, the override and the values back.
fn apply_override_to_file(world: &mut World, apply: FileApply<'_>) -> Result<EditReport, EditError> {
    let FileApply {
        entity,
        root,
        outer,
        local_id,
        entry,
        component,
        path,
        file_path,
        asset: file_asset,
    } = apply;
    let failed = |message: String| EditError::new("prefab_apply_failed", message);
    let old_text = std::fs::read_to_string(file_path)
        .map_err(|err| failed(format!("cannot read {}: {err}", file_path.display())))?;
    let mut file = SceneFile::from_ron_str(&old_text)
        .map_err(|err| failed(format!("cannot parse {}: {err}", file_path.display())))?;
    let reflect_path = strip_dot(&entry.path).to_string();
    let full = full_type_path(world, component).unwrap_or(entry.component.clone());
    let live_field = read_property(world, entity, &full, &reflect_path).map_err(map_command)?;
    let target = regia_core::id::Guid::from_u128(local_id);
    let registry_handle = world.resource::<AppTypeRegistry>().clone();
    if let Some(record) = file.entities.iter_mut().find(|record| record.guid == target) {
        // The entity is the file's own: patch its component value.
        let registry = registry_handle.read();
        let registration = registration(&registry, &full)
            .ok_or_else(|| EditError::new("component_on_none", format!("no reflected component {full}")))?;
        let key = record
            .components
            .keys()
            .find(|key| components_match(key, &full) || components_match(key, component))
            .cloned()
            .ok_or_else(|| {
                EditError::new(
                    "prefab_template_missing",
                    format!("the prefab entity has no {component} in {}", file_path.display()),
                )
            })?;
        let mut codec = ReflectCodec::new(&registry);
        let current = record
            .components
            .get(&key)
            .map(|raw| raw.get_ron().to_string())
            .unwrap_or_default();
        let mut value = codec
            .from_ron_typed(&current, registration)
            .map_err(|err| failed(format!("the file's {key} does not parse: {err}")))?;
        if reflect_path.is_empty() {
            value
                .try_apply(live_field.as_ref())
                .map_err(|err| EditError::new("value_mismatch", err.to_string()))?;
        } else {
            value
                .reflect_path_mut(reflect_path.as_str())
                .map_err(|_| EditError::new("path_unknown", format!("no field {reflect_path} on {key}")))?
                .try_apply(live_field.as_ref())
                .map_err(|err| EditError::new("value_mismatch", err.to_string()))?;
        }
        let text = codec
            .to_ron_typed(value.as_partial_reflect())
            .map_err(|err| failed(err.to_string()))?;
        // Trimmed like a parsed file's values, so the written bytes parse back to the same text.
        record.components.insert(key, raw_ron(text.trim())?);
    } else {
        // An entity of a nested prefab: the outermost nested instance below the root whose
        // record is in this file takes an override (outer wins); the inner file is untouched.
        let records: BTreeSet<regia_core::id::Guid> = file
            .entities
            .iter()
            .filter(|record| record.prefab_instance.is_some())
            .map(|record| record.guid)
            .collect();
        let holder = ancestors(world, entity)
            .into_iter()
            .take_while(|at| *at != outer)
            .filter_map(|at| world.get::<PrefabLink>(at).map(|link| link.guid()))
            .filter(|guid| records.contains(guid))
            .last()
            .ok_or_else(|| {
                failed(format!(
                    "{target} is not in {} or a prefab nested in it",
                    file_path.display()
                ))
            })?;
        let record = file
            .entities
            .iter_mut()
            .find(|record| record.guid == holder)
            .ok_or_else(|| failed("the nested record vanished".to_string()))?;
        let nested = record
            .prefab_instance
            .as_mut()
            .ok_or_else(|| failed(format!("record {holder} holds no nested prefab instance")))?;
        let value = raw_ron(entry.value.trim())?;
        nested.overrides.retain(|existing| {
            !(existing.target == target
                && components_match(&existing.component, component)
                && paths_match(&existing.path, path))
        });
        nested.overrides.push(regia_scene::prelude::PrefabOverride {
            target,
            component: entry.component.clone(),
            path: entry.path.clone(),
            value,
        });
    }
    let new_text = file.to_ron_string().map_err(|err| failed(err.to_string()))?;
    let instance = world
        .get::<PrefabInstance>(root)
        .cloned()
        .ok_or_else(|| EditError::new("not_prefab_instance", "the instance root lost PrefabInstance"))?;
    let asset = file_asset.unwrap_or_else(|| {
        world
            .get::<PrefabInstance>(outer)
            .map_or(instance.prefab_guid(), PrefabInstance::prefab_guid)
    });
    // Bake before writing: a file that does not bake is never written.
    {
        let registry = registry_handle.read();
        PrefabTemplate::from_scene_file(asset, &file, &registry).map_err(|err| failed(err.to_string()))?;
    }
    let mut write = WritePrefabFile {
        path: file_path.to_path_buf(),
        asset,
        old: old_text,
        new: new_text,
    };
    write.try_apply(world).map_err(failed)?;

    let old = instance.overrides.clone();
    let mut new = old.clone();
    new.retain(|candidate| {
        !(candidate.target == local_id
            && components_match(&candidate.component, component)
            && paths_match(&candidate.path, path))
    });
    if let Some(mut live) = world.get_mut::<PrefabInstance>(root) {
        live.overrides.clone_from(&new);
    }
    let mut commands: Vec<Box<dyn EditorCommand>> = vec![
        Box::new(write),
        Box::new(SetPrefabOverrides {
            entity: root.to_bits(),
            old,
            new,
        }),
    ];
    // Every other live copy of the entity that keeps no override of its own takes the value.
    let mut members: Vec<Entity> = {
        let mut query = world.query::<(Entity, &PrefabLink)>();
        query
            .iter(world)
            .filter(|(member, link)| link.local_id == local_id && *member != entity)
            .map(|(member, _)| member)
            .collect()
    };
    members.sort_unstable();
    let library = world.get_resource::<PrefabLibrary>().cloned().unwrap_or_default();
    for member in members {
        // An instance of the prefab, or of a variant built on it that leaves the field alone.
        let of_asset = follows_asset(world, &library, member, asset, component, path);
        if !of_asset || overridden_above(world, member, local_id, component, path) {
            continue;
        }
        let value = must_clone(live_field.as_ref())?;
        match SetProperty::capture(world, member, &full, &reflect_path, value) {
            Ok(mut property) => {
                property.coalesce = false;
                property.apply(world);
                commands.push(Box::new(property));
            }
            Err(err) => {
                tracing::warn!(target: "regia_editor", "apply override: instance {member:?} not updated: {err}");
            }
        }
    }
    let undo_depth = push_applied(
        world,
        commands,
        format!("Apply {component}.{path} to {}", file_path.display()),
        true,
    );
    Ok(EditReport {
        edited: vec![entity.to_bits()],
        skipped: Vec::new(),
        undo_depth,
    })
}

/// Writes a prefab file and bakes it again; undo writes the old bytes back.
struct WritePrefabFile {
    path: std::path::PathBuf,
    asset: regia_core::id::Guid,
    old: String,
    new: String,
}

impl WritePrefabFile {
    fn write(
        world: &mut World,
        path: &std::path::Path,
        asset: regia_core::id::Guid,
        text: &str,
    ) -> Result<(), String> {
        let file = SceneFile::from_ron_str(text).map_err(|err| err.to_string())?;
        let registry = world.resource::<AppTypeRegistry>().clone();
        let template =
            PrefabTemplate::from_scene_file(asset, &file, &registry.read()).map_err(|err| err.to_string())?;
        crate::atomic_write::write_atomic(path, text.as_bytes())?;
        world
            .get_resource_or_insert_with(crate::prefab_link::PrefabWriteEcho::default)
            .note(path, text);
        world
            .get_resource_or_insert_with::<PrefabLibrary>(PrefabLibrary::default)
            .insert(template);
        Ok(())
    }

    fn try_apply(&mut self, world: &mut World) -> Result<(), String> {
        Self::write(world, &self.path, self.asset, &self.new)
    }
}

impl EditorCommand for WritePrefabFile {
    fn apply(&mut self, world: &mut World) {
        if let Err(err) = Self::write(world, &self.path, self.asset, &self.new) {
            tracing::error!(target: "regia_editor", "redo apply to {}: {err}", self.path.display());
        }
    }

    fn revert(&mut self, world: &mut World) {
        if let Err(err) = Self::write(world, &self.path, self.asset, &self.old) {
            tracing::error!(target: "regia_editor", "undo apply to {}: {err}", self.path.display());
        }
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn changes_level_state(&self) -> bool {
        false
    }

    fn description(&self) -> String {
        format!("Write {}", self.path.display())
    }
}

/// JSON the agent shows for a multi-edit that has been checked and not yet written.
pub(crate) struct EditPreview {
    /// Holders that would be written, as entity bits.
    pub edited: Vec<u64>,
    /// Selected entities that lack the component.
    pub skipped: Vec<u64>,
    /// Each holder's current field, in `edited` order. An Euler axis is a number of degrees.
    pub befores: Vec<Value>,
    /// The value that would be written, in the same form as `befores`.
    pub after: Value,
    /// First holder, as entity bits.
    pub first: u64,
}

fn field_json_of(value: &dyn PartialReflect, world: &World) -> Value {
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    reflect_json(value, &registry)
}

/// Turns the agent's JSON `value` into the reflected value `edit_value` writes.
///
/// `rotation.ry` (and the other Euler lanes) is a number of degrees. `.r` / `.g` / `.b` / `.a`
/// are accepted on a vector or a color array.
///
/// # Errors
/// [`EditError`] with `entities_empty`, `not_found`, `component_on_none`, `path_unknown` or
/// `value_mismatch`.
pub(crate) fn decode_edit_value(
    world: &World,
    entities: &[Entity],
    component: &str,
    path: &str,
    value: &Value,
) -> Result<Box<dyn PartialReflect>, EditError> {
    if entities.is_empty() {
        return Err(EditError::new("entities_empty", "entities is empty"));
    }
    require_alive(world, entities)?;
    let holders = holders_of(world, entities, component);
    if holders.is_empty() {
        return Err(EditError::new(
            "component_on_none",
            format!("none of the entities has {component}"),
        ));
    }
    let path = rewrite_color_alias(world, holders[0], component, path);
    // Resolve the field before taking the registry lock: `read_property` locks it too, and the
    // lock is not re-entrant.
    let euler =
        euler_axis(&path).is_some_and(|(parent, _)| parent_is_quat(world, holders[0], component, parent));
    let current = if euler {
        None
    } else {
        Some(read_property(world, holders[0], component, &path).map_err(map_command)?)
    };
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let field_type = if let Some(current) = current.as_ref() {
        current
            .get_represented_type_info()
            .map(|info| info.type_id())
            .and_then(|type_id| registry.get(type_id))
            .ok_or_else(|| {
                EditError::new(
                    "value_mismatch",
                    format!("`{component}.{path}` has no registered type"),
                )
            })?
    } else {
        registry
            .get(std::any::TypeId::of::<f32>())
            .ok_or_else(|| EditError::new("value_mismatch", "f32 is not in the type registry"))?
    };
    if let Some(missing) = crate::agent_ext::first_missing_field(value, field_type.type_info(), &registry, "")
    {
        return Err(EditError::new(
            "value_mismatch",
            format!("`{missing}` is missing; give every field"),
        ));
    }
    let decoded = regia_ecs::reflect::serde::TypedReflectDeserializer::new(field_type, &registry)
        .deserialize(value)
        .map_err(|err| EditError::new("value_mismatch", err.to_string()))?;
    if !crate::agent_ext::reflected_is_finite(decoded.as_ref()) {
        return Err(EditError::new(
            "value_mismatch",
            "value has a non-finite number (inf, nan, or outside the f32 range)",
        ));
    }
    Ok(decoded)
}

/// Checks a multi-edit and reports the before/after JSON without writing.
///
/// # Errors
/// The same codes as [`edit_value`], including `feed_owned`.
pub(crate) fn preview_edit(
    world: &World,
    entities: &[Entity],
    component: &str,
    path: &str,
    new_value: &dyn PartialReflect,
) -> Result<EditPreview, EditError> {
    if entities.is_empty() {
        return Err(EditError::new("entities_empty", "entities is empty"));
    }
    require_alive(world, entities)?;
    let holders = holders_of(world, entities, component);
    if holders.is_empty() {
        return Err(EditError::new(
            "component_on_none",
            format!("none of the entities has {component}"),
        ));
    }
    refuse_feed_owned(world, &holders)?;
    let path = rewrite_color_alias(world, holders[0], component, path);
    let skipped = entities
        .iter()
        .filter(|entity| !holders.contains(entity))
        .map(|entity| entity.to_bits())
        .collect();
    let (befores, after) = if let Some((parent, axis)) = euler_axis(&path)
        && parent_is_quat(world, holders[0], component, parent)
    {
        let degrees = as_f32(new_value)
            .ok_or_else(|| EditError::new("value_mismatch", "an Euler axis takes a number of degrees"))?;
        let mut befores = Vec::with_capacity(holders.len());
        for entity in &holders {
            let current = read_quat(world, *entity, component, parent)?;
            befores.push(json!(quat_to_euler_deg(&current)[axis]));
        }
        (befores, json!(degrees))
    } else {
        let current = read_property(world, holders[0], component, &path).map_err(map_command)?;
        let mut trial = must_clone(current.as_ref())?;
        trial
            .try_apply(new_value)
            .map_err(|err| EditError::new("value_mismatch", err.to_string()))?;
        let mut befores = Vec::with_capacity(holders.len());
        for entity in &holders {
            let value = read_property(world, *entity, component, &path).map_err(map_command)?;
            befores.push(field_json_of(value.as_ref(), world));
        }
        (befores, field_json_of(new_value, world))
    };
    Ok(EditPreview {
        first: holders[0].to_bits(),
        edited: holders.iter().map(|entity| entity.to_bits()).collect(),
        skipped,
        befores,
        after,
    })
}

/// Overrides are shown per selected instance so Revert and Apply have an explicit target.
///
/// `marked` holds the field paths whose row already drew an override marker with the Revert/Apply
/// menu (the single-entity inspector, ADR-7744): those get no second row here. Every other
/// override (the whole component, a list element or vector lane with no row of its own, any field
/// of an extension-drawn or Lua component) keeps its row.
pub(super) fn prefab_override_controls(
    ui: &mut egui::Ui,
    world: &mut World,
    entities: &[Entity],
    component: &str,
    marked: &BTreeSet<String>,
) {
    let mut rows = Vec::new();
    for entity in entities {
        let Ok((link, root)) = link_and_root(world, *entity) else {
            continue;
        };
        let Some(instance) = world.get::<PrefabInstance>(root) else {
            continue;
        };
        for entry in &instance.overrides {
            if entry.target == link.local_id
                && components_match(&entry.component, component)
                && !marked.contains(strip_dot(&entry.path))
            {
                rows.push((*entity, entry.path.clone()));
            }
        }
    }
    for (entity, path) in rows {
        ui.push_id((entity.to_bits(), &path), |ui| {
            ui.horizontal(|ui| {
                ui.strong(format!("Override {} · {path}", entity.to_bits()));
                if ui.small_button("Revert").clicked()
                    && !ui.ctx().will_discard()
                    && let Err(err) = revert_prefab_override(world, entity, component, &path)
                {
                    tracing::warn!(target: "regia_editor", "revert override: {err}");
                }
                if ui
                    .small_button("Apply")
                    .on_hover_text(
                        "Apply to the prefab: writes its .prefab.ron when the project has it, and other instances without an override follow (one undo)",
                    )
                    .clicked()
                    && !ui.ctx().will_discard()
                    && let Err(err) = apply_prefab_override(world, entity, component, &path)
                {
                    tracing::warn!(target: "regia_editor", "apply override: {err}");
                }
                // Every override of the instance, with a choice of where Apply writes (ADR-7790).
                if crate::agent_parity::action_button(ui, &crate::agent_parity::INSPECTOR_OPEN_OVERRIDES)
                    .on_hover_text("Every override of this instance, prefab and instance values side by side, and where Apply writes")
                    .clicked()
                    && !ui.ctx().will_discard()
                {
                    crate::prefab_overrides_panel::open_for(world, entity);
                }
            });
        });
    }
}

/// The field paths of `component` that `entity`'s prefab instance overrides (`intensity`, not the
/// whole-component entry): what the field rows mark (ADR-7744). Empty outside a prefab instance.
pub(super) fn overridden_field_paths(world: &World, entity: Entity, component: &str) -> Vec<String> {
    let Ok((link, root)) = link_and_root(world, entity) else {
        return Vec::new();
    };
    let Some(instance) = world.get::<PrefabInstance>(root) else {
        return Vec::new();
    };
    instance
        .overrides
        .iter()
        .filter(|entry| entry.target == link.local_id && components_match(&entry.component, component))
        .map(|entry| strip_dot(&entry.path).to_owned())
        .filter(|path| !path.is_empty())
        .collect()
}

/// Replaces one loaded prefab template, retaining both scene descriptions for undo and redo.
/// The files are validated before this command is pushed; replay errors are reported to the log.
struct SetPrefabTemplate {
    asset: regia_core::id::Guid,
    old: regia_scene::format::SceneFile,
    new: regia_scene::format::SceneFile,
}

impl SetPrefabTemplate {
    fn write(world: &mut World, asset: regia_core::id::Guid, file: &regia_scene::format::SceneFile) {
        let registry = world.resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        match regia_scene::prefab::PrefabTemplate::from_scene_file(asset, file, &registry) {
            Ok(template) if template.warnings.is_empty() => {
                drop(registry);
                world
                    .resource_mut::<regia_scene::prefab::PrefabLibrary>()
                    .insert(template);
            }
            Ok(template) => {
                tracing::warn!(target: "regia_editor", "prefab template replay: {:?}", template.warnings)
            }
            Err(err) => tracing::warn!(target: "regia_editor", "prefab template replay: {err}"),
        }
    }
}

impl EditorCommand for SetPrefabTemplate {
    fn apply(&mut self, world: &mut World) {
        Self::write(world, self.asset, &self.new);
    }
    fn revert(&mut self, world: &mut World) {
        Self::write(world, self.asset, &self.old);
    }
    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }
    fn description(&self) -> String {
        "Apply prefab field".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Reflect, Clone, Debug, PartialEq)]
    enum Shape {
        Capsule(f32, f32),
    }

    #[derive(Reflect, Clone, Debug, PartialEq)]
    struct Holder {
        shape: Shape,
    }

    #[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
    #[reflect(Component, Default)]
    struct Tag {
        level: u32,
    }

    fn tag_world() -> World {
        let mut world = World::new();
        world.init_resource::<AppTypeRegistry>();
        world.resource::<AppTypeRegistry>().write().register::<Tag>();
        world.insert_resource(UndoStack::new());
        world
    }

    /// Corr editor-a finding 6: an entity listed twice is one target. The second remove used to
    /// fail after the first had applied, so the component was gone with an error and no undo.
    /// Audit (sprint-corr-editor-a U1): the short-name fallback took `a::Prop` and `b::Prop` for
    /// one component, so an edit of one overwrote (and renamed) the other's override entry.
    #[test]
    fn same_short_names_from_two_crates_keep_their_own_overrides() {
        let mut overrides = vec![OverrideEntry {
            target: 1,
            component: "a::Prop".to_string(),
            path: ".x".to_string(),
            value: "1".to_string(),
        }];
        upsert_override(&mut overrides, 1, "b::Prop", ".x", "2".to_string());
        assert_eq!(overrides.len(), 2, "{overrides:?}");
        assert_eq!(overrides[0].component, "a::Prop");
        assert_eq!(overrides[0].value, "1");
        assert_eq!(overrides[1].component, "b::Prop");
        // A short spelling (an older file, or a hand edit) still finds its full-path entry.
        assert!(components_match("Prop", "a::Prop"));
        assert!(components_match("a::Prop", "Prop"));
        assert!(!components_match("a::Prop", "b::Prop"));
    }

    #[test]
    fn a_duplicate_entity_removes_once_and_undoes() {
        let mut world = tag_world();
        let a = world.spawn(Tag { level: 3 }).id();
        let report = remove_component_from(&mut world, &[a, a], "Tag").expect("remove");
        assert_eq!(report.edited, vec![a.to_bits()]);
        assert!(world.get::<Tag>(a).is_none());
        world.resource_scope(|world, mut stack: Mut<UndoStack>| {
            assert!(stack.undo(world));
        });
        assert_eq!(world.get::<Tag>(a), Some(&Tag { level: 3 }));

        let report = add_component_to(&mut world, &[a, a], "Tag").expect("nothing to add");
        assert!(report.edited.is_empty());
        let b = world.spawn_empty().id();
        let report = add_component_to(&mut world, &[b, b], "Tag").expect("add");
        assert_eq!(report.edited, vec![b.to_bits()]);
        assert_eq!(world.get::<Tag>(b), Some(&Tag::default()));
    }

    #[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
    #[reflect(Component, Default)]
    struct Pair {
        a: f32,
        b: f32,
    }

    /// Corr editor-a finding 8: an extension drawer that changes one field of the sample writes
    /// that field on every holder; the fields that differ between holders stay their own.
    #[test]
    fn an_extension_edit_writes_only_the_fields_it_changed() {
        let mut world = World::new();
        world.init_resource::<AppTypeRegistry>();
        world.resource::<AppTypeRegistry>().write().register::<Pair>();
        world.insert_resource(UndoStack::new());
        let first = world.spawn(Pair { a: 1.0, b: 1.0 }).id();
        let second = world.spawn(Pair { a: 2.0, b: 2.0 }).id();
        let original = Pair { a: 1.0, b: 1.0 };
        let edited = Pair { a: 5.0, b: 1.0 };
        let changes = extension_changes(&original, &edited);
        assert_eq!(changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), ["a"]);
        assert!(extension_changes(&original, &original).is_empty());
        commit_widget_changes(&mut world, &[first, second], "Pair", &edited, &changes).expect("commit");
        assert_eq!(world.get::<Pair>(first), Some(&Pair { a: 5.0, b: 1.0 }));
        assert_eq!(
            world.get::<Pair>(second),
            Some(&Pair { a: 5.0, b: 2.0 }),
            "the second holder keeps its own b"
        );
    }

    /// Corr editor-a finding 4: the fields of a tuple variant are `.0`, `.1`, ... A difference
    /// in the second field used to be reported as `shape.0`, so the drawer marked the wrong field
    /// mixed and showed the primary's value for the one that differs.
    #[test]
    fn a_tuple_variant_field_is_mixed_under_its_own_position() {
        let sample = Holder {
            shape: Shape::Capsule(1.0, 2.0),
        };
        let other = Holder {
            shape: Shape::Capsule(1.0, 3.0),
        };
        let mixed = mixed_paths(&sample, &[&other]);
        assert_eq!(
            mixed.into_iter().collect::<Vec<_>>(),
            ["shape.1"],
            "only the second field differs"
        );
    }
}
