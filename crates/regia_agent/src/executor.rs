// SPDX-License-Identifier: MPL-2.0
//! The schedule executor on the agent protocol (ADR-4933, book `agents/schedule-executor.md`):
//! `executor.get` reports which `bevy_ecs` executor the schedules run on, `executor.set` asks for
//! the other one. The app installs a change before the next frame's first schedule.

use regia_app::prelude::*;
use regia_app::{ScheduleExecutor, ScheduleExecutorSetting};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn executor_json(world: &World) -> Value {
    let default = ScheduleExecutor::platform_default();
    match world.get_resource::<ScheduleExecutorSetting>() {
        Some(s) => json!({
            "executor": s.applied().as_str(),
            "requested": s.requested.as_str(),
            "pending": s.requested != s.applied(),
            "platform_default": default.as_str(),
            "switched_schedules": s.switched_schedules(),
            "switches": s.switches(),
        }),
        None => json!({
            "executor": ScheduleExecutor::MultiThreaded.as_str(),
            "requested": ScheduleExecutor::MultiThreaded.as_str(),
            "pending": false,
            "platform_default": default.as_str(),
            "switched_schedules": 0,
            "switches": 0,
        }),
    }
}

/// `executor.get`.
pub(crate) fn executor_get(world: &mut World) -> Result<Value, RpcError> {
    Ok(executor_json(world))
}

/// `executor.set`: `executor` is `multi_threaded`, `single_threaded` or `default` (the
/// platform default).
pub(crate) fn executor_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = params.get("executor").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params("`executor` is required: multi_threaded | single_threaded | default")
    })?;
    let kind = if name == "default" {
        ScheduleExecutor::platform_default()
    } else {
        ScheduleExecutor::parse(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown executor {name:?}: multi_threaded | single_threaded | default"
            ))
        })?
    };
    world
        .get_resource_or_insert_with(ScheduleExecutorSetting::default)
        .requested = kind;
    Ok(executor_json(world))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.add_systems(Update, || {});
        app
    }

    #[test]
    fn set_switches_every_schedule_on_the_next_update() {
        let mut app = app();
        app.set_schedule_executor(ScheduleExecutor::MultiThreaded);
        app.update();
        let r = executor_set(app.world_mut(), &json!({ "executor": "single_threaded" })).expect("answers");
        assert_eq!(r["requested"], "single_threaded");
        assert_eq!(r["pending"], true);
        app.update();
        let r = executor_get(app.world_mut()).expect("answers");
        assert_eq!(r["executor"], "single_threaded");
        assert_eq!(r["pending"], false);
        assert_eq!(r["switches"], 1);
        assert!(r["switched_schedules"].as_u64().expect("a count") > 5, "{r}");
        // Back to multi-threaded, and `default` names the platform default.
        executor_set(app.world_mut(), &json!({ "executor": "multi_threaded" })).expect("answers");
        app.update();
        assert_eq!(
            executor_get(app.world_mut()).expect("answers")["executor"],
            "multi_threaded"
        );
        let r = executor_set(app.world_mut(), &json!({ "executor": "default" })).expect("answers");
        assert_eq!(r["requested"], ScheduleExecutor::platform_default().as_str());
    }

    #[test]
    fn set_rejects_unknown_and_missing_names() {
        let mut app = app();
        for params in [
            json!({}),
            json!({ "executor": "parallel" }),
            json!({ "executor": 3 }),
        ] {
            let err = executor_set(app.world_mut(), &params).expect_err("rejected");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{params}");
        }
    }
}
