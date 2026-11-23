// SPDX-License-Identifier: MPL-2.0
//! `ai.perception_hysteresis` (ADR-4586, `book/agents/perception-hysteresis.md`): read or set the
//! lose cone on one agent's [`regia_ai::Perception`].
//!
//! Absent or null lose fields are a read. A present field is validated together with the one that
//! was left out, and either both are written or neither is. `perception_bad_hysteresis` is the
//! refusal. A missing plugin is `ai_perception_unavailable`. An entity that is absent, unknown or
//! not an agent is `F0003`. A parameter of the wrong JSON type is `F0002`.

use regia_ai::Perception;
use regia_ai::perception_hysteresis::{
    HysteresisReject, SightRanges, assign_hysteresis, effective_lose_half_angle, effective_lose_radius,
};
use regia_ai::perception_system::PerceptionStats;
use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

fn unavailable() -> RpcError {
    RpcError::new(
        protocol::UNSUPPORTED,
        "ai_perception_unavailable",
        "Perception is not installed in this app",
        "add regia_ai::PerceptionPlugin",
    )
}

fn entity_param(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let Some(bits) = crate::params::opt_u64(params, "entity")? else {
        return Err(RpcError::not_found(
            "`entity` is required",
            "pass the entity id from scene.query_entities",
        ));
    };
    let Some(entity) = Entity::try_from_bits(bits) else {
        return Err(RpcError::not_found(
            format!("entity {bits} does not exist"),
            "scene.query_entities lists live entities",
        ));
    };
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", entity.to_bits()),
            "scene.query_entities lists live entities",
        ));
    }
    Ok(entity)
}

fn snapshot(entity: Entity, perception: &Perception, applied: bool) -> Value {
    let ranges = SightRanges {
        acquire_radius: perception.sight_radius,
        acquire_half_angle: perception.sight_half_angle,
        lose_radius: perception.sight_lose_radius,
        lose_half_angle: perception.sight_lose_half_angle,
    };
    json!({
        "entity": entity.to_bits(),
        "sight_radius": perception.sight_radius,
        "sight_half_angle": perception.sight_half_angle,
        "sight_lose_radius": perception.sight_lose_radius,
        "sight_lose_half_angle": perception.sight_lose_half_angle,
        "effective_lose_radius": effective_lose_radius(ranges),
        "effective_lose_half_angle": effective_lose_half_angle(ranges),
        "applied": applied,
    })
}

/// `ai.perception_hysteresis`: the lose cone, or a refusal that leaves the component unchanged.
pub fn perception_hysteresis(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<PerceptionStats>().is_none() {
        return Err(unavailable());
    }
    let entity = entity_param(world, params)?;
    let lose_radius = crate::params::opt_f64(params, "sight_lose_radius")?.map(|value| value as f32);
    let lose_half_angle = crate::params::opt_f64(params, "sight_lose_half_angle")?.map(|value| value as f32);
    let writing = lose_radius.is_some() || lose_half_angle.is_some();
    if writing {
        let Some(mut perception) = world.get_mut::<Perception>(entity) else {
            return Err(RpcError::not_found(
                format!("entity {} has no Perception", entity.to_bits()),
                "add a Perception component to the agent",
            ));
        };
        if let Err(HysteresisReject) = assign_hysteresis(&mut perception, lose_radius, lose_half_angle) {
            return Err(RpcError::new(
                protocol::INVALID_PARAMS,
                HysteresisReject::CODE,
                "the lose cone must be at least as wide as the acquire cone",
                "use 0 to keep the acquire range; a positive sight_lose_radius must be >= sight_radius \
                 and a positive sight_lose_half_angle must be >= sight_half_angle",
            ));
        }
        return Ok(snapshot(entity, &perception, true));
    }
    let Some(perception) = world.get::<Perception>(entity) else {
        return Err(RpcError::not_found(
            format!("entity {} has no Perception", entity.to_bits()),
            "add a Perception component to the agent",
        ));
    };
    Ok(snapshot(entity, perception, false))
}

#[cfg(test)]
mod tests {
    use regia_ai::perception_system::PerceptionPlugin;
    use regia_scene::Transform;
    use serde_json::json;

