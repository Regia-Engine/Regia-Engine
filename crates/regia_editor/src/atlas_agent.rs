// SPDX-License-Identifier: MPL-2.0
//! `editor.atlas_generate`: the lightmap UV atlas generator on the agent protocol
//! (`book/agents/atlas.md`, ADR-1483).
//!
//! Unwraps and packs an inline triangle mesh (or a built-in test shape) with
//! [`regia_asset_pipeline::atlas::generate`] and reports the charts, the density, the utilization
//! and the warnings; `include_uvs` also returns the output buffers. Read-only: nothing in the
//! world changes.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::atlas::{self, AtlasMesh, AtlasOptions, ChartKind};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::{Handler, str_param};

/// Largest index count an agent call may send.
const MAX_INDICES: usize = 3_000_000;
/// Largest `segments` of the built-in sphere.
const MAX_SEGMENTS: u64 = 512;

/// An `invalid_params` error carrying `data.atlas_error = code`.
fn atlas_error(message: impl Into<String>, code: &str) -> RpcError {
    let mut error = RpcError::invalid_params(message);
    if let Some(data) = error.data.as_mut() {
        data["atlas_error"] = json!(code);
    }
    error
}

/// A unit cube, 24 vertices (one quad per side).
fn cube() -> (Vec<[f32; 3]>, Vec<u32>) {
    let sides: [[[f32; 3]; 4]; 6] = [
        [[1., 0., 0.], [1., 1., 0.], [1., 1., 1.], [1., 0., 1.]],
        [[0., 0., 0.], [0., 0., 1.], [0., 1., 1.], [0., 1., 0.]],
        [[0., 1., 0.], [0., 1., 1.], [1., 1., 1.], [1., 1., 0.]],
        [[0., 0., 0.], [1., 0., 0.], [1., 0., 1.], [0., 0., 1.]],
        [[0., 0., 1.], [1., 0., 1.], [1., 1., 1.], [0., 1., 1.]],
        [[0., 0., 0.], [0., 1., 0.], [1., 1., 0.], [1., 0., 0.]],
    ];
    let mut positions = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    for side in sides {
        let base = positions.len() as u32;
        positions.extend_from_slice(&side);
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (positions, indices)
}

/// A unit latitude/longitude sphere with `segments` around and `segments / 2` rings.
fn sphere(segments: u32) -> (Vec<[f32; 3]>, Vec<u32>) {
    let rings = (segments / 2).max(2);
    let mut positions = Vec::new();
    for r in 0..=rings {
        let theta = std::f32::consts::PI * r as f32 / rings as f32;
        for s in 0..=segments {
            let phi = std::f32::consts::TAU * s as f32 / segments as f32;
            let (st, ct) = theta.sin_cos();
            let (sp, cp) = phi.sin_cos();
            positions.push([st * cp, ct, st * sp]);
        }
    }
    let row = segments + 1;
    let mut indices = Vec::new();
    for r in 0..rings {
        for s in 0..segments {
            let a = r * row + s;
            let b = a + row;
            if r != 0 {
                indices.extend_from_slice(&[a, a + 1, b]);
            }
            if r != rings - 1 {
                indices.extend_from_slice(&[a + 1, b + 1, b]);
            }
        }
    }
    (positions, indices)
}

/// The mesh named by the params: `shape`, or `positions` + `indices`.
fn mesh_from_params(params: &Value) -> Result<(Vec<[f32; 3]>, Vec<u32>), RpcError> {
    if let Some(shape) = str_param(params, "shape") {
        return match shape {
            "cube" => Ok(cube()),
            "sphere" => {
                let segments = params.get("segments").and_then(Value::as_u64).unwrap_or(32);
                if !(3..=MAX_SEGMENTS).contains(&segments) {
                    return Err(atlas_error("`segments` must be in 3..=512", "atlas_bad_params"));
                }
                Ok(sphere(segments as u32))
            }
            other => Err(atlas_error(
                format!("unknown shape {other:?} (cube | sphere)"),
                "atlas_bad_params",
            )),
        };
    }
    let (Some(flat), Some(index_values)) = (
        params.get("positions").and_then(Value::as_array),
        params.get("indices").and_then(Value::as_array),
    ) else {
        return Err(atlas_error(
            "give `shape`, or `positions` (flat x, y, z numbers) and `indices`",
            "atlas_bad_params",
        ));
    };
    if !flat.len().is_multiple_of(3) {
        return Err(atlas_error(
            "`positions` must hold x, y, z triples",
            "atlas_bad_params",
        ));
    }
    if index_values.len() > MAX_INDICES {
        return Err(atlas_error(
            format!("at most {MAX_INDICES} indices per call"),
            "atlas_bad_params",
        ));
    }
    let numbers: Option<Vec<f32>> = flat.iter().map(|v| v.as_f64().map(|f| f as f32)).collect();
    let numbers = numbers.ok_or_else(|| atlas_error("`positions` must be numbers", "atlas_bad_params"))?;
    let (triples, _) = numbers.as_chunks::<3>();
    let indices: Option<Vec<u32>> = index_values
        .iter()
        .map(|v| v.as_u64().and_then(|i| u32::try_from(i).ok()))
        .collect();
    let indices = indices.ok_or_else(|| {
        atlas_error(
            "`indices` must be non-negative integers below 2^32",
            "atlas_bad_params",
        )
    })?;
    Ok((triples.to_vec(), indices))
}

fn options_from_params(params: &Value) -> AtlasOptions {
    let mut options = AtlasOptions {
        // One worker per call keeps agent calls from saturating the editor machine.
        threads: 1,
        ..AtlasOptions::default()
    };
    let u32_param = |name: &str| {
        params
            .get(name)
            .and_then(Value::as_u64)
            .map(|v| v.min(u64::from(u32::MAX)) as u32)
    };
    let f32_param = |name: &str| params.get(name).and_then(Value::as_f64).map(|v| v as f32);
    let bool_param = |name: &str| params.get(name).and_then(Value::as_bool);
    if let Some(v) = u32_param("resolution") {
        options.resolution = v;
    }
    if let Some(v) = u32_param("padding") {
        options.padding = v;
    }
    if let Some(v) = f32_param("texels_per_unit") {
        options.texels_per_unit = v;
    }
    if let Some(v) = f32_param("max_cost") {
        options.max_cost = v;
    }
    if let Some(v) = u32_param("max_iterations") {
        options.max_iterations = v;
    }
    if let Some(v) = bool_param("brute_force") {
        options.brute_force = v;
    }
    if let Some(v) = bool_param("rotate_charts") {
        options.rotate_charts = v;
    }
    if let Some(v) = bool_param("bilinear") {
        options.bilinear = v;
    }
    options
}

/// `editor.atlas_generate`.
fn atlas_generate_method(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (positions, indices) = mesh_from_params(params)?;
    let options = options_from_params(params);
    let result = atlas::generate(&AtlasMesh::new(&positions, &indices), &options)
        .map_err(|e| atlas_error(e.to_string(), e.code()))?;
    let kinds = [
        ChartKind::Planar,
        ChartKind::Ortho,
        ChartKind::Lscm,
        ChartKind::Piecewise,
    ];
    let mut chart_kinds = serde_json::Map::new();
    for kind in kinds {
        chart_kinds.insert(kind.name().to_string(), json!(result.chart_count(kind)));
    }
    let mut out = json!({
        "width": result.width,
        "height": result.height,
        "charts": result.charts.len(),
        "chart_kinds": chart_kinds,
        "faces": indices.len() / 3,
        "vertices": result.vertices.len(),
        "texels_per_unit": result.texels_per_unit,
        "utilization": result.utilization,
        "pack_passes": result.pack_passes,
        "warnings": result
            .warnings
            .iter()
            .map(|w| json!({ "code": w.code(), "message": w.to_string() }))
            .collect::<Vec<_>>(),
    });
    if params
        .get("include_uvs")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        out["uvs"] = json!(result.vertices.iter().flat_map(|v| v.uv).collect::<Vec<f32>>());
        out["xref"] = json!(result.vertices.iter().map(|v| v.xref).collect::<Vec<u32>>());
        out["indices"] = json!(result.indices);
        out["chart_faces"] = json!(result.charts.iter().map(|c| &c.faces).collect::<Vec<_>>());
    }
    Ok(out)
}

