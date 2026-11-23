// SPDX-License-Identifier: MPL-2.0
//! The quick app layer on the agent protocol (`book/agents/quick-app.md`): the quick app's reflected
//! model and its loop state, as the `QuickModel` and `QuickLoop` resources of `play.wait_until` /
//! `play.expect` predicates.

use regia_app::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, ReflectComponent};
use regia_quick::{QuickLoop, RunMode};
use serde_json::{Value, json};

/// Type-path prefix of every quick app model resource (`regia_quick::app::Model<my_game::Model>`).
const MODEL_PREFIX: &str = "regia_quick::app::Model<";

/// The quick app's model as JSON (the `model` field of the `Model<M>` resource).
pub fn model_snapshot(world: &World) -> Result<Value, String> {
    let registry = world
        .get_resource::<AppTypeRegistry>()
        .ok_or_else(|| "no type registry in this app".to_string())?
        .clone();
    let registry = registry.read();
    for registration in registry.iter() {
        let path = registration.type_info().type_path();
        if !path.starts_with(MODEL_PREFIX) {
            continue;
        }
        let (Some(reflect), Some(id)) = (
            registration.data::<ReflectComponent>(),
            world.components().get_id(registration.type_id()),
        ) else {
            continue;
        };
        let Some(entity) = world.resource_entities().get(id) else {
            continue;
        };
        let Some(value) = reflect.reflect(world.entity(entity)) else {
            continue;
        };
        let json = crate::scene::serialize_reflect(value.as_partial_reflect(), &registry)?;
        return Ok(json.get("model").cloned().unwrap_or(json));
    }
    Err("no quick app model in this app (regia::quick::app(...) inserts Model<M>)".into())
}

/// The quick app's run mode and loop counters as JSON.
pub fn loop_snapshot(world: &World) -> Result<Value, String> {
    let state = world
        .get_resource::<QuickLoop>()
        .ok_or_else(|| "no quick app in this app (no QuickLoop resource)".to_string())?;
    let mode = match world.get_resource::<RunMode>().copied().unwrap_or_default() {
        RunMode::Loop => json!({ "mode": "Loop" }),
        RunMode::Wait => json!({ "mode": "Wait" }),
        RunMode::LoopNTimes(n) => json!({ "mode": "LoopNTimes", "n": n }),
    };
    Ok(json!({
        "run_mode": mode,
        "runs": state.runs,
        "ran_this_frame": state.run_this_frame,
        "frozen": state.frozen,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Reflect, Default)]
    struct Score {
        points: u32,
        label: String,
    }

    #[test]
    fn model_and_loop_are_readable_as_json() {
        let mut app = App::new();
        assert!(model_snapshot(app.world()).is_err());
        assert!(loop_snapshot(app.world()).is_err());
        app.add_plugins(
            regia_quick::app(|_: &regia_quick::Quick| Score {
                points: 0,
                label: "go".into(),
            })
            .update(|_: &regia_quick::Quick, s: &mut Score| s.points += 2)
            .loop_ntimes(2),
        );
        for _ in 0..4 {
            app.update();
        }
        let model = model_snapshot(app.world()).expect("model");
        assert_eq!(model["points"], 4);
        assert_eq!(model["label"], "go");
        let state = loop_snapshot(app.world()).expect("loop");
        assert_eq!(state["run_mode"]["mode"], "LoopNTimes");
        assert_eq!(state["runs"], 2);
        assert_eq!(state["frozen"], true);
    }
}
