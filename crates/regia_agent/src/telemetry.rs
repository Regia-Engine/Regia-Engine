// SPDX-License-Identifier: MPL-2.0
//! `telemetry.get_last_summary`: the newest `regia::frame_summary` record, the session header and
//! the telemetry counts (`book/user-guide/telemetry.md`). `telemetry.live` (v0.1.14): the newest
//! frame's render scale, GPU time, bound class and GPU waits.

use regia_app::prelude::*;
use regia_telemetry::Telemetry;
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn parse(text: Option<&str>) -> Value {
    text.and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or(Value::Null)
}

fn telemetry_off() -> RpcError {
    RpcError::not_found(
        "telemetry is off",
        "add regia_telemetry::TelemetryPlugin (in DefaultPlugins) and unset REGIA_TELEMETRY=off",
    )
}

/// `telemetry.live`: [`Telemetry::live`] (the newest frame) and the bound thresholds.
pub(crate) fn live(world: &World) -> Result<Value, RpcError> {
    let telemetry = world.get_resource::<Telemetry>().ok_or_else(telemetry_off)?;
    Ok(json!({
        "live": telemetry.live(),
        "thresholds": {
            "gpu_bound_share": regia_telemetry::GPU_BOUND_SHARE,
            "cpu_bound_share": regia_telemetry::CPU_BOUND_SHARE,
            "event_wait_share": regia_telemetry::EVENT_WAIT_SHARE,
            "hitch_floor_ms": telemetry.hitch_floor_ms(),
            "pacing_period_ms": telemetry.pacing_period_ms(),
            "present_wait_share": telemetry.options().present_wait_share,
        },
    }))
}

/// The last summary (null before the first one, ~10 s after start), the session header (null
/// before the first rendered frame) and the counts since start.
pub(crate) fn get_last_summary(world: &World, params: &Value) -> Result<Value, RpcError> {
    let telemetry = world.get_resource::<Telemetry>().ok_or_else(telemetry_off)?;
    let with_session = crate::params::opt_bool(params, "session")?.unwrap_or(false);
    let with_record = crate::params::opt_bool(params, "session_summary")?.unwrap_or(false);
    let paths = telemetry
        .summary_paths()
        .map(|p| json!({ "final": p.final_path, "partial": p.partial_path }));
    Ok(json!({
        "summary": parse(telemetry.last_summary_json()),
        "session": if with_session { parse(telemetry.session_json()) } else { Value::Null },
        "session_summary": if with_record {
            serde_json::to_value(telemetry.build_summary(true)).unwrap_or(Value::Null)
        } else {
            Value::Null
        },
        "summary_paths": paths,
        "counts": telemetry.counts(),
        "level": telemetry.options().level.name(),
        "summary_interval_s": telemetry.options().summary_interval.as_secs_f64(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_off_then_counts() {
        let mut world = World::new();
        assert!(get_last_summary(&world, &json!({})).is_err());
        world.insert_resource(Telemetry::new(regia_telemetry::TelemetryOptions::default()));
        let v = get_last_summary(&world, &json!({ "session": true })).expect("telemetry on");
        assert_eq!(v["summary"], Value::Null);
        assert_eq!(v["counts"]["frames"], 0);
        assert_eq!(v["level"], "verbose");
        assert_eq!(v["session_summary"], Value::Null);
        let v = get_last_summary(&world, &json!({ "session_summary": true })).expect("telemetry on");
        assert_eq!(v["session_summary"]["schema"], 1);
        assert_eq!(v["session_summary"]["partial"], true);
    }

    #[test]
    fn live_reports_off_then_the_newest_frame() {
        let mut world = World::new();
        assert!(live(&world).is_err());
        world.insert_resource(Telemetry::new(regia_telemetry::TelemetryOptions::default()));
        let v = live(&world).expect("telemetry on");
        assert_eq!(v["live"]["frame"], 0);
        assert_eq!(v["live"]["bound"], "unknown");
        assert_eq!(v["live"]["render_scale_permille"], Value::Null);
        assert!(v["live"]["wait_acquire_ms"].is_number());
        assert!(
            v["thresholds"]["gpu_bound_share"]
                .as_f64()
                .is_some_and(|s| s > 0.85)
        );
    }
}
