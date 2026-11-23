// SPDX-License-Identifier: MPL-2.0
//! The frame's exposure on the agent protocol (`book/agents/exposure-zones.md`, ADR-322):
//! `render.get_exposure` reports the main view's last exposure and the exposure-zone eye,
//! `render.exposure_log` starts, stops and reads the `(time, ev, target)` curve, and the
//! `exposure_settled` wait predicate holds once the eye is within a tolerance of its goal.
//!
//! Error reasons (`data.reason`): `renderer_missing`, `invalid_action`, `invalid_cap`,
//! `invalid_tolerance`.

use regia_app::prelude::*;
use regia_framework::exposure_zones::ExposureZoneState;
use regia_render::RuntimeRenderer;
use regia_render::exposure_zones::{
    EXPOSURE_LOG_DEFAULT_CAP, EXPOSURE_SETTLED_TOLERANCE_EV, EvTarget, ExposureLog, ExposureReport,
};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Largest `cap` `render.exposure_log` accepts: 262 144 samples, about 2.4 hours at 30 fps.
const MAX_LOG_CAP: u64 = 1 << 18;

fn error(code: i64, regia_code: &str, reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: Some(json!({ "code": regia_code, "reason": reason, "hint": hint })),
    }
}

fn renderer_missing() -> RpcError {
    error(
        protocol::NOT_FOUND,
        "F0003",
        "renderer_missing",
        "no renderer",
        "render.get_exposure and render.exposure_log need the render plugin (a windowed or headless app with RuntimeRenderer)",
    )
}

fn invalid(reason: &str, message: &str) -> RpcError {
    error(
        protocol::INVALID_PARAMS,
        "F0002",
        reason,
        message,
        "see book/agents/exposure-zones.md",
    )
}

fn target_json(target: Option<EvTarget>) -> Value {
    target.map_or(Value::Null, |t| {
        json!({
            "ev_min": t.ev_min,
            "ev_max": t.ev_max,
            "target_ev": t.target_ev(),
            "adapt_up_speed": t.adapt_up_speed,
            "adapt_down_speed": t.adapt_down_speed,
        })
    })
}

fn report_json(report: Option<ExposureReport>) -> Value {
    report.map_or(Value::Null, |r| {
        json!({
            "ev": r.ev,
            "goal": r.goal,
            "meter_ev": r.meter_ev,
            "mode": r.mode.name(),
        })
    })
}

fn log_json(log: Option<&ExposureLog>) -> Vec<Value> {
    log.map(ExposureLog::samples)
        .unwrap_or_default()
        .iter()
        .map(|s| json!({ "time": s.time, "ev": s.ev, "target": s.target }))
        .collect()
}

/// The baked section volumes `render.get_exposure` lists (`book/agents/exposure-zones.md`).
///
/// Absent, the call returns `zones: null`. Present, it returns one object per entry, with no cap
/// of four. The aerial composite still packs four interior slots.
#[derive(Resource, Clone, Debug, Default)]
pub struct BakedZoneList {
    /// One row per cooked section, in section order.
    pub zones: Vec<regia_world_geom::zones::SectionVolumes>,
}

fn zones_json(world: &World) -> Value {
    match world.get_resource::<BakedZoneList>() {
        None => Value::Null,
        Some(list) => Value::Array(list.zones.iter().map(section_volume_json).collect()),
    }
}

fn section_volume_json(volume: &regia_world_geom::zones::SectionVolumes) -> Value {
    json!({
        "section": volume.section,
        "exposure_ev": volume.exposure_ev,
        "blend_m": volume.blend_m,
        "reverb_preset": volume.reverb_preset,
        "reverb_size_m": volume.reverb_size_m,
        "interior_center_m": volume.interior_center_m,
        "interior_half_m": volume.interior_half_m,
    })
}

