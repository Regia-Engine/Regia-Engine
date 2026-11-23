// SPDX-License-Identifier: MPL-2.0
//! `render.fog_shadow`: how the last drawn froxel fog pass read the sun shadow (ADR-3122): the rule
//! (`VolumetricFogSettings::sun_shadow`), the sun cascade it read among the frame's cascades, the
//! compare's depth bias, and, for a world `point`, where that point falls in each cascade
//! (covered, faded, outside, or not read by the fog) and what the compare there reads
//! (`regia_render::FogSunShadowProbe`, `book/agents/volumetric-fog.md`).

use regia_app::prelude::*;
use regia_render::{
    FOG_CASCADE_FADE, FogCascadeState, FogSunShadowFrame, FogSunShadowProbe, FogSunShadowRule,
    VolumetricFogSettings,
};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// The optional `point` parameter: three finite numbers.
fn point_param(params: &Value) -> Result<Option<[f32; 3]>, RpcError> {
    let Some(raw) = params.get("point") else {
        return Ok(None);
    };
    let values: Option<Vec<f32>> = raw
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_f64).map(|v| v as f32).collect());
    match values.as_deref() {
        Some(&[x, y, z]) if [x, y, z].iter().all(|v| v.is_finite()) => Ok(Some([x, y, z])),
        _ => Err(RpcError::invalid_params(
            "point must be an array of three finite numbers [x, y, z] (world space)",
        )),
    }
}

/// What the fog's lookup does at `point` in the frame's cascades: the read cascade's state and,
/// for every other cascade, `not_read` (the fog reads one cascade).
fn point_json(frame: &FogSunShadowFrame, point: [f32; 3]) -> Value {
    let Some(sample) = frame.sample(point) else {
        return json!({
            "point": point,
            "reads": "lit",
            "why": "no_sun_shadow",
            "cascades": [],
        });
    };
    let (reads, why) = match sample.state {
        FogCascadeState::Outside => ("lit", "outside_cascade"),
        FogCascadeState::Faded => ("compare_faded", "fade_band"),
        FogCascadeState::Covered => ("compare", "covered"),
    };
    let cascades: Vec<Value> = (0..frame.cascade_count.max(1))
        .map(|index| {
            let read = frame.cascade_index.is_none_or(|i| i == index);
            json!({
                "index": index,
                "read": read,
                "state": if read { sample.state.as_str() } else { "not_read" },
            })
        })
        .collect();
    json!({
        "point": point,
        "reads": reads,
        "why": why,
        "state": sample.state.as_str(),
        "edge": sample.edge,
        "fade": sample.fade,
        "texel": sample.texel,
        "ndc_z": sample.ndc_z,
        "bias_ndc": sample.bias_ndc,
        "cascades": cascades,
    })
}

/// The JSON for the last fog pass's read; `point` adds the lookup there.
fn frame_json(
    frame: Option<&FogSunShadowFrame>,
    configured: FogSunShadowRule,
    point: Option<[f32; 3]>,
) -> Value {
    let rules: Vec<&str> = FogSunShadowRule::ALL.iter().map(|r| r.as_str()).collect();
    let Some(frame) = frame else {
        return json!({
            "recorded": false,
            "rule": configured.as_str(),
            "rules": rules,
            "fade_band": FOG_CASCADE_FADE,
        });
    };
    let mut v = json!({
        "recorded": true,
        "rule": frame.rule.as_str(),
        "configured_rule": configured.as_str(),
        "rules": rules,
        "fade_band": if frame.rule == FogSunShadowRule::Legacy { FOG_CASCADE_FADE } else { 0.0 },
        "cascade_index": frame.cascade_index,
        "cascade_count": frame.cascade_count,
        "sun_travel": frame.sun_travel,
        "bias_ndc": frame.bias_ndc(),
        "bias_m": frame.bias_m(),
        "cascade": frame.cascade.map(|c| json!({
            "resolution": c.resolution,
            "texel_m": c.texel_m,
            "depth_m": c.depth_m,
        })),
    });
    if let Some(point) = point {
        v["lookup"] = point_json(frame, point);
    }
    v
}

