// SPDX-License-Identifier: MPL-2.0
//! The agent surface of the Inspector's pickers (spec addendum § 7.10-7.12, ADR-7786):
//! `editor.asset_field_set` (with `from_selection`), `editor.make_unique`,
//! `editor.asset_referencer_count`, `editor.curve_field_edit` and `editor.color_swatches`.
//! `book/agents/inspector.md` documents them and their error codes.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, ReflectMut};
use serde_json::{Value, json};

use super::curve_drawer::{CurvePoint, GradientStop, Keys, ShapeKind};
use super::picker_links;
use crate::data_panels::PanelError;
use crate::viewport_agent::Handler;

const HINT: &str = "see book/agents/inspector.md";

fn err(code: &str, message: impl Into<String>) -> RpcError {
    PanelError::new(code, message).rpc(HINT)
}

fn text<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::wrong_type(key, "a string"))
}

/// `editor.asset_field_set`.
fn asset_field_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::undo_agent::live_entity(world, params, "entity")?;
    let component = text(params, "component")?;
    let field = text(params, "field")?;
    let from_selection = params
        .get("from_selection")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let path = params.get("path").and_then(Value::as_str);
    let registry = world.resource::<AppTypeRegistry>().clone();
    let filter = {
        let registry = registry.read();
        super::asset_field_agent::field_filter(&registry, component, field).map_err(|e| e.rpc(HINT))?
    };
    let wanted = match (from_selection, path) {
        (true, Some(_)) => {
            return Err(err(
                "asset_field_bad_params",
                "give `path` or `from_selection`, not both",
            ));
        }
        (false, None) => {
            return Err(err(
                "asset_field_bad_params",
                "give `path` (\"\" clears the field) or `from_selection: true`",
            ));
        }
        (true, None) => world
            .get_resource::<picker_links::ContentSelection>()
            .and_then(|selection| selection.0.clone())
            .ok_or_else(|| {
                err(
                    "asset_field_no_selection",
                    "no asset is selected in the Content Browser",
                )
            })?,
        (false, Some(path)) => path.to_owned(),
    };
    if !wanted.is_empty() {
        super::impls::asset_drop_verdict(filter, &wanted, crate::dnd::kind_of_path(&wanted))
            .map_err(|reason| err("asset_field_refused", reason))?;
    }
    let before = crate::command::read_property(world, entity, component, field)
        .map_err(|e| err("asset_field_unknown", e.to_string()))?
        .try_downcast_ref::<String>()
        .cloned()
        .unwrap_or_default();
    let mut undoable = world
        .get_resource_or_insert_with(crate::command::UndoStack::new)
        .undo_depth();
    if before != wanted {
        let report = super::multi::edit_value(world, &[entity], component, field, Box::new(wanted.clone()))
            .map_err(|e| err(e.code, e.message))?;
        undoable = report.undo_depth;
    }
    Ok(json!({
        "filter": filter,
        "before": before,
        "after": wanted,
        "changed": before != wanted,
        "undoable": undoable,
    }))
}

/// `editor.make_unique`.
fn make_unique(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::undo_agent::live_entity(world, params, "entity")?;
    let component = text(params, "component")?;
    let path = text(params, "path")?;
    let made = super::make_unique::make_unique(world, entity, component, path).map_err(|e| e.rpc(HINT))?;
    Ok(json!({
        "from": made.from,
        "to": made.to,
        "value": made.value,
        "undoable": made.undoable,
    }))
}

/// Most referencers one answer lists (the rest are counted in `count`).
pub const MAX_REFERENCERS: usize = 200;

/// `editor.asset_referencer_count`.
fn asset_referencer_count(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = text(params, "path")?;
    let rel = super::impls::asset_rel_path(path)
        .ok_or_else(|| err("asset_path_invalid", "give a project asset path, not a built-in"))?;
    let found = picker_links::referencers(world, &rel);
    let generation = world
        .get_resource::<picker_links::ReferencerCache>()
        .map_or(0, |cache| cache.generation);
    Ok(json!({
        "path": rel,
        "count": found.len(),
        "shared": found.len() > 1,
        "referencers": found.iter().take(MAX_REFERENCERS).collect::<Vec<_>>(),
        "generation": generation,
    }))
}