/// `render.get_exposure`: the main view's last exposure, the zone target, the eye's state and the
/// baked zone list.
pub(crate) fn get_exposure(world: &mut World) -> Result<Value, RpcError> {
    let zone_count = world.get_resource::<ExposureZoneState>().map(|s| s.zone_count);
    let zones = zones_json(world);
    let runtime = world
        .get_resource::<RuntimeRenderer>()
        .ok_or_else(renderer_missing)?;
    let renderer = &runtime.0;
    let eye = renderer.exposure_zones();
    let report = eye.report();
    Ok(json!({
        "ev": report.map(|r| r.ev),
        "goal": report.map(|r| r.goal),
        "meter_ev": report.map(|r| r.meter_ev),
        "mode": report.map(|r| r.mode.name()),
        "adapting": report.is_some_and(|r| r.adapting()),
        "settled": report.map(|r| r.settled(EXPOSURE_SETTLED_TOLERANCE_EV)),
        "zone_target": target_json(eye.target()),
        "zone_count": zone_count,
        "metered_luminance": renderer.metered_luminance(),
        "logging": eye.is_logging(),
        "log_samples": eye.log().map_or(0, ExposureLog::len),
        "zones": zones,
    }))
}

/// `render.exposure_log { action: "start" | "stop" | "get", cap? }`.
pub(crate) fn exposure_log(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // A wrong-typed `action` answers the documented `invalid_action` reason like a missing one.
    let action = crate::params::opt_str(params, "action").unwrap_or(None);
    let cap = match params.get("cap") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|cap| (1..=MAX_LOG_CAP).contains(cap))
                .ok_or_else(|| invalid("invalid_cap", "`cap` must be an integer from 1 to 262144"))?,
        ),
    };
    if !matches!(action, Some("start" | "stop" | "get")) {
        return Err(invalid(
            "invalid_action",
            "`action` must be \"start\", \"stop\" or \"get\"",
        ));
    }
    if cap.is_some() && action != Some("start") {
        return Err(invalid("invalid_cap", "`cap` applies to \"start\" only"));
    }
    let mut runtime = world
        .get_resource_mut::<RuntimeRenderer>()
        .ok_or_else(renderer_missing)?;
    let eye = runtime.0.exposure_zones_mut();
    match action {
        Some("start") => {
            let cap = cap.map_or(EXPOSURE_LOG_DEFAULT_CAP, |cap| cap as usize);
            eye.start_log(cap);
            Ok(json!({ "logging": true, "cap": cap }))
        }
        Some("stop") => {
            eye.stop_log();
            Ok(json!({ "logging": false, "count": eye.log().map_or(0, ExposureLog::len) }))
        }
        _ => {
            let samples = log_json(eye.log());
            Ok(json!({
                "logging": eye.is_logging(),
                "count": samples.len(),
                "last": report_json(eye.report()),
                "samples": samples,
            }))
        }
    }
}

/// The wait atom `{ "predicate": "exposure_settled", "tolerance"? }`: holds when the main
/// view's last exposure is within `tolerance` EV (default 0.05) of the goal it chases.
/// `Ok((false, ..))` before the first exposed frame, so a wait keeps polling; `Err` when it
/// cannot be evaluated at all.
pub(crate) fn settled_predicate(world: &World, atom: &Value) -> Result<(bool, String), String> {
    let tolerance = match atom.get("tolerance") {
        None | Some(Value::Null) => EXPOSURE_SETTLED_TOLERANCE_EV,
        Some(value) => value
            .as_f64()
            .filter(|t| t.is_finite() && *t > 0.0)
            .map(|t| t as f32)
            .ok_or_else(|| "invalid_tolerance: `tolerance` must be a number > 0 (EV)".to_owned())?,
    };
    let runtime = world
        .get_resource::<RuntimeRenderer>()
        .ok_or_else(|| "renderer_missing: exposure_settled needs the render plugin".to_owned())?;
    let Some(report) = runtime.0.exposure_zones().report() else {
        return Ok((false, "no exposed frame yet".to_owned()));
    };
    let gap = (report.ev - report.goal).abs();
    Ok((
        report.settled(tolerance),
        format!(
            "exposure ({}): ev {:.3}, goal {:.3}, |ev - goal| {gap:.3} EV, tolerance {tolerance}",
            report.mode.name(),
            report.ev,
            report.goal,
        ),
    ))
}

#[cfg(test)]
mod tests {
    use regia_render::exposure_zones::{EvRange, MeterReading};

    use super::*;

    fn reason(err: &RpcError) -> Option<&str> {
        err.data.as_ref().and_then(|d| d["reason"].as_str())
    }