    use super::*;

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str)
            .map_or_else(|| err.message.clone(), str::to_string)
    }

    fn agent_with(radius: f32, half: f32) -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, PerceptionPlugin));
        let entity = app
            .world_mut()
            .spawn(Perception {
                sight_radius: radius,
                sight_half_angle: half,
                ..Perception::default()
            })
            .id();
        (app, entity)
    }

    #[test]
    fn ai_perception_hysteresis_sets_the_lose_cone_and_keeps_old_values_on_refusal() {
        let method = "ai.perception_hysteresis";
        assert_eq!(method, "ai.perception_hysteresis");
        let half = std::f32::consts::FRAC_PI_2;
        let (mut app, entity) = agent_with(10.0, half);
        let set = perception_hysteresis(
            app.world_mut(),
            &json!({ "entity": entity.to_bits(), "sight_lose_radius": 14.0 }),
        )
        .expect("14 is at least 10");
        assert_eq!(set["applied"], true);
        assert_eq!(set["sight_lose_radius"], 14.0);
        assert_eq!(set["effective_lose_radius"], 14.0);
        let perception = app.world().get::<Perception>(entity).expect("perception");
        assert_eq!(perception.sight_lose_radius, 14.0);
        assert_eq!(perception.sight_lose_half_angle, 0.0);
        assert_eq!(perception.sight_half_angle, half);

        let err = perception_hysteresis(
            app.world_mut(),
            &json!({ "entity": entity.to_bits(), "sight_lose_radius": 9.0 }),
        )
        .expect_err("below the acquire radius");
        assert_eq!(code(&err), "perception_bad_hysteresis");
        let perception = app.world().get::<Perception>(entity).expect("perception");
        assert_eq!(perception.sight_lose_radius, 14.0);
        assert_eq!(perception.sight_lose_half_angle, 0.0);

        let err = perception_hysteresis(
            app.world_mut(),
            &json!({ "entity": entity.to_bits(), "sight_lose_half_angle": 0.2 }),
        )
        .expect_err("below the acquire half-angle");
        assert_eq!(code(&err), "perception_bad_hysteresis");
        let perception = app.world().get::<Perception>(entity).expect("perception");
        assert_eq!(perception.sight_lose_radius, 14.0);
        assert_eq!(perception.sight_lose_half_angle, 0.0);

        let err = perception_hysteresis(
            app.world_mut(),
            &json!({ "entity": entity.to_bits(), "sight_lose_radius": -1.0 }),
        )
        .expect_err("negative");
        assert_eq!(code(&err), "perception_bad_hysteresis");

        let err = perception_hysteresis(
            app.world_mut(),
            &json!({ "entity": entity.to_bits(), "sight_lose_radius": 1e308 }),
        )
        .expect_err("not finite as f32");
        assert_eq!(code(&err), "perception_bad_hysteresis");
        let perception = app.world().get::<Perception>(entity).expect("perception");
        assert_eq!(perception.sight_lose_radius, 14.0);

        let read =
            perception_hysteresis(app.world_mut(), &json!({ "entity": entity.to_bits() })).expect("read");
        assert_eq!(read["applied"], false);
        assert_eq!(read["sight_radius"], 10.0);
        assert_eq!(read["sight_lose_radius"], 14.0);
        assert_eq!(read["entity"], entity.to_bits());
    }

    #[test]
    fn ai_perception_hysteresis_reports_a_missing_plugin_and_a_missing_entity() {
        let mut bare = App::new();
        bare.add_plugins(MinimalPlugins);
        let err = perception_hysteresis(bare.world_mut(), &json!({})).expect_err("no plugin");
        assert_eq!(code(&err), "ai_perception_unavailable");

        let (mut app, _agent) = agent_with(10.0, 1.0);
        let err = perception_hysteresis(app.world_mut(), &json!({})).expect_err("no entity");
        assert_eq!(code(&err), "F0003");
        let err = perception_hysteresis(app.world_mut(), &json!({ "entity": "x" })).expect_err("type");
        assert_eq!(code(&err), "F0002");
        let err = perception_hysteresis(app.world_mut(), &json!({ "entity": u64::MAX })).expect_err("bits");
        assert_eq!(code(&err), "F0003");
        let other = app.world_mut().spawn(Transform::default()).id();
        let err = perception_hysteresis(app.world_mut(), &json!({ "entity": other.to_bits() }))
            .expect_err("component");
        assert_eq!(code(&err), "F0003");
    }
}
