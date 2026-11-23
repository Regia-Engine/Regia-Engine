// SPDX-License-Identifier: MPL-2.0
//! `gen.mesh` (`book/agents/gen-mesh.md`, ADR-7240): builds a `regia_gen` recipe (a base shape
//! plus operations: extrude with bevel, lathe, box, plane clip, convex intersection, planar or box
//! UVs, translate, scale, merge) and reports the mesh's counts, closedness, bounds, UV range,
//! volume and byte hash; with `mesh: true` it returns the arrays too. Pure: needs no world, no
//! renderer and no project, and the same recipe gives the same bits on every platform.
//!
//! Errors carry a lowercase `data.code`: `gen_invalid_recipe` (the recipe does not parse; the
//! message names the field), `gen_invalid_input`, `gen_bevel_too_wide`, `gen_cap_not_closed`,
//! `gen_too_large` (a [`regia_gen::RecipeLimits`] limit, or more than [`MAX_RETURNED_VERTICES`]
//! vertices with `mesh: true`).

use regia_gen::{CpuMesh, MeshStats, Recipe, RecipeLimits, build_recipe, mesh_stats};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// Most vertices `mesh: true` returns as arrays.
pub const MAX_RETURNED_VERTICES: usize = 16_384;

const HINT: &str = "see agents/gen-mesh in the book for the recipe shape and limits";

fn coded(code: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, HINT)
}

/// `gen.mesh { recipe, mesh? }`.
pub(crate) fn gen_mesh(params: &Value) -> Result<Value, RpcError> {
    let Some(raw) = params.get("recipe") else {
        return Err(RpcError::invalid_params("`recipe` is required"));
    };
    if !raw.is_object() {
        return Err(RpcError::wrong_type("recipe", "an object"));
    }
    let recipe: Recipe = serde_json::from_value(raw.clone())
        .map_err(|e| coded("gen_invalid_recipe", format!("gen.mesh: recipe: {e}")))?;
    let want_mesh = crate::params::opt_bool(params, "mesh")?.unwrap_or(false);
    let mesh = build_recipe(&recipe, &RecipeLimits::default())
        .map_err(|e| coded(e.code(), format!("gen.mesh: {e}")))?;
    let stats = mesh_stats(&mesh);
    let mut out = stats_json(&stats);
    if want_mesh {
        if mesh.positions.len() > MAX_RETURNED_VERTICES {
            return Err(coded(
                "gen_too_large",
                format!(
                    "gen.mesh: {} vertices is above the {MAX_RETURNED_VERTICES} that `mesh: true` returns; \
                     call without `mesh` for the stats",
                    mesh.positions.len()
                ),
            ));
        }
        out["mesh"] = arrays(&mesh);
    }
    Ok(out)
}

fn stats_json(s: &MeshStats) -> Value {
    json!({
        "vertices": s.vertices,
        "triangles": s.triangles,
        "watertight": s.watertight,
        "boundary_edges": s.boundary_edges,
        "non_manifold_edges": s.non_manifold_edges,
        "degenerate_triangles": s.degenerate_triangles,
        "bounds": s.bounds.map(|(min, max)| json!({ "min": min, "max": max })),
        "uv_bounds": s.uv_bounds.map(|(min, max)| json!({ "min": min, "max": max })),
        "volume": s.volume,
        "hash": s.hash,
    })
}

