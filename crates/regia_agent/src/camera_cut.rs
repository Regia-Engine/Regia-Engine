// SPDX-License-Identifier: MPL-2.0
//! `render.camera_cut`: drop every temporal history before the next frame, configure the
//! automatic cut detection, and report the last cut (book `agents/camera-cuts.md`, guide topic
//! `camera_cut`).
//!
//! A cut makes the next frame a fresh renderer's first frame: no TAA/TSR history, fog history,
//! static-screen cache, cached shadows or metered exposure from the view before it.

use regia_ecs::prelude::*;
use regia_render::{CameraCutDetection, CutExposure, RuntimeRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn detection_json(detection: Option<CameraCutDetection>) -> Value {
    match detection {
        Some(d) => json!({
            "max_eye_jump_m": d.max_eye_jump_m,
            "max_turn_degrees": d.max_turn_degrees,
            "max_kept_share": d.max_kept_share,
        }),
        None => Value::Null,
    }
}

fn number(object: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite() && *f >= 0.0)
            .map(|f| f as f32)
            .filter(|f| f.is_finite())
            .map(Some)
            .ok_or_else(|| {
                RpcError::invalid_params(format!("`detection.{name}` must be a finite f32 number >= 0"))
            }),
    }
}

/// `render.camera_cut { cut?, exposure?, detection? }`.
///
/// `cut` (default `true`) requests a cut on the next frame; `exposure` is `"snap"` (default) or
/// `"fade"`. `detection` is `false` (explicit cuts only), `true` (the defaults) or an object of
/// thresholds merged onto the current ones.
pub(crate) fn camera_cut(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let cut = match params.get("cut") {
        None | Some(Value::Null) => true,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`cut` must be a boolean"))?,
    };
    let exposure = match crate::params::opt_str(params, "exposure")? {
        None if params.get("exposure").is_none_or(Value::is_null) => CutExposure::Snap,
        Some("snap") => CutExposure::Snap,
        Some("fade") => CutExposure::Fade,
        _ => {
            return Err(RpcError::invalid_params(
                "`exposure` must be \"snap\" or \"fade\"",
            ));
        }
    };
    let Some(mut runtime) = world.get_resource_mut::<RuntimeRenderer>() else {
        return Err(RpcError::not_found(
            "no renderer",
            "render.camera_cut needs the render plugin (a windowed or headless app with RuntimeRenderer)",
        ));
    };
    let renderer = &mut runtime.0;
    match params.get("detection") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(false)) => renderer.set_camera_cut_detection(None),
        Some(Value::Bool(true)) => renderer.set_camera_cut_detection(Some(CameraCutDetection::default())),
        Some(object @ Value::Object(_)) => {
            let mut d = renderer.camera_cut_detection().unwrap_or_default();
            if let Some(v) = number(object, "max_eye_jump_m")? {
                d.max_eye_jump_m = v;
            }
            if let Some(v) = number(object, "max_turn_degrees")? {
                d.max_turn_degrees = v;
            }
            if let Some(v) = number(object, "max_kept_share")? {
                d.max_kept_share = v.min(1.0);
            }
            renderer.set_camera_cut_detection(Some(d));
        }
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`detection` must be a boolean or an object of thresholds",
            ));
        }
    }
    if cut {
        renderer.camera_cut_with(exposure);
    }
    let last = renderer
        .last_camera_cut()
        .map(|(frame, reason)| json!({ "frame": frame, "reason": reason.name() }));
    Ok(json!({
        "requested": cut,
        "exposure": if exposure == CutExposure::Fade { "fade" } else { "snap" },
        "detection": detection_json(renderer.camera_cut_detection()),
        "last_cut": last,
        "cuts": renderer.camera_cut_count(),
        "frame": renderer.frame_index(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_detection_threshold_is_rejected() {
        let mut world = World::new();
        world.init_resource::<RuntimeRenderer>();
        let before = camera_cut(&mut world, &json!({ "cut": false })).expect("before");
        for field in ["max_eye_jump_m", "max_turn_degrees", "max_kept_share"] {
            let mut detection = json!({});
            detection[field] = json!(1.0e39);
            let err = camera_cut(&mut world, &json!({ "detection": detection })).expect_err("overflow");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
            assert_eq!(
                camera_cut(&mut world, &json!({ "cut": false })).expect("after"),
                before
            );
        }
    }

    #[test]
    fn requests_configures_and_validates() {
        let mut world = World::new();
        assert!(camera_cut(&mut world, &json!({})).is_err(), "no renderer");
        world.init_resource::<RuntimeRenderer>();
        let out = camera_cut(&mut world, &json!({})).expect("cut");
        assert_eq!(out["requested"], json!(true));
        assert_eq!(out["exposure"], json!("snap"));
        assert_eq!(out["detection"]["max_eye_jump_m"], json!(50.0));
        assert_eq!(out["last_cut"], Value::Null, "applied on the next frame");
        let out = camera_cut(
            &mut world,
            &json!({ "cut": false, "detection": { "max_eye_jump_m": 10.0 } }),
        )
        .expect("configure");
        assert_eq!(out["requested"], json!(false));
        assert_eq!(out["detection"]["max_eye_jump_m"], json!(10.0));
        assert_eq!(out["detection"]["max_turn_degrees"], json!(90.0), "merged");
        let out = camera_cut(&mut world, &json!({ "detection": false, "exposure": "fade" })).expect("off");
        assert_eq!(out["detection"], Value::Null);
        assert_eq!(out["exposure"], json!("fade"));
        assert!(camera_cut(&mut world, &json!({ "exposure": "slow" })).is_err());
        assert!(camera_cut(&mut world, &json!({ "detection": { "max_turn_degrees": -1 } })).is_err());
        assert!(camera_cut(&mut world, &json!({ "cut": 1 })).is_err());
    }

    #[test]
    fn detection_edges_clamp_refuse_and_leave_state_alone_on_error() {
        let mut world = World::new();
        world.init_resource::<RuntimeRenderer>();
        // The kept share is clamped to 1, a zero threshold is allowed.
        let out = camera_cut(
            &mut world,
            &json!({ "cut": false, "detection": { "max_kept_share": 7.5, "max_eye_jump_m": 0 } }),
        )
        .expect("clamp");
        assert_eq!(out["detection"]["max_kept_share"], json!(1.0));
        assert_eq!(out["detection"]["max_eye_jump_m"], json!(0.0));
        // `true` restores the defaults, `{}` keeps what is set, an array/string/number is refused.
        let defaults = camera_cut(&mut world, &json!({ "cut": false, "detection": true })).expect("defaults");
        assert_eq!(defaults["detection"]["max_eye_jump_m"], json!(50.0));
        let kept = camera_cut(&mut world, &json!({ "cut": false, "detection": {} })).expect("merge nothing");
        assert_eq!(kept["detection"], defaults["detection"]);
        for bad in [json!([]), json!("on"), json!(1)] {
            assert!(
                camera_cut(&mut world, &json!({ "detection": bad })).is_err(),
                "{bad}"
            );
        }
        for bad in [
            json!({ "max_eye_jump_m": "far" }),
            json!({ "max_kept_share": -0.1 }),
            json!({ "max_turn_degrees": null, "max_eye_jump_m": [] }),
        ] {
            let before = camera_cut(&mut world, &json!({ "cut": false })).expect("read");
            let res = camera_cut(&mut world, &json!({ "cut": false, "detection": bad }));
            let after = camera_cut(&mut world, &json!({ "cut": false })).expect("read");
            if res.is_err() {
                assert_eq!(
                    before["detection"], after["detection"],
                    "a refused call changes nothing: {bad}"
                );
            }
            assert!(res.is_err(), "{bad}");
        }
        // Wrong types for exposure are refused, including a non-string.
        for bad in [json!(5), json!(true), json!("SNAP"), json!("")] {
            assert!(
                camera_cut(&mut world, &json!({ "exposure": bad })).is_err(),
                "{bad}"
            );
        }
        assert_eq!(
            camera_cut(&mut world, &json!({ "exposure": null })).expect("null")["exposure"],
            json!("snap")
        );
        // Each requested cut is counted only when the frame applies it; none is applied here.
        let count = camera_cut(&mut world, &json!({ "cut": false })).expect("count")["cuts"].clone();
        camera_cut(&mut world, &json!({})).expect("request");
        assert_eq!(
            camera_cut(&mut world, &json!({ "cut": false })).expect("count")["cuts"],
            count
        );
    }
}
