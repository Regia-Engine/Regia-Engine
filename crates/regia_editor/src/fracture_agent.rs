// SPDX-License-Identifier: MPL-2.0
//! Agent method `editor.fracture_preview` (`book/agents/fracture.md`, ADR-1567): runs the
//! pre-fractured destruction tool (`regia_asset_pipeline::fracture`) on a box or on an inline
//! mesh and reports the chunks, the joints and the volume check. Nothing is written or changed.
//! It lives in the editor because the pipeline is desktop-only and the runtime protocol crate
//! cannot depend on it.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::fracture::{
    FractureError, FractureSet, FractureSettings, MAX_CHUNKS, box_mesh, fracture_mesh,
};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// Joints listed in one reply; `joints_total` still counts them all.
const MAX_JOINTS_LISTED: usize = 256;

/// Largest inline mesh accepted (vertices, indices).
const MAX_INLINE_VERTICES: usize = 100_000;
const MAX_INLINE_INDICES: usize = 300_000;

fn fracture_error(e: &FractureError) -> RpcError {
    let mut err = RpcError::invalid_params(e.to_string());
    if let Some(data) = err.data.as_mut() {
        data["reason"] = json!(e.code());
    }
    err
}

fn number(params: &Value, key: &str) -> Result<Option<f64>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|x| x.is_finite())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a finite number"))),
    }
}

fn whole(params: &Value, key: &str) -> Result<Option<u64>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a non-negative integer"))),
    }
}

/// FNV-1a over the RON text: equal hashes mean equal fractures.
fn fnv(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn report(source: &str, set: &FractureSet, hash: String) -> Value {
    let total = set.total_chunk_volume();
    let source_volume = f64::from(set.source_volume);
    json!({
        "source": source,
        "chunks": set.chunks.len(),
        "source_volume": set.source_volume,
        "chunk_volume_total": total,
        "volume_error": if source_volume > 0.0 { (total - source_volume).abs() / source_volume } else { 0.0 },
        "chunk_list": set.chunks.iter().map(|c| json!({
            "volume": c.volume,
            "triangles": c.triangle_count(),
            "interior_triangles": c.triangle_count() - c.surface_triangle_count(),
            "interior_area": c.interior_area,
            "centroid": c.centroid,
        })).collect::<Vec<_>>(),
        "joints_total": set.joints.len(),
        "joints_truncated": set.joints.len() > MAX_JOINTS_LISTED,
        "joints": set.joints.iter().take(MAX_JOINTS_LISTED).map(|j| json!({
            "a": j.a,
            "b": j.b,
            "contact_area": j.contact_area,
            "break_force": j.break_force,
            "anchor": j.anchor,
        })).collect::<Vec<_>>(),
        "hash": hash,
    })
}

/// An inline mesh: `{ "positions": [[x, y, z], ...], "indices": [i, ...] }`.
fn inline_mesh(mesh: &Value) -> Result<(Vec<[f32; 3]>, Vec<u32>), RpcError> {
    let bad = |what: &str| RpcError::invalid_params(format!("`mesh` {what}"));
    let positions = mesh
        .get("positions")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("needs a `positions` array of [x, y, z]"))?;
    let indices = mesh
        .get("indices")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("needs an `indices` array"))?;
    if positions.len() > MAX_INLINE_VERTICES || indices.len() > MAX_INLINE_INDICES {
        return Err(bad("is too large (100000 vertices, 300000 indices at most)"));
    }
    let mut p = Vec::with_capacity(positions.len());
    for v in positions {
        let c: Option<Vec<f32>> = v
            .as_array()
            .filter(|a| a.len() == 3)
            .and_then(|a| a.iter().map(|x| x.as_f64().map(|x| x as f32)).collect());
        let Some(c) = c else {
            return Err(bad("positions must be [x, y, z] numbers"));
        };
        p.push([c[0], c[1], c[2]]);
    }
    let mut i = Vec::with_capacity(indices.len());
    for v in indices {
        let Some(n) = v.as_u64().and_then(|n| u32::try_from(n).ok()) else {
            return Err(bad("indices must be non-negative integers"));
        };
        i.push(n);
    }
    Ok((p, i))
}