fn arrays(m: &CpuMesh) -> Value {
    json!({
        "positions": m.positions,
        "normals": m.normals,
        "uvs": m.uvs,
        "indices": m.indices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{assert_listed, assert_result_named};

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn a_box_reports_a_closed_mesh_and_the_arrays_on_request() {
        let params = json!({ "recipe": { "shape": { "box": { "size": [2, 1, 1] } } }, "mesh": true });
        let out = gen_mesh(&params).expect("box");
        assert_result_named("gen.mesh", &out);
        assert_eq!(out["watertight"], true);
        assert_eq!(
            (out["vertices"].as_u64(), out["triangles"].as_u64()),
            (Some(24), Some(12))
        );
        assert_eq!(out["bounds"]["min"], json!([-1.0, -0.5, -0.5]));
        assert_eq!(out["mesh"]["indices"].as_array().map(Vec::len), Some(36));
        assert_eq!(out["hash"].as_str().map(str::len), Some(64));
        assert!((out["volume"].as_f64().unwrap_or(0.0) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn a_rounded_box_and_a_capped_cylinder_are_closed() {
        let rounded = json!({ "recipe": { "shape": { "rounded_box": {
            "half": [0.2, 0.2, 0.2], "radius": 0.05, "segments": 2
        } } } });
        let out = gen_mesh(&rounded).expect("rounded box");
        assert_eq!(out["watertight"], true);
        assert!(out["volume"].as_f64().unwrap_or(0.0) > 0.0);
        let cylinder = json!({ "recipe": { "shape": { "cylinder": {
            "radius": 0.4, "length": 1.0, "sides": 8, "cap": true, "bottom_cap": true
        } } } });
        let out = gen_mesh(&cylinder).expect("cylinder");
        assert_eq!(out["watertight"], true);
        assert!(out["volume"].as_f64().unwrap_or(0.0) > 0.0);
    }

    #[test]
    fn every_error_code_is_in_the_contract() {
        let cases = [
            (
                json!({ "recipe": { "shape": { "cone": {} } } }),
                "gen_invalid_recipe",
            ),
            (
                json!({ "recipe": { "shape": { "extrude": { "profile": [[0, 0], [1, 0], [2, 0]], "depth": 1 } } } }),
                "gen_invalid_input",
            ),
            (
                json!({ "recipe": { "shape": { "extrude": { "profile": [[0, 0], [4, 0], [4, 0.2], [0, 0.2]], "depth": 2,
                    "bevel": { "width": 0.5, "segments": 1 } } } } }),
                "gen_bevel_too_wide",
            ),
            (
                json!({ "recipe": { "shape": { "lathe": { "profile": [[1, 0], [1, 2]], "segments": 6 } },
                    "ops": [ { "clip": { "plane": { "normal": [1, 0, 0], "offset": 0 } } } ] } }),
                "gen_cap_not_closed",
            ),
            (
                json!({ "recipe": { "shape": { "lathe": { "profile": [[0, 0], [1, 1], [0, 2]], "segments": 999 } } } }),
                "gen_too_large",
            ),
        ];
        for (params, want) in cases {
            let err = gen_mesh(&params).expect_err(want);
            assert_eq!(code(&err), want, "{err:?}");
            assert_listed("gen.mesh", want);
        }
        assert_eq!(code(&gen_mesh(&json!({})).expect_err("no recipe")), "F0002");
    }

    /// End to end through the dispatcher: permission, parameter gate and rate classes included.
    #[test]
    fn gen_mesh_through_the_protocol_dispatcher() {
        use regia_app::prelude::*;

        use crate::dispatch::{AgentInbox, process_requests};
        use crate::protocol::{Request, Response};
        use crate::safety::{AgentConfig, AgentPermissions};
        use crate::server::Inbound;

        fn call(world: &mut World, params: Value) -> Response {
            let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
            let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
            inbox_tx
                .send(Inbound {
                    request: Request {
                        jsonrpc: "2.0".into(),
                        id: Some(json!(1)),
                        method: "gen.mesh".into(),
                        params,
                    },
                    reply: reply_tx,
                })
                .expect("queue request");
            world.insert_resource(AgentInbox(inbox_rx));
            process_requests(world);
            // Dispatch is synchronous; no clock or sleeping is needed to observe its reply.
            reply_rx.try_recv().expect("dispatcher answered")
        }

        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(AgentPermissions {
                config: AgentConfig {
                    rate_limit_per_second: 10_000,
                    heavy_rate_limit_per_second: 10_000,
                    ..AgentConfig::default()
                },
                source: None,
            })
            .init_resource::<crate::safety::AuditLog>()
            .init_resource::<crate::safety::AgentSession>();
        let world = app.world_mut();
        let vase = json!({ "recipe": {
            "shape": { "lathe": { "profile": [[0, 0], [0.6, 0], [0.8, 0.5], [0.4, 1.4], [0.5, 2], [0, 2]], "segments": 24 } },
            "ops": [
                { "clip": { "plane": { "normal": [1, 0, 1], "offset": 0.3 } } },
                { "uv_planar": { "axis": "y", "scale": 0.5 } }
            ]
        } });
        let first = call(world, vase.clone());
        assert!(first.error.is_none(), "{:?}", first.error);
        let first = first.result.expect("result");
        assert_eq!(first["watertight"], true, "{first}");
        let again = call(world, vase).result.expect("again");
        assert_eq!(
            first["hash"], again["hash"],
            "the same recipe gives the same bits"
        );
        let bad = call(world, json!({ "recipe": 3 }));
        assert_eq!(code(&bad.error.expect("not an object")), "F0002");
        let bad = call(
            world,
            json!({ "recipe": { "shape": { "box": { "size": [1, 1, 1] } } }, "mesh": "yes" }),
        );
        assert_eq!(code(&bad.error.expect("mesh not a boolean")), "F0002");
    }
}