fn keys_json(keys: &Keys) -> Value {
    match keys {
        Keys::Curve(points) => Value::Array(
            points
                .iter()
                .map(|p| match p.tangents {
                    Some((left, right)) => json!({ "t": p.time, "v": p.value, "in": left, "out": right }),
                    None => json!({ "t": p.time, "v": p.value }),
                })
                .collect(),
        ),
        Keys::Gradient(stops) => Value::Array(
            stops
                .iter()
                .map(|s| json!({ "t": s.time, "color": s.color, "hex": super::color::to_hex(s.color, true) }))
                .collect(),
        ),
    }
}

/// A color from JSON: `"#rrggbb[aa]"` or `[r, g, b]` / `[r, g, b, a]` linear.
fn color_param(value: &Value) -> Option<[f32; 4]> {
    if let Some(hex) = value.as_str() {
        return super::color::parse_hex(hex);
    }
    let channels: Vec<f32> = value
        .as_array()?
        .iter()
        .map(|c| c.as_f64().map(|c| c as f32))
        .collect::<Option<_>>()?;
    match channels[..] {
        [r, g, b] => Some([r, g, b, 1.0]),
        [r, g, b, a] => Some([r, g, b, a]),
        _ => None,
    }
    .filter(|c| c.iter().all(|x| x.is_finite()))
}

fn keys_param(kind: ShapeKind, value: &Value) -> Result<Keys, RpcError> {
    let items = value
        .as_array()
        .ok_or_else(|| RpcError::wrong_type("keys", "an array of keys"))?;
    let num = |item: &Value, key: &str| item.get(key).and_then(Value::as_f64).map(|v| v as f32);
    match kind {
        ShapeKind::Curve { tangents, .. } => items
            .iter()
            .map(|item| {
                let (Some(time), Some(value)) = (num(item, "t"), num(item, "v")) else {
                    return Err(err(
                        "curve_key_invalid",
                        format!("a curve key needs `t` and `v`: {item}"),
                    ));
                };
                Ok(CurvePoint {
                    time,
                    value,
                    tangents: tangents
                        .then(|| (num(item, "in").unwrap_or(0.0), num(item, "out").unwrap_or(0.0))),
                })
            })
            .collect::<Result<_, _>>()
            .map(Keys::Curve),
        ShapeKind::Gradient(_) => items
            .iter()
            .map(|item| {
                let time = num(item, "t");
                let color = item.get("color").and_then(color_param);
                match (time, color) {
                    (Some(time), Some(color)) => Ok(GradientStop { time, color }),
                    _ => Err(err(
                        "curve_key_invalid",
                        format!(
                            "a gradient stop needs `t` and `color` (\"#rrggbb\" or [r, g, b(, a)]): {item}"
                        ),
                    )),
                }
            })
            .collect::<Result<_, _>>()
            .map(Keys::Gradient),
    }
}

