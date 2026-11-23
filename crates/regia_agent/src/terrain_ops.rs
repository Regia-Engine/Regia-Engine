// SPDX-License-Identifier: MPL-2.0
//! `terrain.apply_operator` (DO27 absorb row C-18, ADR-8465, `book/agents/terrain-ops.md`): runs one
//! heightfield operator of [`regia_terrain::ops`] (terrace, crater, flatten_pad, domain_warp, noise)
//! over the first terrain, or a sample window of it, and queues the changed samples through the
//! runtime height-patch queue that `terrain.patch_heights` uses (drawn terrain, CPU map and
//! `TerrainHeightsChanged` for physics and navigation). `dry_run` reports what would change and
//! queues nothing.

use regia_app::prelude::*;
use regia_terrain::ops::{
    CraterParams, GridFrame, NoiseKind, NoiseParams, OpError, PadParams, TerraceParams, WarpParams,
    changed_rect,
};
use regia_terrain::{HeightPatch, MAX_PATCH_SAMPLES, Terrain};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};
use crate::terrain_runtime::{first_terrain, no_terrain, patch_error, patch_handle, queue_json};

/// The operators `op` names.
pub(crate) const OPERATORS: &[&str] = &["crater", "domain_warp", "flatten_pad", "noise", "terrace"];

fn op_error(e: &OpError) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        e.code(),
        e.to_string(),
        "see book/agents/terrain-ops.md",
    )
}

fn bad_param(message: impl Into<String>) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        "op_bad_param",
        message,
        "see book/agents/terrain-ops.md for each operator's params",
    )
}

/// `params` as an operator's settings: missing fields take their defaults.
fn settings<T: DeserializeOwned>(params: &Value) -> Result<T, RpcError> {
    serde_json::from_value(params.clone()).map_err(|e| bad_param(format!("`params`: {e}")))
}

/// The sample window `[col0, row0, col1, row1)`: `rect` when given, else the whole map.
fn window(params: &Value, width: u32, height: u32) -> Result<[u32; 4], RpcError> {
    let Some(rect) = params.get("rect") else {
        return Ok([0, 0, width, height]);
    };
    let r: [u32; 4] = serde_json::from_value(rect.clone()).map_err(|_| {
        RpcError::new(
            protocol::INVALID_PARAMS,
            "op_bad_grid",
            "`rect` must be [col0, row0, col1, row1] in whole samples",
            "see book/agents/terrain-ops.md",
        )
    })?;
    if r[0].saturating_add(1) >= r[2] || r[1].saturating_add(1) >= r[3] || r[2] > width || r[3] > height {
        return Err(RpcError::new(
            protocol::INVALID_PARAMS,
            "op_bad_grid",
            format!("`rect` {r:?} must hold at least 2 x 2 samples inside the {width} x {height} map"),
            "terrain.get reports the map's width and height in samples",
        ));
    }
    Ok(r)
}

