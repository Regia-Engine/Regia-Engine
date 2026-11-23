// SPDX-License-Identifier: MPL-2.0
//! `render.aerial_interiors`: the interior volumes the outdoor composite's aerial perspective reads
//! for the last extracted frame (ADR-4757, `book/agents/aerial-interiors.md`). The air inside one
//! takes no sky in-scatter (a roof is between it and the sky); the froxel fog reads the same four
//! slots, which hold the four volumes nearest the eye (ADR-7975). With a `direction`, the view ray from the primary camera's eye is clipped against them
//! (`regia_render::interior_spans_on_ray`, the CPU twin of the shader's clip): the stretches under a
//! roof, their length, and the share of the in-scatter the composite keeps for a surface within the
//! table's first 2 km. `render.set_aerial_interiors {enabled}` turns the rule off and on
//! (`FrameRenderer::set_aerial_interiors`; on by default) for before/after look comparisons.

use regia_app::prelude::*;
use regia_render::{
    AERIAL_INTERIOR_SLOTS, InteriorVolume, RuntimeRenderer, aerial_interior_slots, interior_spans_on_ray,
};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// The default ray length when `direction` is given without `distance` (meters).
const DEFAULT_DISTANCE_M: f32 = 100.0;

fn invalid_ray(message: &str) -> RpcError {
    RpcError::new(
        crate::protocol::INVALID_PARAMS,
        "invalid_aerial_ray",
        message,
        "use three finite direction numbers and a positive finite distance",
    )
}

/// The optional `direction` and `distance` parameters: a unit direction and a length in meters.
fn ray_params(params: &Value) -> Result<Option<([f32; 3], f32)>, RpcError> {
    let distance = match params.get("distance") {
        None => DEFAULT_DISTANCE_M,
        Some(raw) => match raw.as_f64().map(|v| v as f32) {
            Some(d) if d.is_finite() && d > 0.0 => d,
            _ => {
                return Err(invalid_ray("distance must be a positive finite number of meters"));
            }
        },
    };
    let Some(raw) = params.get("direction") else {
        return Ok(None);
    };
    let values: Option<Vec<f64>> = raw.as_array().and_then(|a| a.iter().map(Value::as_f64).collect());
    match values.as_deref() {
        Some(&[x, y, z]) if [x, y, z].iter().all(|v| v.is_finite()) => {
            // Scale before squaring: every finite JSON direction can be normalized without
            // overflowing the f32 diagnostic ray (or the f64 length).
            let scale = x.abs().max(y.abs()).max(z.abs());
            if scale == 0.0 {
                return Err(invalid_ray("direction must not be the zero vector"));
            }
            let [x, y, z] = [x / scale, y / scale, z / scale];
            let len = (x * x + y * y + z * z).sqrt();
            Ok(Some((
                [(x / len) as f32, (y / len) as f32, (z / len) as f32],
                distance,
            )))
        }
        _ => Err(invalid_ray(
            "direction must be an array of three finite numbers [x, y, z] (world space)",
        )),
    }
}

/// The JSON for a set of volumes as the composite binds them (`regia_render::aerial_interior_slots`:
/// all of them up to four, a box with a non-positive or non-finite half extent holding its slot
/// unused; past four, the four usable boxes nearest the eye, ADR-7661), the eye, and an optional ray.
fn report(interiors: &[InteriorVolume], eye: Option<[f32; 3]>, ray: Option<([f32; 3], f32)>) -> Value {
    // No eye: the composite has no view to pick for; the first four stand in.
    let slots = aerial_interior_slots(interiors, eye.unwrap_or([f32::NAN; 3]));
    let read: Vec<(usize, usize)> = slots
        .iter()
        .enumerate()
        .filter_map(|(slot, index)| index.map(|i| (slot, i)))
        .collect();
    let bound: Vec<Value> = read
        .iter()
        .map(|&(slot, i)| {
            let v = &interiors[i];
            let used = v.half_extent.iter().all(|h| h.is_finite() && *h > 0.0)
                && v.center.iter().all(|c| c.is_finite());
            json!({
                "slot": slot,
                "index": i,
                "center": v.center,
                "half_extent": v.half_extent,
                "used": used,
            })
        })
        .collect();
    let inside = eye.map(|e| {
        read.iter()
            .filter(|&&(_, i)| interiors[i].contains(e))
            .map(|&(slot, _)| slot)
            .collect::<Vec<_>>()
    });
    let mut out = json!({
        "slots": AERIAL_INTERIOR_SLOTS,
        "volumes": interiors.len(),
        "dropped": interiors.len().saturating_sub(read.len()),
        "bound": bound,
        "eye": eye,
        "eye_inside": inside.as_ref().map(|s| !s.is_empty()),
        "eye_slots": inside,
    });
    if let (Some((direction, distance)), Some(eye)) = (ray, eye) {
        let spans = interior_spans_on_ray(eye, direction, distance, interiors);
        out["ray"] = json!({
            "direction": direction,
            "distance_m": distance,
            "spans": spans.spans,
            "inside_m": spans.inside_m,
            "outside_m": spans.outside_m,
            "inscatter_share_near": spans.inscatter_share_near(),
        });
    }
    out
}

