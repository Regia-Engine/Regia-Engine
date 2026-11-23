// SPDX-License-Identifier: MPL-2.0
//! `gen.texture` and `gen.heightfield` (`book/agents/gen-textures.md`, ADR-7280): build a
//! `regia_gen` texture recipe (noise, a constant, or an edge, cavity, wear or grime mask baked
//! from a `gen.mesh` recipe; then threshold, levels, invert, blur and blend operations; written
//! as RGBA8, R16 or R32F) or a heightfield recipe (noise fill, terrace, flatten, clamp,
//! normalize, thermal erosion), and report the size, byte hash and value statistics. Pure: no
//! world, renderer or project, and the same recipe gives the same hash on every platform.
//!
//! Errors carry a lowercase `data.code`: `gen_invalid_recipe` (the recipe does not parse; the
//! message names the field), `gen_invalid_input`, `gen_too_large` (a
//! [`regia_gen::TextureLimits`] or [`regia_gen::HeightfieldLimits`] limit), and for mesh
//! sources the `gen.mesh` codes `gen_bevel_too_wide` and `gen_cap_not_closed`.

use regia_gen::{
    HeightfieldLimits, HeightfieldRecipe, TextureLimits, TextureRecipe, build_heightfield, build_texture,
    mesh_hash, r16_hash,
};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

const HINT: &str = "see agents/gen-textures in the book for the recipe shape and limits";

fn coded(code: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, HINT)
}

fn recipe<T: serde::de::DeserializeOwned>(method: &str, params: &Value) -> Result<T, RpcError> {
    let Some(raw) = params.get("recipe") else {
        return Err(RpcError::invalid_params("`recipe` is required"));
    };
    if !raw.is_object() {
        return Err(RpcError::wrong_type("recipe", "an object"));
    }
    serde_json::from_value(raw.clone())
        .map_err(|e| coded("gen_invalid_recipe", format!("{method}: recipe: {e}")))
}

/// `gen.texture { recipe }`.
pub(crate) fn gen_texture(params: &Value) -> Result<Value, RpcError> {
    let recipe: TextureRecipe = recipe("gen.texture", params)?;
    let out = build_texture(&recipe, &TextureLimits::default())
        .map_err(|e| coded(e.code(), format!("gen.texture: {e}")))?;
    let s = out.image.stats();
    Ok(json!({
        "width": out.image.width,
        "height": out.image.height,
        "format": out.format.name(),
        "bytes": out.bytes().len(),
        "hash": out.hash(),
        "min": s.min,
        "max": s.max,
        "mean": s.mean,
    }))
}