/// `editor.curve_field_edit`.
fn curve_field_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::undo_agent::live_entity(world, params, "entity")?;
    let component = text(params, "component")?;
    let path = text(params, "path")?;
    let mut value = crate::command::read_property(world, entity, component, path)
        .map_err(|e| err("curve_field_unknown", e.to_string()))?;
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let shape = super::curve_drawer::shape_of(&registry, value.as_ref()).ok_or_else(|| {
        err(
            "curve_field_not_curve",
            format!("`{component}.{path}` is not a curve or gradient key list"),
        )
    })?;
    let ReflectMut::List(list) = value.reflect_mut() else {
        return Err(err("curve_field_not_curve", "not a list"));
    };
    let current = super::curve_drawer::read_keys(list, &shape);
    let wanted = match (params.get("keys"), params.get("preset").and_then(Value::as_str)) {
        (Some(_), Some(_)) => return Err(err("curve_field_bad_params", "give `keys` or `preset`, not both")),
        (Some(keys), None) => Some(keys_param(shape.kind, keys)?),
        (None, Some(name)) => Some(super::curve_drawer::preset(&shape, name).ok_or_else(|| {
            err(
                "curve_preset_unknown",
                format!(
                    "no preset `{name}`; this field has: {}",
                    super::curve_drawer::preset_names(&shape).join(", ")
                ),
            )
        })?),
        (None, None) => None,
    };
    let kind = match shape.kind {
        ShapeKind::Curve { .. } => "curve",
        ShapeKind::Gradient(_) => "gradient",
    };
    let presets = super::curve_drawer::preset_names(&shape);
    let mut undoable = world
        .get_resource::<crate::command::UndoStack>()
        .map_or(0, crate::command::UndoStack::undo_depth);
    let keys = match wanted {
        Some(keys) if keys != current => {
            super::curve_drawer::write_keys(&registry, list, &shape, &keys)
                .map_err(|reason| err("curve_key_invalid", reason))?;
            let written = super::curve_drawer::read_keys(list, &shape);
            drop(registry);
            let report = super::multi::edit_value(world, &[entity], component, path, value)
                .map_err(|e| err(e.code, e.message))?;
            undoable = report.undo_depth;
            written
        }
        _ => current,
    };
    Ok(json!({
        "kind": kind,
        "keys": keys_json(&keys),
        "presets": presets,
        "undoable": undoable,
    }))
}

/// `editor.color_swatches`.
fn color_swatches(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(color) = params.get("add") {
        let rgba = color_param(color).ok_or_else(|| {
            err(
                "color_invalid",
                format!("`add` takes \"#rrggbb[aa]\" or [r, g, b(, a)] linear, got {color}"),
            )
        })?;
        picker_links::save_swatch(world, rgba);
    }
    if let Some(index) = params.get("remove") {
        let index = index
            .as_u64()
            .ok_or_else(|| RpcError::wrong_type("remove", "a swatch index"))? as usize;
        let count = picker_links::links(world, None, "").swatches.len();
        if index >= count {
            return Err(err(
                "swatch_index_out_of_range",
                format!("swatch {index} of {count}"),
            ));
        }
        picker_links::remove_swatch(world, index);
    }
    if params.get("clear").and_then(Value::as_bool) == Some(true) {
        let count = picker_links::links(world, None, "").swatches.len();
        for _ in 0..count {
            picker_links::remove_swatch(world, 0);
        }
    }
    let links = picker_links::links(world, None, "");
    let hex = |colors: &[[f32; 4]]| {
        colors
            .iter()
            .map(|c| super::color::to_hex(*c, true))
            .collect::<Vec<_>>()
    };
    Ok(json!({
        "swatches": hex(&links.swatches),
        "recent": hex(&links.recent),
    }))
}

const ENTITY: ParamSchema = ParamSchema {
    name: "entity",
    kind: "number",
    required: true,
    description: "The entity whose component holds the field.",
};
const COMPONENT: ParamSchema = ParamSchema {
    name: "component",
    kind: "string",
    required: true,
    description: "Component type path or short name (`MeshSource`).",
};

const SET_PARAMS: &[ParamSchema] = &[
    ENTITY,
    COMPONENT,
    ParamSchema {
        name: "field",
        kind: "string",
        required: true,
        description: "The asset field (`material`; nested struct fields as `a.b`).",
    },
    ParamSchema {
        name: "path",
        kind: "string",
        required: false,
        description: "The asset to put in the field (\"\" clears it); checked against the field's filter as a drop is.",
    },
    ParamSchema {
        name: "from_selection",
        kind: "boolean",
        required: false,
        description: "Take the Content Browser's selected asset instead of `path` (the field's Use selected button).",
    },
];