/// Whether the runtime renderer's composite reads the volumes (`None` without a `RuntimeRenderer`).
fn enabled(world: &World) -> Option<bool> {
    world
        .get_resource::<RuntimeRenderer>()
        .map(|renderer| renderer.0.aerial_interiors())
}

/// `render.aerial_interiors`: `{ enabled, slots, volumes, dropped, bound: [{ slot, index, center,
/// half_extent, used }], eye, eye_inside, eye_slots, ray?: { direction, distance_m, spans, inside_m,
/// outside_m, inscatter_share_near } }`. `eye` is null (and no ray is clipped) when the frame has no
/// view; `enabled` is null without a `RuntimeRenderer`.
pub(crate) fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let ray = ray_params(params)?;
    let extracted = world
        .get_resource::<regia_render::runtime::ExtractedFrame>()
        .ok_or_else(|| {
            RpcError::new(
                crate::protocol::NOT_FOUND,
                "aerial_frame_missing",
                "no ExtractedFrame resource in this app",
                "add regia_render::RenderPlugin (extract_frame fills the outdoor composite's interior volumes)",
            )
        })?;
    let eye = extracted.primary_view().map(|view| view.camera.eye);
    let mut out = report(&extracted.outdoor.interiors, eye, ray);
    out["enabled"] = json!(enabled(world));
    Ok(out)
}