/// `gen.heightfield { recipe }`.
pub(crate) fn gen_heightfield(params: &Value) -> Result<Value, RpcError> {
    let recipe: HeightfieldRecipe = recipe("gen.heightfield", params)?;
    let hf = build_heightfield(&recipe, &HeightfieldLimits::default())
        .map_err(|e| coded(e.code(), format!("gen.heightfield: {e}")))?;
    let s = hf.stats();
    let slope = hf.slope_map().stats();
    let terrain = hf.to_terrain().ok().map(|t| {
        json!({
            "world_width": t.world_width,
            "height_scale": t.height_scale,
            "height_offset": t.height_offset,
            "hash": r16_hash(&t.heightfield),
        })
    });
    let mesh = hf
        .to_mesh()
        .map_err(|e| coded(e.code(), format!("gen.heightfield: {e}")))?;
    Ok(json!({
        "width": hf.width,
        "height": hf.height,
        "spacing": hf.spacing,
        "min": s.min,
        "max": s.max,
        "mean": s.mean,
        "hash": hf.hash(),
        "slope": { "max": slope.max, "mean": slope.mean },
        "terrain": terrain,
        "mesh": {
            "vertices": mesh.positions.len(),
            "triangles": mesh.indices.len() / 3,
            "hash": mesh_hash(&mesh),
        },
    }))
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
    fn a_tileable_noise_texture_reports_size_hash_and_stats() {
        let params = json!({ "recipe": {
            "width": 64, "height": 32,
            "source": { "noise": { "kind": "cellular", "seed": 5, "cells": 4, "octaves": 3, "tile": true } },
            "ops": [ { "levels": { "in_low": 0.1, "in_high": 0.8 } } ],
            "format": "r16"
        } });
        let out = gen_texture(&params).expect("texture");
        assert_result_named("gen.texture", &out);
        assert_eq!(
            (out["width"].as_u64(), out["height"].as_u64()),
            (Some(64), Some(32))
        );
        assert_eq!(out["format"], "r16");
        assert_eq!(out["bytes"].as_u64(), Some(64 * 32 * 2));
        assert_eq!(out["hash"].as_str().map(str::len), Some(64));
        assert!(out["min"].as_f64() >= Some(0.0) && out["max"].as_f64() <= Some(1.0));
        assert_eq!(gen_texture(&params).expect("again")["hash"], out["hash"]);
    }

    #[test]
    fn a_wear_mask_from_a_mesh_recipe_builds() {
        let params = json!({ "recipe": {
            "width": 64, "height": 64,
            "source": { "wear": {
                "mesh": { "shape": { "box": { "size": [1, 1, 1] } }, "ops": [ { "uv_planar": { "axis": "y" } } ] },
                "wear": { "seed": 3, "amount": 0.6 }
            } }
        } });
        let out = gen_texture(&params).expect("wear");
        assert_eq!(out["format"], "rgba8");
        assert_eq!(out["bytes"].as_u64(), Some(64 * 64 * 4));
    }

    #[test]
    fn a_heightfield_reports_terrain_and_mesh() {
        let params = json!({ "recipe": {
            "width": 33, "height": 33, "spacing": 2.0,
            "ops": [
                { "noise": { "noise": { "kind": "gradient", "seed": 1, "cells": 3, "octaves": 4 }, "amplitude": 20 } },
                { "erode": { "iterations": 5, "talus": 0.6 } },
                { "flatten": { "center": [0, 0], "radius": 6, "falloff": 4, "height": 8 } }
            ]
        } });
        let out = gen_heightfield(&params).expect("heightfield");
        assert_result_named("gen.heightfield", &out);
        assert_eq!(out["terrain"]["world_width"].as_f64(), Some(64.0));
        assert_eq!(out["mesh"]["vertices"].as_u64(), Some(33 * 33));
        assert_eq!(out["mesh"]["triangles"].as_u64(), Some(32 * 32 * 2));
        assert!(out["slope"]["max"].as_f64() > Some(0.0));
        // A non-square grid has no terrain form.
        let wide = gen_heightfield(&json!({ "recipe": { "width": 9, "height": 5 } })).expect("wide");
        assert!(wide["terrain"].is_null());
    }

    #[test]
    fn every_error_code_is_in_the_contract() {
        let texture_cases = [
            (
                json!({ "recipe": { "width": 8, "height": 8, "source": { "plasma": {} } } }),
                "gen_invalid_recipe",
            ),
            (
                json!({ "recipe": { "width": 8, "height": 8, "source": { "noise": { "kind": "value", "octaves": 0 } } } }),
                "gen_invalid_input",
            ),
            (
                json!({ "recipe": { "width": 4096, "height": 8, "source": { "constant": { "value": 1 } } } }),
                "gen_too_large",
            ),
            (
                json!({ "recipe": { "width": 8, "height": 8, "source": { "edge": { "mesh": { "shape": {
                    "extrude": { "profile": [[0, 0], [4, 0], [4, 0.2], [0, 0.2]], "depth": 2,
                    "bevel": { "width": 0.5, "segments": 1 } } } } } } } }),
                "gen_bevel_too_wide",
            ),
            (
                json!({ "recipe": { "width": 8, "height": 8, "source": { "cavity": { "mesh": {
                    "shape": { "lathe": { "profile": [[1, 0], [1, 2]], "segments": 6 } },
                    "ops": [ { "clip": { "plane": { "normal": [1, 0, 0], "offset": 0 } } } ] } } } } }),
                "gen_cap_not_closed",
            ),
        ];
        for (params, want) in texture_cases {
            let err = gen_texture(&params).expect_err(want);
            assert_eq!(code(&err), want, "{err:?}");
            assert_listed("gen.texture", want);
        }
        let heightfield_cases = [
            (
                json!({ "recipe": { "width": 8, "height": 8, "ops": [ { "smooth": {} } ] } }),
                "gen_invalid_recipe",
            ),
            (
                json!({ "recipe": { "width": 8, "height": 8, "ops": [ { "terrace": { "steps": 1 } } ] } }),
                "gen_invalid_input",
            ),
            (
                json!({ "recipe": { "width": 4097, "height": 8 } }),
                "gen_too_large",
            ),
        ];
        for (params, want) in heightfield_cases {
            let err = gen_heightfield(&params).expect_err(want);
            assert_eq!(code(&err), want, "{err:?}");
            assert_listed("gen.heightfield", want);
        }
        assert_eq!(code(&gen_texture(&json!({})).expect_err("no recipe")), "F0002");
        assert_eq!(
            code(&gen_heightfield(&json!({ "recipe": [] })).expect_err("array")),
            "F0002"
        );
    }

    /// End to end through the dispatcher: permission, parameter gate and rate classes included.
    #[test]
    fn gen_texture_and_heightfield_through_the_protocol_dispatcher() {
        use regia_app::prelude::*;

        use crate::dispatch::{AgentInbox, process_requests};
        use crate::protocol::{Request, Response};
        use crate::safety::{AgentConfig, AgentPermissions};
        use crate::server::Inbound;

        fn call(world: &mut World, method: &str, params: Value) -> Response {
            let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
            let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
            inbox_tx
                .send(Inbound {
                    request: Request {
                        jsonrpc: "2.0".into(),
                        id: Some(json!(1)),
                        method: method.into(),
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
        let texture = json!({ "recipe": {
            "width": 32, "height": 32,
            "source": { "noise": { "kind": "gradient", "seed": 9, "octaves": 4, "fractal": "ridged", "tile": true } },
            "ops": [ { "blur": { "radius": 1, "wrap": true } }, { "threshold": { "at": 0.5, "softness": 0.1 } } ]
        } });
        let first = call(world, "gen.texture", texture.clone());
        assert!(first.error.is_none(), "{:?}", first.error);
        let first = first.result.expect("result");
        assert_eq!(first["bytes"].as_u64(), Some(32 * 32 * 4));
        let again = call(world, "gen.texture", texture).result.expect("again");
        assert_eq!(
            first["hash"], again["hash"],
            "the same recipe gives the same bits"
        );
        let terrain = call(
            world,
            "gen.heightfield",
            json!({ "recipe": { "width": 17, "height": 17, "ops": [ { "terrace": { "steps": 3 } } ] } }),
        );
        assert!(terrain.error.is_none(), "{:?}", terrain.error);
        assert_eq!(
            terrain.result.expect("hf")["mesh"]["vertices"].as_u64(),
            Some(17 * 17)
        );
        let bad = call(world, "gen.texture", json!({ "recipe": 3 }));
        assert_eq!(code(&bad.error.expect("not an object")), "F0002");
        let bad = call(world, "gen.heightfield", json!({ "recipe": "flat" }));
        assert_eq!(code(&bad.error.expect("not an object")), "F0002");
    }
}