/// `editor.fracture_preview`: fracture a `box` (half extents) or an inline `mesh` into `chunks`
/// Voronoi chunks from `seed`.
fn fracture_preview(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!("editor.fracture_preview").entered();
    let chunks = whole(params, "chunks")?.unwrap_or(8);
    if chunks == 0 || chunks > MAX_CHUNKS as u64 {
        return Err(RpcError::invalid_params(format!(
            "`chunks` must be 1 to {MAX_CHUNKS}"
        )));
    }
    let seed = whole(params, "seed")?.unwrap_or(0);
    let mut settings = FractureSettings::random(chunks as usize, seed);
    if let Some(s) = number(params, "strength_per_area")? {
        settings.joints.strength_per_area = s as f32;
    }
    let (label, positions, indices) = match (params.get("box"), params.get("mesh")) {
        (Some(b), None) => {
            let half: Option<Vec<f32>> = b
                .as_array()
                .filter(|a| a.len() == 3)
                .and_then(|a| a.iter().map(|v| v.as_f64().map(|x| x as f32)).collect());
            let Some(half) = half else {
                return Err(RpcError::invalid_params("`box` must be [x, y, z] half extents"));
            };
            let (p, i) = box_mesh([half[0], half[1], half[2]]).map_err(|e| fracture_error(&e))?;
            ("box", p, i)
        }
        (None, Some(m)) => {
            let (p, i) = inline_mesh(m)?;
            ("mesh", p, i)
        }
        _ => {
            return Err(RpcError::invalid_params(
                "give exactly one of `box` ([x, y, z] half extents) or `mesh` ({ positions, indices })",
            ));
        }
    };
    let set = fracture_mesh(&positions, &indices, &settings).map_err(|e| fracture_error(&e))?;
    let text = set.to_ron().map_err(|e| fracture_error(&e))?;
    Ok(report(label, &set, fnv(&text)))
}

const fn param(name: &'static str, kind: &'static str, description: &'static str) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required: false,
        description,
    }
}

const PARAMS: &[ParamSchema] = &[
    param(
        "box",
        "array",
        "[x, y, z] half extents of a box to fracture. Give this or mesh.",
    ),
    param(
        "mesh",
        "object",
        "{ positions: [[x, y, z]], indices: [i] }: a closed, outward-facing triangle mesh.",
    ),
    param("chunks", "integer", "Chunks to make, 1 to 1024 (default 8)."),
    param(
        "seed",
        "integer",
        "Site sampling seed (default 0); equal seeds give equal fractures.",
    ),
    param(
        "strength_per_area",
        "number",
        "Newtons of break force per square metre of contact.",
    ),
];

