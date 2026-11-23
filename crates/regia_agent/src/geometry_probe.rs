// SPDX-License-Identifier: MPL-2.0
//! `geometry.body_probe`: a standing capsule's band and support over a cooked world geometry file
//! (lane W-30, ADR-8323; book/agents/world-geometry-queries.md, section "Character mover").
//!
//! Stateless: the call names a cooked `<map>.meshlayers` on the host, loads it, and answers
//! `regia_world_geom::mover::body_probe` for one body. The `terrain.*` methods over the running
//! map's geometry are lane W-28's.

use regia_world_geom::mover::{CapsuleParams, body_probe};
use regia_world_geom::{SurfaceHit, SurfaceKind, WorldGeom};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// Longest `cooked` path, bytes.
const MAX_PATH_BYTES: usize = 4096;

fn bad(code: &str, message: String, hint: &str) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

fn kind_name(kind: SurfaceKind) -> &'static str {
    match kind {
        SurfaceKind::Ground => "ground",
        SurfaceKind::Floor => "floor",
        SurfaceKind::Roof => "roof",
        SurfaceKind::Wall => "wall",
        SurfaceKind::Blocker => "blocker",
    }
}

fn hit_json(hit: &SurfaceHit) -> Value {
    json!({
        "y": hit.y,
        "kind": kind_name(hit.kind),
        "layer": hit.tri.map(|t| t.layer),
        "triangle": hit.tri.map(|t| t.triangle),
    })
}

fn number(params: &Value, key: &str) -> Result<f32, RpcError> {
    params
        .get(key)
        .and_then(Value::as_f64)
        .map(|v| v as f32)
        .filter(|v| v.is_finite())
        .ok_or_else(|| {
            bad(
                "body_probe_bad_params",
                format!("`{key}` must be a finite number"),
                "pass radius, height, step_up and step_down in metres",
            )
        })
}

