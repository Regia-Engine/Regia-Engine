// SPDX-License-Identifier: MPL-2.0
//! `camera.interior_state` (ADR-8336, book/agents/interior-camera.md).
//!
//! A read of the presentation zone machine. The call carries its own `state` and returns the next
//! one; it does not move a camera in the world and it does not sweep geometry. `dt_s` steps the
//! frame clock. Without it, the call reports the state it was given.
//!
//! Error reasons (`data.reason`): `interior_bad_dt`, `interior_bad_zone`, `interior_bad_boom`,
//! `interior_bad_state`.

use regia_app::prelude::*;
use regia_world_geom::interior::{
    CamZone, CameraInteriorParams, InteriorProfile, InteriorSnapshot, shaped_boom,
};
use serde_json::{Map, Value, json};

use crate::protocol::RpcError;

fn reason(code: &str, message: &str) -> RpcError {
    let mut err = RpcError::invalid_params(message);
    if let Some(Value::Object(data)) = err.data.as_mut() {
        data.insert("reason".into(), Value::String(code.into()));
    }
    err
}

fn f32_of(n: f64) -> Option<f32> {
    let narrow = n as f32;
    narrow.is_finite().then_some(narrow)
}

fn opt_vec3(params: &Value, key: &str) -> Result<Option<[f32; 3]>, RpcError> {
    let Some(items) = crate::params::opt_array(params, key)? else {
        return Ok(None);
    };
    if items.len() != 3 {
        return Err(reason(
            "interior_bad_state",
            &format!("`{key}` must hold 3 numbers"),
        ));
    }
    let mut out = [0.0_f32; 3];
    for (i, item) in items.iter().enumerate() {
        let Some(n) = item.as_f64() else {
            return Err(RpcError::wrong_type(key, "an array of 3 numbers"));
        };
        let Some(narrow) = f32_of(n) else {
            return Err(reason("interior_bad_state", &format!("`{key}` must be finite")));
        };
        out[i] = narrow;
    }
    Ok(Some(out))
}

fn count_u32(name: &str, n: u64) -> Result<u32, RpcError> {
    u32::try_from(n).map_err(|_| reason("interior_bad_state", &format!("`{name}` is out of range")))
}

/// A wrong type inside `state` names the path (`state.zone`), not only the field.
fn in_state(mut err: RpcError) -> RpcError {
    if let Some(Value::Object(data)) = err.data.as_mut()
        && let Some(Value::String(param)) = data.get_mut("param")
        && !param.contains('.')
    {
        param.insert_str(0, "state.");
    }
    err
}

fn profile_from(state: Option<&Map<String, Value>>) -> Result<InteriorProfile, RpcError> {
    let Some(obj) = state else {
        return Ok(InteriorProfile::new());
    };
    let zone = match crate::params::opt_str(obj, "zone").map_err(in_state)? {
        None => CamZone::Exterior,
        Some(text) => CamZone::parse(text)
            .ok_or_else(|| reason("interior_bad_zone", "zone must be exterior, mouth, or interior"))?,
    };
    let blend = crate::params::opt_f64(obj, "blend")
        .map_err(in_state)?
        .unwrap_or(0.0);
    if !blend.is_finite() || !(0.0..=1.0).contains(&blend) {
        return Err(reason(
            "interior_bad_state",
            "blend must be a finite number from 0 to 1",
        ));
    }
    if let Some(boom) = crate::params::opt_f64(obj, "boom_m").map_err(in_state)?
        && (!boom.is_finite() || boom < 0.0 || f32_of(boom).is_none())
    {
        return Err(reason("interior_bad_boom", "boom_m must be a finite number >= 0"));
    }
    let cover_timer_s = crate::params::opt_f64(obj, "cover_timer_s")
        .map_err(in_state)?
        .unwrap_or(0.0);
    if !cover_timer_s.is_finite() || cover_timer_s < 0.0 || f32_of(cover_timer_s).is_none() {
        return Err(reason(
            "interior_bad_state",
            "cover_timer_s must be a finite number >= 0",
        ));
    }
    let snap = InteriorSnapshot {
        zone,
        blend: blend as f32,
        boom_m: 0.0,
        open_ticks: count_u32(
            "open_ticks",
            crate::params::opt_u64(obj, "open_ticks")
                .map_err(in_state)?
                .unwrap_or(0),
        )?,
        covered: crate::params::opt_bool(obj, "covered")
            .map_err(in_state)?
            .unwrap_or(false),
        seen_inside: crate::params::opt_bool(obj, "seen_inside")
            .map_err(in_state)?
            .unwrap_or(false),
        exiting: crate::params::opt_bool(obj, "exiting")
            .map_err(in_state)?
            .unwrap_or(false),
        gap: count_u32(
            "gap",
            crate::params::opt_u64(obj, "gap").map_err(in_state)?.unwrap_or(0),
        )?,
        seed: crate::params::opt_bool(obj, "seed")
            .map_err(in_state)?
            .unwrap_or(true),
        cover_timer_s: cover_timer_s as f32,
        passthrough: false,
    };
    Ok(InteriorProfile::restore(&snap))
}

