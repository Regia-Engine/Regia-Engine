// SPDX-License-Identifier: MPL-2.0
//! `spawn.prefab` attribute overrides on spawned instances (spec 63 § 13.2, ADR-2724).
//!
//! A run records, per spawned prefab, the point's attribute values for each `overrides` row
//! ([`crate::exec::PrefabSpawn::overrides`]). Once the instance exists, the host calls
//! [`apply_prefab_overrides`], which finds the component on the instance root (or else on the first
//! descendant that has it, depth first in child order), converts the value to the field's reflected
//! type and patches the field through `regia_scene::prefab::apply_override`. Instances that resolve
//! later (`PendingPrefab`) carry a [`ProcPrefabOverrides`] component; the
//! [`apply_pending_prefab_overrides`] system applies it when the prefab is in and removes it.
//!
//! Conversion (spec 62 § 3.5): a scalar writes into an `f32`, `f64`, integer (integral values only)
//! or `bool` field; a 2-, 3- or 4-component value writes into a struct (`x, y, z`), tuple struct,
//! tuple or array with that many numeric fields; names and assets write into `String` fields.

use regia_app::prelude::*;
use regia_ecs::reflect::{
    AppTypeRegistry, GetPath, PartialReflect, ReflectComponent, ReflectRef, TypeInfo, TypeRegistry,
};
use regia_scene::prefab::{PendingPrefab, apply_override};

use crate::data::AttrValue;
use crate::exec::PrefabFieldOverride;
use crate::graph::{CompiledGraph, GraphError};

/// Overrides waiting for a pending prefab instance (`PendingPrefab`) to resolve.
#[derive(Component, Clone, Debug, PartialEq)]
pub struct ProcPrefabOverrides {
    /// The prefab asset, for messages.
    pub prefab: String,
    /// The overrides, in setting order.
    pub overrides: Vec<PrefabFieldOverride>,
}

/// An override that could not be applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverrideFailure {
    /// `override_path_unknown` (no such component on the instance, or no such field),
    /// `override_type_mismatch` (the value does not convert to the field's type) or
    /// `override_failed` (the reflected patch was refused).
    pub code: &'static str,
    /// Detail naming the prefab and the path.
    pub message: String,
}

/// Applies `overrides` to the instance rooted at `entity`; returns one failure per override that
/// could not be applied (the others still apply).
pub fn apply_prefab_overrides(
    world: &mut World,
    entity: Entity,
    prefab: &str,
    overrides: &[PrefabFieldOverride],
    registry: &TypeRegistry,
) -> Vec<OverrideFailure> {
    let mut failures = Vec::new();
    for o in overrides {
        if let Err(f) = apply_one(world, entity, prefab, o, registry) {
            failures.push(f);
        }
    }
    failures
}

fn apply_one(
    world: &mut World,
    entity: Entity,
    prefab: &str,
    o: &PrefabFieldOverride,
    registry: &TypeRegistry,
) -> Result<(), OverrideFailure> {
    let unknown = |what: String| OverrideFailure {
        code: "override_path_unknown",
        message: format!("prefab `{prefab}`: {what}"),
    };
    let registration = registry
        .get_with_type_path(&o.component)
        .or_else(|| registry.get_with_short_type_path(&o.component))
        .ok_or_else(|| unknown(format!("no registered component `{}`", o.component)))?;
    let type_path = registration.type_info().type_path();
    let reflect = registration
        .data::<ReflectComponent>()
        .ok_or_else(|| unknown(format!("`{}` is not a reflected component", o.component)))?;
    let target = find_with(world, entity, reflect)
        .ok_or_else(|| unknown(format!("the instance has no `{}`", o.component)))?;
    let ron = {
        let entity_ref = world
            .get_entity(target)
            .map_err(|_| unknown(format!("instance entity {target:?} is gone")))?;
        let current = reflect
            .reflect(entity_ref)
            .ok_or_else(|| unknown(format!("the instance has no `{}`", o.component)))?;
        let field = current
            .reflect_path(o.field.as_str())
            .map_err(|_| unknown(format!("`{}` has no field `{}`", o.component, o.field)))?;
        value_ron(&o.value, field).map_err(|m| OverrideFailure {
            code: "override_type_mismatch",
            message: format!("prefab `{prefab}`: `{}.{}`: {m}", o.component, o.field),
        })?
    };
    // A struct field may deserialize from its named form or, when its type uses its own serde form
    // (glam vectors), from the positional one: try them in that order.
    let mut last = String::new();
    for text in &ron {
        match apply_override(world, target, type_path, &o.field, text, registry) {
            Ok(()) => return Ok(()),
            Err(m) => last = m,
        }
    }
    Err(OverrideFailure {
        code: "override_failed",
        message: format!("prefab `{prefab}`: `{}.{}`: {last}", o.component, o.field),
    })
}