/// The atlas generator's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.atlas_generate",
            tool: "atlas_generate",
            description: "Unwrap and pack a triangle mesh into a lightmap UV atlas (chart segmentation, \
                orthogonal or least squares conformal parametrization, bitmap packing; ADR-1483). Give a \
                built-in `shape` or inline `positions` and `indices`. Deterministic; read-only.",
            params: &[
                ParamSchema {
                    name: "shape",
                    kind: "string",
                    required: false,
                    description: "\"cube\" or \"sphere\" instead of inline geometry.",
                },
                ParamSchema {
                    name: "segments",
                    kind: "integer",
                    required: false,
                    description: "Sphere segments around (3..=512, default 32; rings = segments / 2).",
                },
                ParamSchema {
                    name: "positions",
                    kind: "array",
                    required: false,
                    description: "Flat vertex positions: x, y, z, x, y, z, …",
                },
                ParamSchema {
                    name: "indices",
                    kind: "array",
                    required: false,
                    description: "Triangle list (at most 3,000,000 indices).",
                },
                ParamSchema {
                    name: "resolution",
                    kind: "integer",
                    required: false,
                    description: "Atlas side in texels (8..=16384, default 1024).",
                },
                ParamSchema {
                    name: "padding",
                    kind: "integer",
                    required: false,
                    description: "Gutter texels around each chart (default 2).",
                },
                ParamSchema {
                    name: "texels_per_unit",
                    kind: "number",
                    required: false,
                    description: "Fixed density; 0 (default) searches the largest that fits.",
                },
                ParamSchema {
                    name: "max_cost",
                    kind: "number",
                    required: false,
                    description: "Chart growth cost limit (default 2; lower = more charts).",
                },
                ParamSchema {
                    name: "max_iterations",
                    kind: "integer",
                    required: false,
                    description: "Seed relocation rounds (default 1).",
                },
                ParamSchema {
                    name: "brute_force",
                    kind: "boolean",
                    required: false,
                    description: "Try every position when packing (slower, tighter). Default false.",
                },
                ParamSchema {
                    name: "rotate_charts",
                    kind: "boolean",
                    required: false,
                    description: "Allow 90° turns when packing. Default true.",
                },
                ParamSchema {
                    name: "bilinear",
                    kind: "boolean",
                    required: false,
                    description: "Reserve the bilinear filter footprint around charts. Default true.",
                },
                ParamSchema {
                    name: "include_uvs",
                    kind: "boolean",
                    required: false,
                    description: "Also return uvs (flat u, v in [0,1]), xref, indices and chart_faces.",
                },
            ],
            result: "{ width, height, charts, chart_kinds: {planar, ortho, lscm, piecewise}, faces, vertices, \
                texels_per_unit, utilization, pack_passes, warnings: [{code, message}], uvs?, xref?, \
                indices?, chart_faces? }",
            mutating: false,
            permission: Permission::Read,
        },
        atlas_generate_method as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(error: &RpcError) -> Option<&str> {
        error
            .data
            .as_ref()
            .and_then(|data| data.get("atlas_error"))
            .and_then(Value::as_str)
    }

    #[test]
    fn the_agent_method_packs_shapes_and_inline_meshes() {
        let mut world = World::new();
        let cube =
            atlas_generate_method(&mut world, &json!({ "shape": "cube", "resolution": 256 })).expect("cube");
        assert_eq!(cube["width"], 256);
        assert_eq!(cube["charts"], 6);
        assert_eq!(cube["chart_kinds"]["planar"], 6);
        assert_eq!(cube["faces"], 12);
        assert!(cube["utilization"].as_f64().is_some_and(|u| u > 0.0 && u <= 1.0));
        assert!(cube.get("uvs").is_none());

        let sphere = atlas_generate_method(
            &mut world,
            &json!({ "shape": "sphere", "segments": 24, "resolution": 512, "include_uvs": true }),
        )
        .expect("sphere");
        let uvs = sphere["uvs"].as_array().expect("uvs");
        assert_eq!(
            uvs.len(),
            2 * sphere["vertices"].as_u64().expect("count") as usize
        );
        assert!(
            uvs.iter()
                .all(|u| u.as_f64().is_some_and(|u| (0.0..=1.0).contains(&u)))
        );
        assert_eq!(
            sphere["indices"].as_array().map(Vec::len),
            Some(3 * sphere["faces"].as_u64().expect("faces") as usize)
        );
        // Same call, same answer.
        let again = atlas_generate_method(
            &mut world,
            &json!({ "shape": "sphere", "segments": 24, "resolution": 512, "include_uvs": true }),
        )
        .expect("again");
        assert_eq!(sphere, again);

        let quad = atlas_generate_method(
            &mut world,
            &json!({
                "positions": [0, 0, 0, 1, 0, 0, 1, 1, 0, 0, 1, 0, 2, 0, 0],
                "indices": [0, 1, 2, 0, 2, 3, 0, 1, 4],
            }),
        )
        .expect("quad");
        assert_eq!(quad["charts"], 1);
        assert_eq!(quad["warnings"][0]["code"], "atlas_degenerate_triangle");
    }

    #[test]
    fn the_agent_method_reports_typed_error_codes() {
        let mut world = World::new();
        let cases = [
            (json!({}), "atlas_bad_params"),
            (json!({ "shape": "torus" }), "atlas_bad_params"),
            (json!({ "shape": "sphere", "segments": 2 }), "atlas_bad_params"),
            (json!({ "positions": [0, 0], "indices": [] }), "atlas_bad_params"),
            (
                json!({ "positions": [0, 0, "x"], "indices": [] }),
                "atlas_bad_params",
            ),
            (
                json!({ "positions": [0, 0, 0], "indices": [-1] }),
                "atlas_bad_params",
            ),
            (
                json!({ "positions": [0, 0, 0, 1, 0, 0, 0, 1, 0], "indices": [0, 1] }),
                "atlas_index_count",
            ),
            (
                json!({ "positions": [0, 0, 0, 1, 0, 0, 0, 1, 0], "indices": [0, 1, 7] }),
                "atlas_index_out_of_range",
            ),
            (
                json!({ "positions": [0, 0, 0, 1, 0, 0, 0, 1, 0], "indices": [0, 0, 1] }),
                "atlas_no_triangles",
            ),
            (
                json!({ "shape": "cube", "resolution": 4 }),
                "atlas_invalid_option",
            ),
            (
                json!({ "shape": "cube", "max_cost": 0.0 }),
                "atlas_invalid_option",
            ),
        ];
        for (params, expected) in cases {
            let error = atlas_generate_method(&mut world, &params).expect_err("must fail");
            assert_eq!(code(&error), Some(expected), "{params}");
            assert_eq!(error.code, regia_agent::protocol::INVALID_PARAMS);
        }
    }

    #[test]
    fn the_schema_row_is_read_only() {
        let rows = methods();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.name, "editor.atlas_generate");
        assert!(!rows[0].0.mutating);
        assert!(matches!(rows[0].0.permission, Permission::Read));
    }
}