/// The `geometry.body_probe` handler.
pub(crate) fn body_probe_method(params: &Value) -> Result<Value, RpcError> {
    let path = params.get("cooked").and_then(Value::as_str).ok_or_else(|| {
        bad(
            "body_probe_bad_params",
            "`cooked` must be the host path of a cooked .meshlayers file".into(),
            "cook the map with `regia terrain passages import` and pass the output path",
        )
    })?;
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err(bad(
            "body_probe_bad_params",
            format!("`cooked` must be 1 to {MAX_PATH_BYTES} bytes"),
            "pass the host path of a cooked .meshlayers file",
        ));
    }
    // Every element must be a number: a skipped non-number could leave three and be accepted.
    let feet: Vec<f32> = match params.get("feet") {
        Some(Value::Array(values)) => values
            .iter()
            .map(|v| v.as_f64().map_or(f32::NAN, |v| v as f32))
            .collect(),
        _ => Vec::new(),
    };
    let feet: [f32; 3] = match feet.as_slice() {
        [x, y, z] if [*x, *y, *z].iter().all(|v| v.is_finite()) => [*x, *y, *z],
        _ => {
            return Err(bad(
                "body_probe_bad_point",
                "`feet` must be three finite numbers [x, y, z]".into(),
                "pass the body's feet position in world metres",
            ));
        }
    };
    let capsule = CapsuleParams {
        radius_m: number(params, "radius")?,
        height_m: number(params, "height")?,
        step_up_m: number(params, "step_up")?,
        step_down_m: number(params, "step_down")?,
        substep_m: 0.2,
    };
    if capsule.radius_m <= 0.0
        || capsule.height_m <= 0.0
        || capsule.step_up_m < 0.0
        || capsule.step_down_m < 0.0
    {
        return Err(bad(
            "body_probe_bad_params",
            "radius and height must be above 0, step_up and step_down at least 0".into(),
            "pass the body's capsule sizes in metres",
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| {
        bad(
            "body_probe_unreadable",
            format!("cannot read `{path}`: {e}"),
            "pass the host path of a cooked .meshlayers file",
        )
    })?;
    let geom = WorldGeom::from_cooked(&bytes).map_err(|e| {
        bad(
            "body_probe_corrupt",
            format!("`{path}` is not a loadable cooked .meshlayers file: {e}"),
            "re-cook the map with this engine version",
        )
    })?;
    let state = geom.new_state();
    let probe = body_probe(&geom, &state, &capsule, feet);
    Ok(json!({
        "support": {
            "floor": hit_json(&probe.support.floor),
            "ceiling": probe.support.ceiling.as_ref().map(hit_json),
        },
        "band": { "lo": probe.band.lo, "hi": probe.band.hi },
        "covered": probe.covered,
        "fits": probe.fits,
        "query_bits_version": regia_world_geom::QUERY_BITS_VERSION,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_world_geom::cook::{CookInputs, GroundInput, cook};

    fn code(e: &RpcError) -> Option<&str> {
        e.data.as_ref().and_then(|d| d["code"].as_str())
    }

    #[test]
    fn bad_params_have_stable_codes() {
        let e = body_probe_method(&json!({})).expect_err("no path");
        assert_eq!(code(&e), Some("body_probe_bad_params"));
        let e = body_probe_method(&json!({ "cooked": "x", "feet": [0.0, "a", 1.0] })).expect_err("bad feet");
        assert_eq!(code(&e), Some("body_probe_bad_point"));
        // A non-number among four elements is refused, not skipped down to three.
        let e =
            body_probe_method(&json!({ "cooked": "x", "feet": [0.0, "a", 1.0, 2.0] })).expect_err("bad feet");
        assert_eq!(code(&e), Some("body_probe_bad_point"));
        let e = body_probe_method(&json!({ "cooked": "x", "feet": "0 0 0" })).expect_err("bad feet");
        assert_eq!(code(&e), Some("body_probe_bad_point"));
        let e = body_probe_method(&json!({
            "cooked": "x", "feet": [0, 0, 0], "radius": -1, "height": 1.8, "step_up": 0.5, "step_down": 0.5
        }))
        .expect_err("bad radius");
        assert_eq!(code(&e), Some("body_probe_bad_params"));
        let e = body_probe_method(&json!({
            "cooked": "/nonexistent/regia/none.meshlayers", "feet": [0, 0, 0],
            "radius": 0.35, "height": 1.85, "step_up": 0.55, "step_down": 0.55
        }))
        .expect_err("missing file");
        assert_eq!(code(&e), Some("body_probe_unreadable"));
    }

    /// End to end through the dispatcher's handler: a flat ground cooked to a temp file answers the
    /// ground under the feet, open sky, and a fitting body; a garbage file is `body_probe_corrupt`.
    #[test]
    fn a_cooked_file_answers_the_probe() {
        let ground = GroundInput {
            lattice_n: 65,
            width_m: 1280.0,
            offset_m: 0.0,
            step_m: 0.03125,
            counts: vec![0; 65 * 65],
        };
        let out = cook(&CookInputs {
            document: b"(version:1, layers:[])",
            ground: &ground,
            terrain: None,
            read_file: &|_| Err("no source".into()),
        })
        .expect("cook");
        let dir = regia_test_utils::unique_temp_dir("geometry_probe");
        let path = dir.join("flat.meshlayers");
        std::fs::write(&path, out.file.encode().expect("encode")).expect("write");
        let reply = body_probe_method(&json!({
            "cooked": path.to_string_lossy(), "feet": [1.0, 0.0, 2.0],
            "radius": 0.35, "height": 1.85, "step_up": 0.55, "step_down": 0.55
        }))
        .expect("probe");
        assert_eq!(reply["support"]["floor"]["kind"], "ground");
        assert_eq!(reply["support"]["floor"]["y"], 0.0);
        assert!(reply["support"]["ceiling"].is_null());
        assert_eq!(reply["fits"], true);
        assert_eq!(reply["covered"], false);
        assert_eq!(reply["band"]["hi"].as_f64(), Some(f64::from(1.85f32)));
        assert_eq!(reply["query_bits_version"], regia_world_geom::QUERY_BITS_VERSION);
        // The same call through the dispatcher (the method table, not the handler).
        let mut world = regia_app::prelude::World::new();
        let params = json!({
            "cooked": path.to_string_lossy(), "feet": [1.0, 0.0, 2.0],
            "radius": 0.35, "height": 1.85, "step_up": 0.55, "step_down": 0.55
        });
        match crate::call_local(&mut world, "geometry.body_probe", params, None) {
            crate::LocalReply::Done(Ok(dispatched)) => assert_eq!(dispatched, reply),
            crate::LocalReply::Done(Err(e)) => panic!("dispatch failed: {e:?}"),
            crate::LocalReply::Pending(_) => panic!("body_probe is not deferred"),
        }
        let junk = dir.join("junk.meshlayers");
        std::fs::write(&junk, b"not a cooked file").expect("write");
        let e = body_probe_method(&json!({
            "cooked": junk.to_string_lossy(), "feet": [0, 0, 0],
            "radius": 0.35, "height": 1.85, "step_up": 0.55, "step_down": 0.55
        }))
        .expect_err("junk");
        assert_eq!(code(&e), Some("body_probe_corrupt"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