/// The instance root if it has the component, else the first descendant that has it (depth first,
/// in child order).
fn find_with(world: &World, root: Entity, reflect: &ReflectComponent) -> Option<Entity> {
    let mut stack = vec![root];
    let mut seen = std::collections::BTreeSet::new();
    while let Some(e) = stack.pop() {
        if !seen.insert(e) {
            continue;
        }
        let Ok(entity_ref) = world.get_entity(e) else {
            continue;
        };
        if reflect.reflect(entity_ref).is_some() {
            return Some(e);
        }
        if let Some(children) = world.get::<Children>(e) {
            let kids: Vec<Entity> = children.iter().collect();
            stack.extend(kids.into_iter().rev());
        }
    }
    None
}

/// Whether `type_path` is an integer scalar the conversion writes.
fn is_int_path(type_path: &str) -> bool {
    matches!(
        type_path,
        "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize"
    )
}

/// `i` as RON when it fits `type_path`. Never goes through `f64` (that rounds past 2^53).
fn integer_from_i64(type_path: &str, i: i64) -> Result<String, String> {
    let text = match type_path {
        "i8" => i8::try_from(i).map(|n| n.to_string()),
        "i16" => i16::try_from(i).map(|n| n.to_string()),
        "i32" => i32::try_from(i).map(|n| n.to_string()),
        "i64" => Ok(i.to_string()),
        "isize" => isize::try_from(i).map(|n| n.to_string()),
        "u8" => u8::try_from(i).map(|n| n.to_string()),
        "u16" => u16::try_from(i).map(|n| n.to_string()),
        "u32" => u32::try_from(i).map(|n| n.to_string()),
        "u64" => u64::try_from(i).map(|n| n.to_string()),
        "usize" => usize::try_from(i).map(|n| n.to_string()),
        _ => return Err(format!("cannot write an integer into `{type_path}`")),
    };
    text.map_err(|_| format!("{i} does not fit `{type_path}`"))
}

/// An integer float that is exactly an `i64`. `2^63` round-trips through a saturating cast.
fn exact_i64(x: f64) -> Option<i64> {
    if x.is_finite()
        && x.fract() == 0.0
        && x >= i64::MIN as f64
        && x < 9223372036854775808.0
        && (x as i64) as f64 == x
    {
        Some(x as i64)
    } else {
        None
    }
}

/// An integer float that is exactly a `u64`. `2^64` round-trips through a saturating cast.
fn exact_u64(x: f64) -> Option<u64> {
    if x.is_finite()
        && x.fract() == 0.0
        && (0.0..18446744073709551616.0).contains(&x)
        && (x as u64) as f64 == x
    {
        Some(x as u64)
    } else {
        None
    }
}