/// `render.set_aerial_interiors { enabled }`: `{ before, after }`, each the switch's state.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let on = params.get("enabled").and_then(Value::as_bool).ok_or_else(|| {
        RpcError::new(
            crate::protocol::INVALID_PARAMS,
            "invalid_aerial_enabled",
            "`enabled` (a boolean) is required",
            "pass { enabled: true } or { enabled: false }",
        )
    })?;
    let mut renderer = world.get_resource_mut::<RuntimeRenderer>().ok_or_else(|| {
        RpcError::new(
            crate::protocol::NOT_FOUND,
            "aerial_renderer_missing",
            "no RuntimeRenderer resource in this app",
            "add regia_render::RenderPlugin with a window (the switch lives on the runtime renderer)",
        )
    })?;
    let before = renderer.0.aerial_interiors();
    renderer.0.set_aerial_interiors(on);
    let after = renderer.0.aerial_interiors();
    Ok(json!({ "before": { "enabled": before }, "after": { "enabled": after } }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aerial_interiors_needs_the_extracted_frame() {
        let mut world = World::new();
        assert!(get(&mut world, &json!({})).is_err(), "no ExtractedFrame");
        world.insert_resource(regia_render::runtime::ExtractedFrame::default());
        let got = get(&mut world, &json!({})).expect("an empty frame reports");
        assert_eq!(got["volumes"], 0);
        assert_eq!(got["slots"], AERIAL_INTERIOR_SLOTS);
        assert_eq!(got["eye"], Value::Null);
        assert!(got.get("ray").is_none(), "no eye, no ray");
    }

    #[test]
    fn aerial_interiors_reports_slots_and_clips_a_ray() {
        let room = InteriorVolume::with_floor([0.0, 2.0, 0.0], [5.0, 2.0, 5.0]);
        let broken = InteriorVolume::with_floor([0.0; 3], [0.0, 1.0, 1.0]);
        let far = InteriorVolume::with_floor([100.0, 2.0, 0.0], [1.0; 3]);
        // Four or fewer: every volume in its list slot, a broken box holding its slot unused.
        let four = report(&[room, broken, far, far], Some([0.0, 2.0, 0.0]), None);
        assert_eq!(four["dropped"], 0);
        assert_eq!(four["bound"][1]["used"], false);
        assert_eq!(four["bound"][1]["index"], 1);
        // Five: the four nearest the eye (ADR-7975); the broken box holds no air, so it is the one
        // left out, and the slots keep list order.
        let all = [room, broken, far, far, far];
        let got = report(&all, Some([0.0, 2.0, 0.0]), Some(([1.0, 0.0, 0.0], 100.0)));
        assert_eq!(got["volumes"], 5);
        assert_eq!(got["dropped"], 1);
        assert_eq!(got["bound"].as_array().map(Vec::len), Some(4));
        // Five volumes: the four usable ones nearest the eye are bound (ADR-7661), so the broken box
        // gives its slot to the last far one.
        assert_eq!(got["bound"][1]["index"], 2);
        assert_eq!(got["bound"][3]["index"], 4);
        assert_eq!(got["eye_inside"], true);
        assert_eq!(got["eye_slots"], json!([0]));
        // Four or fewer: each holds its own slot and a broken box keeps its slot unused.
        let four = report(&all[..4], Some([0.0, 2.0, 0.0]), None);
        assert_eq!(four["dropped"], 0);
        assert_eq!(four["bound"][1]["index"], 1);
        assert_eq!(four["bound"][1]["used"], false);
        let ray = &got["ray"];
        assert_eq!(ray["spans"][0], json!([0.0, 5.0]));
        // The far boxes start at x = 99 (the ray ends at 100): [99, 100] counts.
        assert_eq!(
            ray["inside_m"].as_f64().map(|v| (v * 1000.0).round()),
            Some(6000.0)
        );
        let share = ray["inscatter_share_near"].as_f64().unwrap_or(0.0);
        assert!((share - 0.94).abs() < 1.0e-4, "share {share}");
        // Outside every box, looking away: the open air's ray.
        let open = report(&[room], Some([20.0, 2.0, 0.0]), Some(([1.0, 0.0, 0.0], 100.0)));
        assert_eq!(open["eye_inside"], false);
        assert_eq!(open["ray"]["inside_m"], 0.0);
        assert_eq!(open["ray"]["inscatter_share_near"], 1.0);
    }

    /// ADR-7975: five passages, the eye in the last one listed. The slots hold the four nearest the
    /// eye, so the eye's own passage is bound and its air is taken out of the ray.
    #[test]
    fn the_eye_in_a_fifth_passage_is_bound() {
        let passage = |x: f32| InteriorVolume::with_floor([x, 2.0, 0.0], [4.0, 2.0, 4.0]);
        let all = [
            passage(-100.0),
            passage(-50.0),
            passage(50.0),
            passage(100.0),
            passage(200.0),
        ];
        let eye = [200.0, 2.0, 0.0];
        let got = report(&all, Some(eye), Some(([0.0, 0.0, 1.0], 4.0)));
        assert_eq!(got["dropped"], 1);
        assert_eq!(got["eye_inside"], true);
        let indices: Vec<Value> = (0..4).map(|s| got["bound"][s]["index"].clone()).collect();
        assert_eq!(
            indices,
            vec![json!(1), json!(2), json!(3), json!(4)],
            "the farthest is left out"
        );
        assert_eq!(got["eye_slots"], json!([3]));
        assert_eq!(
            got["ray"]["inscatter_share_near"], 0.0,
            "the eye's passage takes its air out"
        );
    }

    #[test]
    fn the_switch_needs_a_renderer_and_round_trips() {
        let mut world = World::new();
        assert!(
            set(&mut world, &json!({ "enabled": false })).is_err(),
            "no renderer"
        );
        world.insert_resource(RuntimeRenderer(regia_render::FrameRenderer::new()));
        assert!(
            set(&mut world, &json!({ "enabled": "no" })).is_err(),
            "not a boolean"
        );
        let got = set(&mut world, &json!({ "enabled": false })).expect("set");
        assert_eq!(got["before"]["enabled"], true, "on by default");
        assert_eq!(got["after"]["enabled"], false);
        world.insert_resource(regia_render::runtime::ExtractedFrame::default());
        assert_eq!(get(&mut world, &json!({})).expect("get")["enabled"], false);
        let back = set(&mut world, &json!({ "enabled": true })).expect("set");
        assert_eq!(back["after"]["enabled"], true);
    }

    #[test]
    fn aerial_methods_round_trip_through_the_protocol() {
        let mut world = World::new();
        world.insert_resource(regia_app::Time::default());
        world.insert_resource(regia_render::runtime::ExtractedFrame::default());
        world.insert_resource(RuntimeRenderer(regia_render::FrameRenderer::new()));
        let mut call = |method, params| {
            crate::dispatch::call_in_process(&mut world, method, params)
                .expect("synchronous method")
                .expect("protocol call")
        };
        assert_eq!(call("render.aerial_interiors", json!({}))["enabled"], true);
        assert_eq!(
            call("render.set_aerial_interiors", json!({"enabled": false}))["after"]["enabled"],
            false
        );
        assert_eq!(call("render.aerial_interiors", json!({}))["enabled"], false);
    }

    #[test]
    fn aerial_interiors_rejects_bad_rays() {
        assert!(ray_params(&json!({ "direction": [0.0, 0.0, 0.0] })).is_err());
        assert!(ray_params(&json!({ "direction": [1.0, 0.0] })).is_err());
        assert!(ray_params(&json!({ "direction": [1.0, "bad", 0.0, 0.0] })).is_err());
        assert!(ray_params(&json!({ "direction": [1.0, 0.0, 0.0, null] })).is_err());
        let huge = ray_params(&json!({ "direction": [1.0e30, 0.0, 0.0] }))
            .expect("finite large directions normalize without overflow")
            .expect("a ray");
        assert_eq!(huge.0, [1.0, 0.0, 0.0]);
        assert!(ray_params(&json!({ "direction": [1.0, 0.0, 0.0], "distance": -1.0 })).is_err());
        assert!(ray_params(&json!({ "distance": "far" })).is_err());
        let (dir, dist) = ray_params(&json!({ "direction": [0.0, 0.0, 2.0] }))
            .ok()
            .flatten()
            .unwrap_or(([0.0; 3], 0.0));
        assert_eq!(dir, [0.0, 0.0, 1.0]);
        assert_eq!(dist, DEFAULT_DISTANCE_M);
    }
}