/// `terrain.apply_operator {op, params?, rect?, dry_run?}`.
pub fn apply_operator(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = params.get("op").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params(format!("`op` is required: one of {}", OPERATORS.join(", ")))
    })?;
    if !OPERATORS.contains(&op) {
        return Err(RpcError::new(
            protocol::INVALID_PARAMS,
            "op_unknown",
            format!("unknown operator `{op}`"),
            format!("`op` is one of {}", OPERATORS.join(", ")),
        ));
    }
    let dry_run = match params.get("dry_run") {
        None => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`dry_run` must be a boolean"))?,
    };
    let op_params = match params.get("params") {
        None => json!({}),
        Some(v @ Value::Object(_)) => v.clone(),
        Some(_) => return Err(bad_param("`params` must be an object")),
    };
    let entity = first_terrain(world).ok_or_else(no_terrain)?;
    let Some(terrain) = world.get::<Terrain>(entity) else {
        return Err(no_terrain());
    };
    let map = &terrain.map;
    let rect = window(params, map.width(), map.height())?;
    let frame = GridFrame::of_map(map);
    let frame = GridFrame::new(rect[2] - rect[0], rect[3] - rect[1], frame.cell_m, frame.origin)
        .and_then(|f| f.with_first([rect[0], rect[1]]))
        .map_err(|e| op_error(&e))?;
    let w = map.width() as usize;
    let before: Vec<f32> = (rect[1] as usize..rect[3] as usize)
        .flat_map(|r| {
            map.heights()[r * w + rect[0] as usize..r * w + rect[2] as usize]
                .iter()
                .copied()
        })
        .collect();
    let _span = tracing::info_span!(target: "regia_agent", "terrain.apply_operator", op).entered();
    let after = match op {
        "terrace" => regia_terrain::ops::terrace(&frame, &before, &settings::<TerraceParams>(&op_params)?),
        "crater" => regia_terrain::ops::crater(&frame, &before, &settings::<CraterParams>(&op_params)?),
        "flatten_pad" => {
            regia_terrain::ops::flatten_pad(&frame, &before, &settings::<PadParams>(&op_params)?)
        }
        "domain_warp" => {
            regia_terrain::ops::domain_warp(&frame, &before, &settings::<WarpParams>(&op_params)?)
        }
        _ => {
            // A wrong JSON type is refused, never read as the default (ADR-2622).
            let sharpness = match op_params.get("sharpness") {
                None | Some(Value::Null) => 2.0,
                Some(value) => value
                    .as_f64()
                    .ok_or_else(|| bad_param("noise `sharpness` must be a number"))?,
            };
            let kind = match op_params.get("kind") {
                None | Some(Value::Null) => NoiseKind::Fbm,
                Some(Value::String(kind)) => match kind.as_str() {
                    "fbm" => NoiseKind::Fbm,
                    "ridged" => NoiseKind::Ridged { sharpness },
                    other => return Err(bad_param(format!("noise `kind` `{other}` is not fbm or ridged"))),
                },
                Some(other) => {
                    return Err(bad_param(format!(
                        "noise `kind` must be a string (fbm or ridged), not {other}"
                    )));
                }
            };
            let mut fields = op_params.clone();
            if let Value::Object(m) = &mut fields {
                m.remove("kind");
                m.remove("sharpness");
            }
            regia_terrain::ops::add_noise(&frame, &before, &settings::<NoiseParams>(&fields)?, kind)
        }
    }
    .map_err(|e| op_error(&e))?;
    let changed = changed_rect(&frame, &before, &after);
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    let mut samples = 0u64;
    for (a, b) in before.iter().zip(&after) {
        if a.to_bits() != b.to_bits() {
            let d = f64::from(*b) - f64::from(*a);
            lo = lo.min(d);
            hi = hi.max(d);
            samples += 1;
        }
    }
    let changed_full = changed.map(|c| [rect[0] + c[0], rect[1] + c[1], rect[0] + c[2], rect[1] + c[3]]);
    let mut out = json!({
        "op": op,
        "rect": rect,
        "changed": changed_full,
        "changed_samples": samples,
        "min_delta_m": if samples > 0 { json!(lo) } else { Value::Null },
        "max_delta_m": if samples > 0 { json!(hi) } else { Value::Null },
        "dry_run": dry_run,
        "patches": [],
    });
    let (Some(c), false) = (changed, dry_run) else {
        return Ok(out);
    };
    let handle = patch_handle(world)?;
    if handle.target().is_none()
        && let Some(terrain) = world.get::<Terrain>(entity)
    {
        handle.set_target(regia_terrain::PatchTarget::of(&terrain.map));
    }
    // The changed rectangle in row bands of at most MAX_PATCH_SAMPLES samples.
    let cw = c[2] - c[0];
    let band_rows = (MAX_PATCH_SAMPLES / u64::from(cw)).max(1) as u32;
    let fw = frame.cols as usize;
    let mut ids = Vec::new();
    let mut row = c[1];
    while row < c[3] {
        let rows = band_rows.min(c[3] - row);
        let heights: Vec<f32> = (row as usize..(row + rows) as usize)
            .flat_map(|r| {
                after[r * fw + c[0] as usize..r * fw + c[2] as usize]
                    .iter()
                    .copied()
            })
            .collect();
        let id = handle
            .request(HeightPatch {
                col: rect[0] + c[0],
                row: rect[1] + row,
                width: cw,
                height: rows,
                heights,
            })
            .map_err(|e| patch_error(&e))?;
        ids.push(id);
        row += rows;
    }
    out["patches"] = json!(ids);
    out["queue"] = queue_json(&handle);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_with_terrain() -> World {
        let n = 65u32;
        let heights: Vec<f32> = (0..n * n).map(|i| (i % n) as f32 * 0.5).collect();
        let map = regia_terrain::TerrainMap::from_heights(n, n, heights, 128.0, 64.0, -8.0).expect("map");
        let terrain = Terrain::new(
            &map.to_heightfield(),
            128.0,
            64.0,
            -8.0,
            regia_terrain::CdlodConfig::default(),
            4,
            regia_terrain::TerrainLayers::default(),
        )
        .expect("terrain");
        let mut world = World::new();
        world.spawn(terrain);
        world.insert_resource(regia_terrain::TerrainHeightPatches::default());
        world
    }

    fn reason(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn apply_operator_queues_the_changed_samples_and_refuses_bad_input() {
        let mut world = world_with_terrain();
        let err = |w: &mut World, p: Value| reason(&apply_operator(w, &p).expect_err("refused"));
        assert_eq!(err(&mut world, json!({ "op": "erode" })), "op_unknown");
        assert_eq!(
            err(
                &mut world,
                json!({ "op": "terrace", "params": { "step_height_m": -1.0 } })
            ),
            "op_bad_param"
        );
        assert_eq!(
            err(&mut world, json!({ "op": "terrace", "rect": [0, 0, 1, 1] })),
            "op_bad_grid"
        );
        assert_eq!(
            err(
                &mut world,
                json!({ "op": "flatten_pad", "params": { "center": [5000.0, 0.0] } })
            ),
            "op_pad_empty"
        );
        // A wrong-typed noise `kind` or `sharpness` is refused, never read as fbm or 2.0.
        for params in [
            json!({ "kind": 1 }),
            json!({ "kind": "ridged", "sharpness": "3" }),
            json!({ "sharpness": true }),
        ] {
            assert_eq!(
                err(
                    &mut world,
                    json!({ "op": "noise", "params": params, "dry_run": true })
                ),
                "op_bad_param",
                "{params}"
            );
        }
        // A dry run reports and queues nothing.
        let pad = json!({ "op": "flatten_pad", "params": { "center": [0.0, 0.0], "shape": { "circle": { "radius_m": 10.0 } }, "feather_m": 4.0 }, "dry_run": true });
        let dry = apply_operator(&mut world, &pad).expect("dry run");
        assert!(dry["changed_samples"].as_u64().is_some_and(|n| n > 50), "{dry}");
        assert_eq!(dry["patches"], json!([]));
        // For real: one patch covering the changed rectangle.
        let mut real = pad.clone();
        real["dry_run"] = json!(false);
        let done = apply_operator(&mut world, &real).expect("queued");
        assert_eq!(done["patches"].as_array().map(Vec::len), Some(1), "{done}");
        assert_eq!(done["changed"], dry["changed"]);
        assert_eq!(done["queue"]["queued_patches"], json!(1));
        // A window keeps the change inside it.
        let windowed = apply_operator(
            &mut world,
            &json!({ "op": "noise", "rect": [8, 8, 24, 20], "params": { "amplitude_m": 2.0, "wavelength_m": 30.0 }, "dry_run": true }),
        )
        .expect("window");
        let c: [u32; 4] = serde_json::from_value(windowed["changed"].clone()).expect("rect");
        assert!(c[0] >= 8 && c[1] >= 8 && c[2] <= 24 && c[3] <= 20, "{c:?}");
    }
}