/// A scalar as RON for a field of type `type_path`.
fn scalar_ron(type_path: &str, x: f64) -> Result<String, String> {
    match type_path {
        "f32" => {
            if !x.is_finite() {
                return Err(format!("{x} is not a finite `f32`"));
            }
            let y = x as f32;
            if !y.is_finite() {
                return Err(format!("{x} does not fit `f32`"));
            }
            Ok(format!("{y:?}"))
        }
        "f64" => {
            if !x.is_finite() {
                return Err(format!("{x} is not a finite `f64`"));
            }
            Ok(format!("{x:?}"))
        }
        "bool" => {
            if !x.is_finite() {
                return Err(format!("{x} is not a finite `bool`"));
            }
            Ok((x != 0.0).to_string())
        }
        "i8" | "i16" | "i32" | "i64" | "isize" => {
            let Some(i) = exact_i64(x) else {
                return Err(format!("{x} is not an integer for `{type_path}`"));
            };
            integer_from_i64(type_path, i)
        }
        "u8" | "u16" | "u32" | "u64" | "usize" => {
            let Some(u) = exact_u64(x) else {
                return Err(format!("{x} is not an integer for `{type_path}`"));
            };
            let text = match type_path {
                "u8" => u8::try_from(u).map(|n| n.to_string()),
                "u16" => u16::try_from(u).map(|n| n.to_string()),
                "u32" => u32::try_from(u).map(|n| n.to_string()),
                "u64" => Ok(u.to_string()),
                "usize" => usize::try_from(u).map(|n| n.to_string()),
                _ => return Err(format!("cannot write a number into `{type_path}`")),
            };
            text.map_err(|_| format!("{x} does not fit `{type_path}`"))
        }
        other => Err(format!("cannot write a number into `{other}`")),
    }
}

/// The value as RON candidates for `field`'s reflected type (a struct gives its named, then its
/// positional form).
fn value_ron(value: &AttrValue, field: &dyn PartialReflect) -> Result<Vec<String>, String> {
    let path = field.reflect_type_path();
    // An integer attribute stays an integer. `i as f64` rounds past 2^53 and turns `i64::MAX`
    // into 2^63, which no longer fits an `i64` field and is one past `i64::MAX` on a `u64`.
    if let AttrValue::Int(i) = value
        && matches!(field.reflect_ref(), ReflectRef::Opaque(_))
        && is_int_path(path)
    {
        return integer_from_i64(path, *i).map(|r| vec![r]);
    }
    let comps: Vec<f64> = match value {
        AttrValue::Name(t) | AttrValue::Asset(t) => {
            return if path == "alloc::string::String" || path == "String" {
                Ok(vec![format!("{t:?}")])
            } else {
                Err(format!("a name writes only into a `String`, not `{path}`"))
            };
        }
        AttrValue::Bool(b) => vec![f64::from(u8::from(*b))],
        AttrValue::Int(i) => vec![*i as f64],
        AttrValue::Float(x) => vec![*x],
        AttrValue::Vec2(v) => v.to_vec(),
        AttrValue::Vec3(v) => v.to_vec(),
        AttrValue::Vec4(v) | AttrValue::Quat(v) => v.to_vec(),
        other => return Err(format!("{other:?} has no field conversion")),
    };
    let n = comps.len();
    let mismatch = format!("a {n}-component value does not fit `{path}`");
    match field.reflect_ref() {
        ReflectRef::Opaque(_) if n == 1 => scalar_ron(path, comps[0]).map(|r| vec![r]),
        // Only 2-4 component values write into structs (spec 62 § 3.5, the compile check's
        // `field_shape`): a scalar never fills a one-field newtype.
        ReflectRef::Struct(s) if n >= 2 && s.field_len() == n => {
            let values = numeric_fields(
                &(0..n).map(|i| s.field_at(i)).collect::<Vec<_>>(),
                &comps,
                &mismatch,
            )?;
            let named: Vec<String> = (0..n)
                .zip(&values)
                .map(|(i, v)| format!("{}: {v}", s.name_at(i).unwrap_or("")))
                .collect();
            Ok(vec![
                format!("({})", named.join(", ")),
                format!("({})", values.join(", ")),
            ])
        }
        ReflectRef::TupleStruct(t) if n >= 2 && t.field_len() == n => {
            let parts: Vec<_> = (0..n).map(|i| t.field(i)).collect();
            Ok(vec![format!(
                "({})",
                numeric_fields(&parts, &comps, &mismatch)?.join(", ")
            )])
        }
        ReflectRef::Tuple(t) if n >= 2 && t.field_len() == n => {
            let parts: Vec<_> = (0..n).map(|i| t.field(i)).collect();
            Ok(vec![format!(
                "({})",
                numeric_fields(&parts, &comps, &mismatch)?.join(", ")
            )])
        }
        ReflectRef::Array(a) if n >= 2 && a.len() == n => {
            let parts: Vec<_> = (0..n).map(|i| a.get(i)).collect();
            Ok(vec![format!(
                "({})",
                numeric_fields(&parts, &comps, &mismatch)?.join(", ")
            )])
        }
        _ => Err(mismatch),
    }
}