/// `render.fog_shadow { point? }`: `{ recorded, rule, configured_rule, rules, fade_band, cascade_index,
/// cascade_count, sun_travel, bias_ndc, bias_m, cascade: { resolution, texel_m, depth_m } | null,
/// lookup?: { point, reads, why, state, edge, fade, texel, ndc_z, bias_ndc, cascades: [{ index, read,
/// state }] } }`. `recorded` is false until a fog pass has run (the rest then lists the settings).
pub(crate) fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let point = point_param(params)?;
    let probe = world.get_resource::<FogSunShadowProbe>().ok_or_else(|| {
        RpcError::not_found(
            "no FogSunShadowProbe resource in this app",
            "add regia_render::RenderPlugin (it inserts the probe; the froxel fog fills it each drawn frame)",
        )
    })?;
    let last = probe.last();
    let configured = world
        .get_resource::<VolumetricFogSettings>()
        .map_or(FogSunShadowRule::default(), |s| s.sun_shadow);
    Ok(frame_json(last.as_ref(), configured, point))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::FogSunCascade;

    /// An orthographic sun cascade looking straight down, ±50 m around the origin, 100 m deep.
    fn frame(rule: FogSunShadowRule) -> FogSunShadowFrame {
        let (r, depth) = (50.0_f32, 100.0_f32);
        // x → x / r, z → −y / r (top of the map is −z), depth = (50 − y) / 100.
        let m = [
            [1.0 / r, 0.0, 0.0, 0.0],
            [0.0, 0.0, -1.0 / depth, 0.0],
            [0.0, -1.0 / r, 0.0, 0.0],
            [0.0, 0.0, 0.5, 1.0],
        ];
        FogSunShadowFrame {
            rule,
            cascade_index: Some(2),
            cascade_count: 4,
            cascade: Some(FogSunCascade::new(m, 1024)),
            sun_travel: [0.0, -1.0, 0.0],
        }
    }

    #[test]
    fn fog_shadow_reports_the_read_cascade_and_point_states() {
        let mut world = World::new();
        assert!(get(&mut world, &json!({})).is_err(), "no probe resource");
        let probe = FogSunShadowProbe::default();
        world.insert_resource(probe.clone());
        let v = get(&mut world, &json!({})).expect("probe");
        assert_eq!(v["recorded"], json!(false));
        assert_eq!(v["rule"], json!("legacy"));
        assert_eq!(v["rules"], json!(["legacy", "calibrated"]));

        probe.publish(frame(FogSunShadowRule::Calibrated));
        let v = get(&mut world, &json!({ "point": [0.0, 10.0, 0.0] })).expect("read");
        assert_eq!(v["recorded"], json!(true));
        assert_eq!(v["rule"], json!("calibrated"));
        assert_eq!(v["cascade_index"], json!(2));
        assert_eq!(v["fade_band"], json!(0.0));
        let texel = v["cascade"]["texel_m"].as_f64().unwrap_or(f64::NAN);
        assert!((texel - 100.0 / 1024.0).abs() < 1e-6, "{texel}");
        let bias = v["bias_m"].as_f64().unwrap_or(f64::NAN);
        assert!(
            (bias - 0.5 * texel).abs() < 1e-5,
            "straight-down sun: half a texel (ADR-4199), {bias}"
        );
        assert_eq!(v["lookup"]["state"], json!("covered"));
        assert_eq!(v["lookup"]["reads"], json!("compare"));
        let cascades = v["lookup"]["cascades"].as_array().cloned().unwrap_or_default();
        assert_eq!(cascades.len(), 4);
        assert_eq!(cascades[2]["state"], json!("covered"));
        assert_eq!(cascades[0]["state"], json!("not_read"));

        let v = get(&mut world, &json!({ "point": [49.0, 10.0, 0.0] })).expect("read");
        assert_eq!(
            v["lookup"]["state"],
            json!("covered"),
            "no fade band when calibrated"
        );
        let v = get(&mut world, &json!({ "point": [80.0, 10.0, 0.0] })).expect("read");
        assert_eq!(v["lookup"]["state"], json!("outside"));
        assert_eq!(v["lookup"]["reads"], json!("lit"));

        probe.publish(frame(FogSunShadowRule::Legacy));
        let v = get(&mut world, &json!({ "point": [49.0, 10.0, 0.0] })).expect("read");
        assert_eq!(v["lookup"]["state"], json!("faded"));
        assert_eq!(v["lookup"]["reads"], json!("compare_faded"));
        assert_eq!(v["fade_band"], json!(FOG_CASCADE_FADE));

        let err = get(&mut world, &json!({ "point": [1.0, "x", 2.0] })).expect_err("bad point");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
    }
}