/// The fracture agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.fracture_preview",
            tool: "fracture_preview",
            description: "Run the pre-fractured destruction tool on a box or an inline closed mesh: Voronoi \
                chunks by seed, the contact graph and the joint break forces. Nothing is written. Errors \
                carry data.reason (fracture_*, book/agents/fracture.md).",
            params: PARAMS,
            result: "{ source, chunks, source_volume, chunk_volume_total, volume_error, chunk_list: [{ volume, \
                triangles, interior_triangles, interior_area, centroid }], joints_total, joints_truncated, \
                joints: [{ a, b, contact_area, break_force, anchor }], hash }",
            mutating: false,
            permission: Permission::Read,
        },
        fracture_preview as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(params: Value) -> Result<Value, RpcError> {
        let mut world = World::new();
        fracture_preview(&mut world, &params)
    }

    #[test]
    fn the_method_is_registered_read_only() {
        let methods = methods();
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].0.name, "editor.fracture_preview");
        assert!(!methods[0].0.mutating);
    }

    #[test]
    fn a_box_fracture_reports_chunks_joints_and_a_stable_hash() -> Result<(), RpcError> {
        let a = call(json!({ "box": [1.0, 1.0, 1.0], "chunks": 6, "seed": 3 }))?;
        let b = call(json!({ "box": [1.0, 1.0, 1.0], "chunks": 6, "seed": 3 }))?;
        assert_eq!(a, b);
        assert_eq!(a["chunks"], 6);
        assert!(a["volume_error"].as_f64().is_some_and(|e| e < 1e-4));
        assert!(a["joints_total"].as_u64().is_some_and(|n| n > 0));
        let c = call(json!({ "box": [1.0, 1.0, 1.0], "chunks": 6, "seed": 4 }))?;
        assert_ne!(a["hash"], c["hash"]);
        Ok(())
    }

    #[test]
    fn an_inline_mesh_is_fractured_like_the_box() -> Result<(), RpcError> {
        let (p, i) = box_mesh([1.0, 1.0, 1.0]).map_err(|e| fracture_error(&e))?;
        let inline = call(json!({ "mesh": { "positions": p, "indices": i }, "chunks": 5, "seed": 2 }))?;
        let boxed = call(json!({ "box": [1.0, 1.0, 1.0], "chunks": 5, "seed": 2 }))?;
        assert_eq!(inline["hash"], boxed["hash"]);
        assert_eq!(inline["source"], "mesh");
        Ok(())
    }

    #[test]
    fn strength_scales_the_break_force() -> Result<(), RpcError> {
        let lo = call(json!({ "box": [1.0, 1.0, 1.0], "chunks": 2, "strength_per_area": 100.0 }))?;
        let hi = call(json!({ "box": [1.0, 1.0, 1.0], "chunks": 2, "strength_per_area": 200.0 }))?;
        let f = |v: &Value| v["joints"][0]["break_force"].as_f64();
        assert!(
            f(&lo)
                .zip(f(&hi))
                .is_some_and(|(l, h)| (h - 2.0 * l).abs() < 1e-3 * h)
        );
        Ok(())
    }

    fn reason(params: Value) -> Option<String> {
        call(params)
            .err()
            .and_then(|e| e.data)
            .and_then(|d| d.get("reason").and_then(Value::as_str).map(str::to_string))
    }

    #[test]
    fn bad_parameters_are_invalid_params() {
        for params in [
            json!({}),
            json!({ "box": [1.0, 1.0] }),
            json!({ "box": [1.0, 1.0, 1.0], "mesh": {} }),
            json!({ "box": [1.0, 1.0, 1.0], "chunks": 0 }),
            json!({ "box": [1.0, 1.0, 1.0], "chunks": 100000 }),
            json!({ "box": [1.0, 1.0, 1.0], "seed": -1 }),
            json!({ "box": [1.0, 1.0, 1.0], "strength_per_area": "x" }),
            json!({ "mesh": { "positions": [[0.0, 0.0]], "indices": [] } }),
            json!({ "mesh": { "positions": [], "indices": [-1] } }),
            json!({ "mesh": { "positions": [] } }),
        ] {
            let got = call(params.clone()).err().map(|e| e.code);
            assert_eq!(got, Some(regia_agent::protocol::INVALID_PARAMS), "{params}");
        }
    }

    #[test]
    fn tool_errors_carry_their_reason() {
        let reasons = [
            (json!({ "box": [0.0, 1.0, 1.0] }), "fracture_invalid_settings"),
            (
                json!({ "mesh": { "positions": [], "indices": [] } }),
                "fracture_empty_mesh",
            ),
            (
                json!({ "mesh": { "positions": [[0.0, 0.0, 0.0]], "indices": [0, 0] } }),
                "fracture_bad_index_count",
            ),
            (
                json!({ "mesh": { "positions": [[0.0, 0.0, 0.0]], "indices": [0, 0, 5] } }),
                "fracture_index_out_of_range",
            ),
            (
                json!({ "mesh": { "positions": [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]], "indices": [0, 1, 2] } }),
                "fracture_not_closed",
            ),
            (
                json!({ "box": [1.0, 1.0, 1.0], "strength_per_area": -5.0 }),
                "fracture_invalid_settings",
            ),
        ];
        for (params, want) in reasons {
            assert_eq!(reason(params.clone()).as_deref(), Some(want), "{params}");
        }
    }
}