/// Each component as RON for its field's scalar type.
fn numeric_fields(
    parts: &[Option<&dyn PartialReflect>],
    comps: &[f64],
    mismatch: &str,
) -> Result<Vec<String>, String> {
    parts
        .iter()
        .zip(comps)
        .map(|(f, x)| match f {
            Some(f) => scalar_ron(f.reflect_type_path(), *x),
            None => Err(mismatch.to_string()),
        })
        .collect()
}

/// Applies and removes [`ProcPrefabOverrides`] on instances whose prefab has resolved (no
/// `PendingPrefab` left); failures are logged as warnings with their code.
pub fn apply_pending_prefab_overrides(world: &mut World) {
    let ready: Vec<(Entity, ProcPrefabOverrides)> = world
        .query_filtered::<(Entity, &ProcPrefabOverrides), Without<PendingPrefab>>()
        .iter(world)
        .map(|(e, o)| (e, o.clone()))
        .collect();
    if ready.is_empty() {
        return;
    }
    let _span = tracing::info_span!("procgen.prefab_overrides", instances = ready.len()).entered();
    let Some(handle) = world.get_resource::<AppTypeRegistry>().map(|r| r.0.clone()) else {
        return;
    };
    let registry = handle.read();
    for (e, o) in ready {
        for f in apply_prefab_overrides(world, e, &o.prefab, &o.overrides, &registry) {
            tracing::warn!(target: "regia_procgen", code = f.code, "{}", f.message);
        }
        if let Ok(mut entity) = world.get_entity_mut(e) {
            entity.remove::<ProcPrefabOverrides>();
        }
    }
}

/// Adds [`apply_pending_prefab_overrides`] to `Update` (the generator plugin's hosts add it; ADR-2724).
pub struct ProcPrefabOverridesPlugin;

impl Plugin for ProcPrefabOverridesPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, apply_pending_prefab_overrides);
    }
}

/// What an attribute can be written into, from the field's reflected type alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldShape {
    /// A scalar (`f32`, `f64`, an integer or `bool`).
    Scalar,
    /// `n` numeric fields (struct, tuple struct, tuple or array of 2-4).
    Numeric(usize),
    /// A `String` (names and assets).
    Text,
}

/// Is `type_path` a scalar the conversion writes?
fn is_scalar(type_path: &str) -> bool {
    matches!(
        type_path,
        "f32"
            | "f64"
            | "bool"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "usize"
    )
}

/// The writable shape of a field type, or `None` when no attribute converts into it.
fn field_shape(info: &TypeInfo) -> Option<FieldShape> {
    let path = info.type_path();
    if path == "alloc::string::String" || path == "String" {
        return Some(FieldShape::Text);
    }
    if is_scalar(path) {
        return Some(FieldShape::Scalar);
    }
    let all_scalar = |paths: Vec<&str>| (2..=4).contains(&paths.len()) && paths.iter().all(|p| is_scalar(p));
    let paths: Vec<&str> = match info {
        TypeInfo::Struct(s) => s.iter().map(|f| f.type_path()).collect(),
        TypeInfo::TupleStruct(t) => t.iter().map(|f| f.type_path()).collect(),
        TypeInfo::Tuple(t) => t.iter().map(|f| f.type_path()).collect(),
        TypeInfo::Array(a) => vec![a.item_ty().path(); a.capacity()],
        _ => return None,
    };
    let n = paths.len();
    all_scalar(paths).then_some(FieldShape::Numeric(n))
}