const UNIQUE_PARAMS: &[ParamSchema] = &[
    ENTITY,
    COMPONENT,
    ParamSchema {
        name: "path",
        kind: "string",
        required: true,
        description: "The asset field's reflect path (`material`).",
    },
];

const COUNT_PARAMS: &[ParamSchema] = &[ParamSchema {
    name: "path",
    kind: "string",
    required: true,
    description: "A project asset path (`assets/materials/rock.material.ron`; the `assets/` prefix is optional).",
}];

const CURVE_PARAMS: &[ParamSchema] = &[
    ENTITY,
    COMPONENT,
    ParamSchema {
        name: "path",
        kind: "string",
        required: true,
        description: "The curve or gradient field's reflect path (`falloff`).",
    },
    ParamSchema {
        name: "keys",
        kind: "array",
        required: false,
        description: "New keys, replacing the old: curve `[{ t, v, in?, out? }]`, gradient `[{ t, color }]` with color \
            \"#rrggbb[aa]\" or [r, g, b(, a)] linear. Sorted by `t`; one undo step.",
    },
    ParamSchema {
        name: "preset",
        kind: "string",
        required: false,
        description: "Replace the keys with a preset (the answer's `presets` lists them).",
    },
];

const SWATCH_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "add",
        kind: "string|array",
        required: false,
        description: "Save a swatch: \"#rrggbb[aa]\" or [r, g, b(, a)] linear (first in the list).",
    },
    ParamSchema {
        name: "remove",
        kind: "number",
        required: false,
        description: "Remove the swatch at this index.",
    },
    ParamSchema {
        name: "clear",
        kind: "boolean",
        required: false,
        description: "Remove every saved swatch.",
    },
];

/// The methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.asset_field_set",
                tool: "asset_field_set",
                description: "Sets an inspector asset field (@AssetFilter) to `path`, or with `from_selection` to the \
                    Content Browser's selected asset (the field's Use selected button), checked against the field's \
                    filter as a drop is; \"\" clears it (Clear). One undo step.",
                params: SET_PARAMS,
                result: "{ filter, before, after, changed, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            asset_field_set as Handler,
        ),
        (
            MethodSchema {
                name: "editor.make_unique",
                tool: "make_unique",
                description: "Make unique: copies the asset an inspector asset field holds to a new file next to the \
                    open level (next to the asset when the level is unsaved), without its .meta, and points the field \
                    at the copy (one undo step; the file stays on undo).",
                params: UNIQUE_PARAMS,
                result: "{ from, to, value, undoable }",
                mutating: true,
                permission: Permission::Assets,
            },
            make_unique as Handler,
        ),
        (
            MethodSchema {
                name: "editor.asset_referencer_count",
                tool: "asset_referencer_count",
                description: "The project files that reference an asset (the Inspector's shared-asset indicator), \
                    counted once per content-index generation.",
                params: COUNT_PARAMS,
                result: "{ path, count, shared, referencers: [path], generation }",
                mutating: false,
                permission: Permission::Read,
            },
            asset_referencer_count as Handler,
        ),
        (
            MethodSchema {
                name: "editor.curve_field_edit",
                tool: "curve_field_edit",
                description: "Reads or replaces the keys of an inspector curve or gradient field (a list of keys with a \
                    time and a value or color), or applies a preset. Without `keys` or `preset` it only reads.",
                params: CURVE_PARAMS,
                result: "{ kind: \"curve\" | \"gradient\", keys, presets: [name], undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            curve_field_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.color_swatches",
                tool: "color_swatches",
                description: "The color picker's saved swatches and recent colors (user preferences color.swatches and \
                    color.recent): add, remove or clear swatches.",
                params: SWATCH_PARAMS,
                result: "{ swatches: [\"#rrggbbaa\"], recent: [\"#rrggbbaa\"] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            color_swatches as Handler,
        ),
    ]
}