fn snap_json(snap: &InteriorSnapshot) -> Value {
    json!({
        "zone": snap.zone.as_str(),
        "blend": f64::from(snap.blend),
        "boom_m": f64::from(snap.boom_m),
        "open_ticks": snap.open_ticks,
        "covered": snap.covered,
        "seen_inside": snap.seen_inside,
        "exiting": snap.exiting,
        "gap": snap.gap,
        "seed": snap.seed,
        "cover_timer_s": f64::from(snap.cover_timer_s),
        "passthrough": snap.passthrough,
    })
}

/// `camera.interior_state`: the presentation zone, blend and boom.
///
/// `{}` reports the documented defaults and does not consume the spawn cut. A present `dt_s` steps.
/// `pivot` and `desired` together also return the blend-shortened eye, with no lid cap and no sweep.
pub fn camera_interior_state(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    crate::params::check_types("camera.interior_state", params)?;
    let tuning = CameraInteriorParams::default();
    let dt = crate::params::opt_f64(params, "dt_s")?;
    let under_lid = crate::params::opt_bool(params, "under_lid")?.unwrap_or(false);
    let near_mouth = crate::params::opt_bool(params, "near_mouth")?.unwrap_or(false);
    let exterior =
        crate::params::opt_f64(params, "exterior_boom_m")?.unwrap_or(f64::from(tuning.exterior_boom_m));
    let pivot = opt_vec3(params, "pivot")?;
    let desired = opt_vec3(params, "desired")?;
    let state = crate::params::opt_object(params, "state")?;
    if let Some(dt) = dt
        && (dt < 0.0 || f32_of(dt).is_none())
    {
        return Err(reason("interior_bad_dt", "dt_s must be a finite number >= 0"));
    }
    if exterior < 0.0 || f32_of(exterior).is_none() {
        return Err(reason(
            "interior_bad_boom",
            "exterior_boom_m must be a finite number >= 0",
        ));
    }
    if pivot.is_some() != desired.is_some() {
        return Err(reason(
            "interior_bad_state",
            "pivot and desired are passed together",
        ));
    }
    let exterior_f = f32_of(exterior).unwrap_or(tuning.exterior_boom_m);
    let mut profile = profile_from(state)?;
    let snap = if let Some(dt) = dt {
        profile.step(
            f32_of(dt).unwrap_or(0.0),
            under_lid,
            near_mouth,
            exterior_f,
            &tuning,
        )
    } else {
        profile.view(exterior_f, &tuning)
    };
    if !snap.blend.is_finite() || !snap.boom_m.is_finite() {
        return Err(reason("interior_bad_boom", "the boom was not a finite length"));
    }
    let fields = snap_json(&snap);
    let mut body = fields.clone();
    body["state"] = fields;
    if let (Some(pivot), Some(desired)) = (pivot, desired) {
        let (look, eye) = shaped_boom(pivot, desired, snap.blend, &tuning);
        if !eye.iter().all(|n| n.is_finite()) {
            return Err(reason("interior_bad_state", "the shaped eye was not finite"));
        }
        body["look"] = json!([f64::from(look[0]), f64::from(look[1]), f64::from(look[2])]);
        body["eye"] = json!([f64::from(eye[0]), f64::from(eye[1]), f64::from(eye[2])]);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use serde_json::json;

    use super::*;
    use crate::dispatch::call_in_process;

    fn call(params: Value) -> Result<Value, RpcError> {
        let mut world = World::new();
        call_in_process(&mut world, "camera.interior_state", params).expect("the call answered")
    }

    fn reason_code(err: &RpcError) -> &str {
        err.data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    #[test]
    fn camera_interior_state_round_trips_the_blend() {
        let fresh = call(json!({})).expect("defaults");
        assert_eq!(fresh["zone"], "exterior");
        assert_eq!(fresh["blend"], 0.0);
        assert_eq!(fresh["boom_m"], 4.5);
        assert_eq!(fresh["seed"], true);
        let again = call(json!({})).expect("defaults again");
        assert_eq!(again["seed"], true);

        let outdoor = call(json!({
            "dt_s": 1.0 / 60.0,
            "under_lid": false,
            "near_mouth": false,
        }))
        .expect("seed frame");
        assert_eq!(outdoor["seed"], false);
        assert_eq!(outdoor["blend"], 0.0);
        assert_eq!(outdoor["zone"], "exterior");

        let mut state = outdoor["state"].clone();
        let mut blend = 0.0;
        for frame in 1..=30 {
            let snap = call(json!({
                "dt_s": 1.0 / 60.0,
                "near_mouth": true,
                "state": state,
            }))
            .expect("mouth step");
            blend = snap["blend"].as_f64().unwrap();
            state = snap["state"].clone();
            assert_eq!(snap["zone"], "mouth");
            if frame == 15 {
                assert!((blend - 0.5).abs() < 1.0e-4, "{blend}");
            }
            if frame == 29 {
                assert!(blend < 1.0, "{blend}");
            }
        }
        assert_eq!(blend, 1.0);

        let half = call(json!({
            "dt_s": 0.5,
            "near_mouth": true,
            "pivot": [0.0, 0.0, 0.0],
            "desired": [4.5, 0.0, 0.0],
            "state": { "zone": "exterior", "blend": 0.0, "seed": false },
        }))
        .expect("half second");
        assert_eq!(half["blend"], 1.0);
        assert_eq!(half["boom_m"], 2.5);
        let eye = half["eye"].as_array().unwrap();
        assert!((eye[0].as_f64().unwrap() - 2.5).abs() < 1.0e-4, "{eye:?}");
        assert_eq!(half["look"][0], 0.0);
    }

    #[test]
    fn camera_interior_state_refuses_bad_params() {
        let bad_dt = call(json!({ "dt_s": -0.1 })).expect_err("negative dt");
        assert_eq!(reason_code(&bad_dt), "interior_bad_dt");
        let bad_boom = call(json!({ "exterior_boom_m": -1.0 })).expect_err("negative boom");
        assert_eq!(reason_code(&bad_boom), "interior_bad_boom");
        let bad_zone = call(json!({ "state": { "zone": "sky" } })).expect_err("zone");
        assert_eq!(reason_code(&bad_zone), "interior_bad_zone");
        let bad_pair = call(json!({ "pivot": [0.0, 0.0, 0.0] })).expect_err("pair");
        assert_eq!(reason_code(&bad_pair), "interior_bad_state");
        let wrong = call(json!({ "state": { "zone": 1 } })).expect_err("type");
        let param = wrong
            .data
            .as_ref()
            .and_then(|data| data.get("param"))
            .and_then(Value::as_str);
        assert_eq!(param, Some("state.zone"));
    }
}