    #[test]
    fn without_a_renderer_every_call_says_so() {
        let mut world = World::new();
        let err = get_exposure(&mut world).expect_err("no renderer");
        assert_eq!(err.code, protocol::NOT_FOUND);
        assert_eq!(reason(&err), Some("renderer_missing"));
        let err = exposure_log(&mut world, &json!({ "action": "get" })).expect_err("no renderer");
        assert_eq!(reason(&err), Some("renderer_missing"));
        let err = settled_predicate(&world, &json!({})).expect_err("no renderer");
        assert!(err.starts_with("renderer_missing"), "{err}");
    }

    #[test]
    fn bad_parameters_carry_snake_case_reasons() {
        let mut world = World::new();
        world.init_resource::<RuntimeRenderer>();
        for params in [json!({}), json!({ "action": "pause" }), json!({ "action": 3 })] {
            let err = exposure_log(&mut world, &params).expect_err("bad action");
            assert_eq!(err.code, protocol::INVALID_PARAMS);
            assert_eq!(reason(&err), Some("invalid_action"), "{params}");
        }
        for params in [
            json!({ "action": "start", "cap": 0 }),
            json!({ "action": "start", "cap": -4 }),
            json!({ "action": "start", "cap": "many" }),
            json!({ "action": "start", "cap": 1_000_000 }),
            json!({ "action": "get", "cap": 10 }),
        ] {
            let err = exposure_log(&mut world, &params).expect_err("bad cap");
            assert_eq!(reason(&err), Some("invalid_cap"), "{params}");
        }
        let err = settled_predicate(&world, &json!({ "tolerance": -1.0 })).expect_err("bad tolerance");
        assert!(err.starts_with("invalid_tolerance"), "{err}");
        for reason in [
            "renderer_missing",
            "invalid_action",
            "invalid_cap",
            "invalid_tolerance",
        ] {
            assert!(
                reason
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            );
        }
    }

    #[test]
    fn the_log_and_the_report_follow_the_zone_eye() {
        let mut world = World::new();
        world.init_resource::<RuntimeRenderer>();
        let got = get_exposure(&mut world).expect("get");
        assert_eq!(got["mode"], Value::Null, "no frame yet");
        assert_eq!(got["zone_target"], Value::Null);
        assert_eq!(got["zones"], Value::Null, "no baked zone list");
        assert_eq!(got["adapting"], json!(false));
        let (holds, detail) = settled_predicate(&world, &json!({})).expect("evaluates");
        assert!(!holds, "{detail}");

        let started = exposure_log(&mut world, &json!({ "action": "start", "cap": 64 })).expect("start");
        assert_eq!(started["cap"], json!(64));
        {
            let mut runtime = world.resource_mut::<RuntimeRenderer>();
            runtime.set_exposure_target(Some(EvTarget::from_range(EvRange::point(8.5))));
            let eye = runtime.0.exposure_zones_mut();
            for _ in 0..3 {
                let _ev = eye.step(MeterReading::steady(14.5), 1.0 / 30.0);
            }
        }
        let got = get_exposure(&mut world).expect("get");
        assert_eq!(got["mode"], json!("zone"));
        assert_eq!(got["adapting"], json!(true));
        assert_eq!(got["settled"], json!(false));
        assert_eq!(got["zone_target"]["target_ev"], json!(8.5));
        assert_eq!(got["logging"], json!(true));
        assert_eq!(got["log_samples"], json!(3));
        let (holds, _) = settled_predicate(&world, &json!({})).expect("evaluates");
        assert!(!holds, "three steps from EV 14.5 is not settled");
        let (holds, _) = settled_predicate(&world, &json!({ "tolerance": 10.0 })).expect("evaluates");
        assert!(holds, "a loose tolerance holds");

        let log = exposure_log(&mut world, &json!({ "action": "get" })).expect("get");
        let samples = log["samples"].as_array().expect("samples");
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0]["time"], json!(0.0));
        assert_eq!(samples[0]["target"], json!(8.5));
        let first = samples[0]["ev"].as_f64().unwrap_or(f64::NAN);
        let last = samples[2]["ev"].as_f64().unwrap_or(f64::NAN);
        assert!(first < 14.5 && last < first && last > 8.5, "{first} -> {last}");
        assert_eq!(log["last"]["mode"], json!("zone"));

        let stopped = exposure_log(&mut world, &json!({ "action": "stop" })).expect("stop");
        assert_eq!(stopped["count"], json!(3));
        assert_eq!(stopped["logging"], json!(false));
    }
}
