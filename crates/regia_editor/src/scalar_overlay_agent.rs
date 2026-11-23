// SPDX-License-Identifier: MPL-2.0
//! The per-vertex scalar overlay's agent-protocol surface (`book/agents/scalar-overlay.md`):
//! `editor.set_scalar_overlay` sets, edits or clears a [`ScalarOverlay`] on a mesh entity.
//!
//! Check it worked with `editor.capture_viewport` before and after: only the targeted entity's
//! pixels change, and `clear` restores them.

use std::sync::Arc;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_render::{MeshRenderer, OverlayFit, Ramp, RampStop, ScalarOverlay};
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

fn ramp_json(ramp: &Ramp) -> Value {
    for name in regia_render::RAMP_NAMES {
        if Ramp::from_name(name).as_ref() == Some(ramp) {
            return json!(name);
        }
    }
    json!(
        ramp.stops()
            .iter()
            .map(|s| json!({ "at": s.at, "color": s.color }))
            .collect::<Vec<_>>()
    )
}

fn parse_ramp(value: &Value) -> Result<Ramp, RpcError> {
    if let Some(name) = value.as_str() {
        return Ramp::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown ramp `{name}` ({}); or pass stops [{{at, color}}]",
                regia_render::RAMP_NAMES.join(", ")
            ))
        });
    }
    let stops = value
        .as_array()
        .ok_or_else(|| RpcError::invalid_params("`ramp` is a name or an array of {at, color:[r,g,b]}"))?
        .iter()
        .map(|stop| {
            let at = stop.get("at").and_then(Value::as_f64);
            let color = stop
                .get("color")
                .and_then(Value::as_array)
                .filter(|c| c.len() == 3);
            match (at, color) {
                (Some(at), Some(color)) => {
                    let channel = |i: usize| color[i].as_f64().unwrap_or(f64::NAN) as f32;
                    Ok(RampStop {
                        at: at as f32,
                        color: [channel(0), channel(1), channel(2)],
                    })
                }
                _ => Err(RpcError::invalid_params(
                    "each ramp stop is {at: number, color: [r, g, b]}",
                )),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ramp::from_stops(stops).map_err(|err| RpcError::invalid_params(err.to_string()))
}

fn numbers(value: &Value, what: &str) -> Result<Vec<f32>, RpcError> {
    value
        .as_array()
        .ok_or_else(|| RpcError::invalid_params(format!("`{what}` is an array of numbers")))?
        .iter()
        .map(|v| {
            v.as_f64()
                .map(|n| n as f32)
                .ok_or_else(|| RpcError::invalid_params(format!("`{what}` holds a non-number")))
        })
        .collect()
}

/// The `paint.vertices` indices as `f64`: an `f32` (what [`numbers`] gives) holds whole numbers
/// exactly only up to 2^24, so index 16 777 217 of a large mesh used to paint vertex 16 777 216.
fn indices(value: &Value, what: &str) -> Result<Vec<f64>, RpcError> {
    value
        .as_array()
        .ok_or_else(|| RpcError::invalid_params(format!("`{what}` is an array of numbers")))?
        .iter()
        .map(|v| {
            v.as_f64()
                .ok_or_else(|| RpcError::invalid_params(format!("`{what}` holds a non-number")))
        })
        .collect()
}

/// Sets `value` at each index of `ids` in `current`. An index that is negative, not finite, not a
/// whole number, or at or above the vertex count is skipped (a negative `as usize` would
/// saturate to 0 and paint vertex 0).
fn paint_vertices(current: &mut [f32], ids: &[f64], value: f32) {
    for &id in ids {
        if !id.is_finite() || id < 0.0 || id.fract() != 0.0 {
            continue;
        }
        if let Some(slot) = current.get_mut(id as usize) {
            *slot = value;
        }
    }
}

/// `editor.set_scalar_overlay`.
fn set_scalar_overlay(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entity` (bits) is required"))?;
    let entity = crate::command::entity_from_bits(bits)
        .filter(|e| world.get_entity(*e).is_ok())
        .ok_or_else(|| RpcError::invalid_params(format!("no entity {bits}")))?;
    let name = crate::outliner::display_name(world, entity);
    if params.get("clear").and_then(Value::as_bool) == Some(true) {
        let had = world.entity_mut(entity).take::<ScalarOverlay>().is_some();
        return Ok(
            json!({ "entity": bits, "name": name, "overlay": null, "removed": had, "undoable": false }),
        );
    }
    let Some(mesh) = world.get::<MeshRenderer>(entity).map(|r| r.mesh.clone()) else {
        return Err(RpcError::invalid_params(
            "the entity has no mesh: pass the mesh entity (the `entity` of editor.pick_hit), not a model's owner",
        ));
    };
    let vertex_count = mesh.vertices.len();
    let existing = world.get::<ScalarOverlay>(entity).cloned();

    // Values: an explicit list, a constant, an object-space axis, or edits of the current ones.
    let sources = ["values", "constant", "axis"]
        .iter()
        .filter(|k| params.get(**k).is_some())
        .count();
    if sources > 1 {
        return Err(RpcError::invalid_params(
            "pass at most one of `values`, `constant`, `axis`",
        ));
    }
    let mut values: Option<Arc<[f32]>> = None;
    if let Some(list) = params.get("values") {
        values = Some(numbers(list, "values")?.into());
    } else if let Some(c) = params.get("constant") {
        let c = c
            .as_f64()
            .ok_or_else(|| RpcError::invalid_params("`constant` is a number"))? as f32;
        values = Some(vec![c; vertex_count].into());
    } else if let Some(axis) = params.get("axis") {
        let k = match axis.as_str() {
            Some("x") => 0,
            Some("y") => 1,
            Some("z") => 2,
            _ => return Err(RpcError::invalid_params("`axis` is x, y or z")),
        };
        values = Some(mesh.vertices.iter().map(|v| v.position[k]).collect());
    }
    if let Some(paint) = params.get("paint") {
        let ids = indices(
            paint
                .get("vertices")
                .ok_or_else(|| RpcError::invalid_params("`paint` needs `vertices` (indices) and `value`"))?,
            "paint.vertices",
        )?;
        let value = paint
            .get("value")
            .and_then(Value::as_f64)
            .ok_or_else(|| RpcError::invalid_params("`paint.value` is a number"))? as f32;
        let mut current: Vec<f32> = match (&values, &existing) {
            (Some(v), _) => v.to_vec(),
            (None, Some(o)) => o.values.to_vec(),
            (None, None) => vec![0.0; vertex_count],
        };
        paint_vertices(&mut current, &ids, value);
        values = Some(current.into());
    }

    let mut overlay = match (values, existing) {
        (Some(values), Some(old)) => ScalarOverlay { values, ..old },
        (Some(values), None) => ScalarOverlay::new(values),
        (None, Some(old)) => old,
        (None, None) => {
            return Err(RpcError::invalid_params(
                "no overlay yet: pass `values`, `constant` or `axis`",
            ));
        }
    };
    if let Some(ramp) = params.get("ramp") {
        overlay.ramp = parse_ramp(ramp)?;
    }
    if let Some(range) = params.get("range") {
        let r = numbers(range, "range")?;
        let [lo, hi] = r[..] else {
            return Err(RpcError::invalid_params("`range` is [lo, hi]"));
        };
        overlay.range = (lo, hi);
    }
    if let Some(opacity) = params.get("opacity").and_then(Value::as_f64) {
        overlay.opacity = opacity as f32;
    }
    let fit = match overlay.fit(vertex_count) {
        OverlayFit::Exact => json!("exact"),
        OverlayFit::Short { values, vertices } => {
            json!({ "mismatch": "short", "values": values, "vertices": vertices })
        }
        OverlayFit::Long { values, vertices } => {
            json!({ "mismatch": "long", "values": values, "vertices": vertices })
        }
    };
    let (min, max) = overlay
        .values
        .iter()
        .filter(|v| v.is_finite())
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &v| {
            (lo.min(v), hi.max(v))
        });
    let out = json!({
        "entity": bits,
        "name": name,
        "overlay": {
            "vertices": vertex_count,
            "values": overlay.values.len(),
            "fit": fit,
            "min": min.is_finite().then_some(min),
            "max": max.is_finite().then_some(max),
            "ramp": ramp_json(&overlay.ramp),
            "range": [overlay.range.0, overlay.range.1],
            "opacity": overlay.opacity,
        },
        "undoable": false,
    });
    world.entity_mut(entity).insert(overlay);
    Ok(out)
}

/// The scalar overlay methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.set_scalar_overlay",
            tool: "set_scalar_overlay",
            description: "Colour a mesh entity by one number per vertex (skin weights, deformation, \
                damage), drawn unlit over the lit viewport. Set values with `values`, `constant`, \
                `axis` or `paint`; ramp, range and opacity can change alone on an existing overlay. \
                A values count that differs from the vertex count draws nothing (see `fit`).",
            params: &[
                ParamSchema {
                    name: "entity",
                    kind: "number",
                    required: true,
                    description: "The mesh entity (bits), e.g. the `entity` of editor.pick_hit.",
                },
                ParamSchema {
                    name: "values",
                    kind: "array",
                    required: false,
                    description: "One number per vertex, in vertex order.",
                },
                ParamSchema {
                    name: "constant",
                    kind: "number",
                    required: false,
                    description: "Every vertex gets this value.",
                },
                ParamSchema {
                    name: "axis",
                    kind: "string",
                    required: false,
                    description: "x, y or z: each vertex's object-space coordinate on that axis.",
                },
                ParamSchema {
                    name: "paint",
                    kind: "object",
                    required: false,
                    description: "{vertices: [index...], value}: set those vertices on top of the \
                        given or current values (zeros for a new overlay). An index that is negative, \
                        not a whole number, or at or above the vertex count is skipped.",
                },
                ParamSchema {
                    name: "ramp",
                    kind: "string|array",
                    required: false,
                    description: "viridis (default), diverging or grayscale; or stops \
                        [{at: 0..1, color: [r, g, b] sRGB 0..1}].",
                },
                ParamSchema {
                    name: "range",
                    kind: "array",
                    required: false,
                    description: "[lo, hi]: values mapped to the ramp's start and end (clamped). Default [0, 1].",
                },
                ParamSchema {
                    name: "opacity",
                    kind: "number",
                    required: false,
                    description: "0..1 blend over the lit frame. Default 1.",
                },
                ParamSchema {
                    name: "clear",
                    kind: "boolean",
                    required: false,
                    description: "Remove the overlay.",
                },
            ],
            result: "{ entity, name, overlay: { vertices, values, fit, min, max, ramp, range, opacity } | null, undoable }",
            mutating: true,
            permission: Permission::EditScene,
        },
        set_scalar_overlay as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_indices_past_two_to_the_24_stay_exact() {
        // As f32 these two indices collapsed onto one vertex.
        let ids = indices(&json!([16_777_216, 16_777_217]), "paint.vertices").expect("indices");
        assert_eq!(ids, [16_777_216.0, 16_777_217.0]);
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn paint_skips_what_the_contract_says_it_skips() {
        let mut values = vec![0.0f32; 4];
        paint_vertices(&mut values, &[-3.0, 1.9, f64::NAN, 9.0, 2.0], 1.0);
        assert_eq!(
            values,
            [0.0, 0.0, 1.0, 0.0],
            "only the whole in-range index 2 is painted"
        );
        // The schema text is the contract an agent reads: it must not promise vertex 0 for a
        // negative index or truncation of 1.9 (the code skips both).
        let (schema, _) = &methods()[0];
        let paint = schema
            .params
            .iter()
            .find(|p| p.name == "paint")
            .expect("paint param");
        assert!(
            !paint.description.contains("paints vertex 0"),
            "{}",
            paint.description
        );
        assert!(!paint.description.contains("truncated"), "{}", paint.description);
        assert!(paint.description.contains("skipped"), "{}", paint.description);
    }
}