/// Follows `field` (`a.b.0`, or `a.tint[1]` as the apply side's `reflect_path` writes an
/// element) from `root` through struct, tuple and array type info.
fn field_info(registry: &TypeRegistry, root: &'static TypeInfo, field: &str) -> Option<&'static TypeInfo> {
    let mut info = root;
    let dotted = field.replace('[', ".").replace(']', "");
    for seg in dotted.split('.') {
        let index = seg.parse::<usize>().ok();
        let (static_info, id) = match info {
            TypeInfo::Struct(s) => {
                let f = s.field(seg)?;
                (f.type_info(), f.type_id())
            }
            TypeInfo::TupleStruct(t) => {
                let f = t.field_at(index?)?;
                (f.type_info(), f.type_id())
            }
            TypeInfo::Tuple(t) => {
                let f = t.field_at(index?)?;
                (f.type_info(), f.type_id())
            }
            TypeInfo::Array(a) if index? < a.capacity() => (a.item_info(), a.item_ty().id()),
            _ => return None,
        };
        info = static_info.or_else(|| registry.get_type_info(id))?;
    }
    Some(info)
}

/// The component count of an attribute selector when it is fixed by the selector alone (a `:xyz`
/// part list or a built-in point property); `None` for custom attributes, whose type is only known
/// when the graph runs.
fn selector_width(sel: &crate::data::Selector) -> Option<usize> {
    if !sel.parts.is_empty() {
        return Some(sel.parts.len());
    }
    if sel.item_scope {
        return None;
    }
    match sel.name.as_str() {
        "density" | "seed" | "hardness" => Some(1),
        "position" | "scale" | "bounds_min" | "bounds_max" => Some(3),
        "rotation" | "color" => Some(4),
        _ => None,
    }
}

/// Checks the `overrides` rows of every `spawn.prefab` node of `compiled` against the reflected types
/// in `registry` (spec 63 § 13.2, ADR-3027): the graph-compile half of the checks
/// [`apply_prefab_overrides`] repeats on each instance.
///
/// Each problem is a [`GraphError`] naming the node and the path: `override_path_unknown` (no
/// registered type of that name, not a reflected component, or no such field) or
/// `override_type_mismatch` (the field's type takes no attribute value, or a built-in point property
/// or `:xyz` selector has another component count than the field). Rows whose syntax is already
/// wrong are left to the node's own check. Graphs run through `graph.call` are compiled at run time
/// and are checked when their instances are spawned.
pub fn check_compiled_overrides(compiled: &CompiledGraph, registry: &TypeRegistry) -> Vec<GraphError> {
    let mut errors = Vec::new();
    for n in compiled.nodes.iter().filter(|n| n.kind.id() == "spawn.prefab") {
        let Ok(specs) = crate::nodes::spawn::parse_overrides(n.settings.get("overrides")) else {
            continue;
        };
        for spec in specs {
            let path = format!("{}.{}", spec.component, spec.field);
            let unknown = |what: String| {
                GraphError::new(
                    Some(n.id),
                    "override_path_unknown",
                    format!("override path `{path}`: {what}"),
                )
            };
            let Some(registration) = registry
                .get_with_type_path(&spec.component)
                .or_else(|| registry.get_with_short_type_path(&spec.component))
            else {
                errors.push(unknown(format!("no registered type `{}`", spec.component)));
                continue;
            };
            if registration.data::<ReflectComponent>().is_none() {
                errors.push(unknown(format!(
                    "`{}` is not a reflected component",
                    spec.component
                )));
                continue;
            }
            let Some(info) = field_info(registry, registration.type_info(), &spec.field) else {
                errors.push(unknown(format!(
                    "`{}` has no field `{}`",
                    spec.component, spec.field
                )));
                continue;
            };
            let mismatch = |what: String| {
                GraphError::new(
                    Some(n.id),
                    "override_type_mismatch",
                    format!("override path `{path}`: {what}"),
                )
            };
            let Some(shape) = field_shape(info) else {
                errors.push(mismatch(format!(
                    "a field of type `{}` takes no attribute value",
                    info.type_path()
                )));
                continue;
            };
            let wanted = match shape {
                FieldShape::Scalar => Some(1),
                FieldShape::Numeric(k) => Some(k),
                FieldShape::Text => None,
            };
            match (selector_width(&spec.attr), wanted) {
                (Some(have), Some(want)) if have != want => errors.push(mismatch(format!(
                    "attribute `{}` has {have} components, the field `{}` takes {want}",
                    spec.attr.name,
                    info.type_path()
                ))),
                (Some(have), None) => errors.push(mismatch(format!(
                    "attribute `{}` is numeric ({have} components), the field is a `String`",
                    spec.attr.name
                ))),
                _ => {}
            }
        }
    }
    errors
}
