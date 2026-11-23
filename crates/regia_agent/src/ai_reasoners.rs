// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for perception, utility reasoners and GOAP agents (ADR-2565,
//! `book/agents/ai-perception.md`, `book/agents/utility-ai.md`, `book/user-guide/goap.md`):
//! `ai.perception_get`, `ai.team_knowledge`, `ai.senses`, `ai.attitude_set`, `ai.utility_list`,
//! `ai.utility_explain` and `ai.goap_plan`.
//!
//! Each answers `ai_perception_unavailable`, `ai_utility_unavailable` or `ai_goap_unavailable` when
//! its plugin is not installed, `F0002` with `data.reason` for a bad parameter and `F0003` with
//! `data.reason` for an entity that is not there or is not that kind of agent.

use regia_ai::goap::IntCmp;
use regia_ai::goap_ecs::{GoapAgent, GoapStats, GoapStatus};
use regia_ai::perception_system::{
    Attitude, Attitudes, DetectionSample, Perceived, PerceptionSenses, PerceptionSettings, PerceptionStats,
    SharedStimulus, Stimulus, TeamKnowledge,
};
use regia_ai::utility_ecs::{UtilityClock, UtilityDefs, UtilityReasoner};
use regia_ai::{Blackboard, Perception};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use serde_json::{Map, Value, json};

use crate::protocol::{self, RpcError};

fn with_reason(mut error: RpcError, reason: &str) -> RpcError {
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn invalid(reason: &str, message: impl Into<String>) -> RpcError {
    with_reason(
        RpcError::new(
            protocol::INVALID_PARAMS,
            "F0002",
            message,
            "see book/agents/ai-perception.md for the parameters",
        ),
        reason,
    )
}

fn missing(reason: &str, message: impl Into<String>) -> RpcError {
    with_reason(
        RpcError::new(
            protocol::NOT_FOUND,
            "F0003",
            message,
            "scene.query_entities lists the entities with that component",
        ),
        reason,
    )
}

fn unavailable(code: &str, what: &str, plugin: &str) -> RpcError {
    RpcError::new(
        protocol::UNSUPPORTED,
        code,
        format!("{what} is not installed in this app"),
        format!("add regia_ai::{plugin}"),
    )
}

/// An optional entity parameter that must name a live entity when given.
fn entity_param(world: &World, params: &Value) -> Result<Option<Entity>, RpcError> {
    let Some(value) = params.get("entity").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let entity = value
        .as_u64()
        .and_then(Entity::try_from_bits)
        .ok_or_else(|| invalid("bad_entity", "`entity` must be an entity id"))?;
    if world.get_entity(entity).is_err() {
        return Err(missing(
            "entity_not_found",
            format!("entity {} does not exist", entity.to_bits()),
        ));
    }
    Ok(Some(entity))
}

/// A required entity parameter.
fn required_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    entity_param(world, params)?.ok_or_else(|| invalid("missing_param", "`entity` is required"))
}

/// The entity id the protocol shows; `null` for "none".
fn entity_json(entity: Entity) -> Value {
    if entity == Entity::PLACEHOLDER {
        Value::Null
    } else {
        json!(entity.to_bits())
    }
}

fn stimulus_json(stimulus: &Stimulus, tick: u64) -> Value {
    json!({
        "entity": entity_json(stimulus.entity),
        "sense": stimulus.sense.name(),
        "team": stimulus.team,
        "last_known_position": vec3_json(stimulus.last_known_position),
        "last_known_velocity": vec3_json(stimulus.last_known_velocity),
        "last_sensed_tick": stimulus.last_sensed_tick,
        "age_ticks": tick.saturating_sub(stimulus.last_sensed_tick),
        "strength": stimulus.strength,
        "confidence": stimulus.confidence,
    })
}

fn vec3_json(value: Vec3) -> Value {
    json!([value.x, value.y, value.z])
}

fn named_code(code: &'static str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(protocol::INVALID_PARAMS, code, message, hint)
}

fn perceived_json(
    entity: Entity,
    perceived: &Perceived,
    perception: Option<&Perception>,
    tick: u64,
) -> Value {
    json!({
        "entity": entity.to_bits(),
        "team": perception.map(|p| p.team),
        "target": entity_json(perceived.target),
        "max_age_ticks": perceived.max_age_ticks,
        "capacity": perceived.capacity,
        "meter": perceived.meter.iter().map(meter_json).collect::<Vec<_>>(),
        "stimuli": perceived.stimuli.iter().map(|s| stimulus_json(s, tick)).collect::<Vec<_>>(),
    })
}

fn meter_json(sample: &DetectionSample) -> Value {
    json!({
        "target": entity_json(sample.target),
        "evidence": sample.evidence,
    })
}

/// `ai.perception_get`: one agent's perceived list, or with no `entity` every agent's, in entity order.
pub fn perception_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(stats) = world.get_resource::<PerceptionStats>() else {
        return Err(unavailable(
            "ai_perception_unavailable",
            "perception",
            "PerceptionPlugin",
        ));
    };
    let tick = stats.tick;
    let summary = json!({
        "tick": tick,
        "sight_checks_last_tick": stats.sight_checks_last_tick,
        "sight_rays_last_tick": stats.sight_rays_last_tick,
        "sweeps_completed": stats.sweeps_completed,
        "messages_dropped": stats.messages_dropped,
    });
    let settings = world.get_resource::<PerceptionSettings>().map(|s| {
        json!({
            "sight_interval_ticks": s.sight_interval_ticks,
            "sight_checks_per_tick": s.sight_checks_per_tick,
            "detection_threshold": s.detection_threshold,
            "touch_interval_ticks": s.touch_interval_ticks,
            "sight_confidence_half_life": s.sight_confidence_half_life,
            "hearing_confidence_half_life": s.hearing_confidence_half_life,
            "damage_confidence_half_life": s.damage_confidence_half_life,
            "touch_confidence_half_life": s.touch_confidence_half_life,
        })
    });
    if let Some(entity) = entity_param(world, params)? {
        let Some(perceived) = world.get::<Perceived>(entity) else {
            return Err(missing(
                "not_a_perceiving_agent",
                format!("entity {} has no Perceived", entity.to_bits()),
            ));
        };
        return Ok(json!({
            "stats": summary,
            "settings": settings,
            "agent": perceived_json(entity, perceived, world.get::<Perception>(entity), tick),
        }));
    }
    let mut agents: Vec<(Entity, Value)> = Vec::new();
    let mut query = world.query::<(Entity, &Perceived, Option<&Perception>)>();
    for (entity, perceived, perception) in query.iter(world) {
        agents.push((entity, perceived_json(entity, perceived, perception, tick)));
    }
    agents.sort_by_key(|(entity, _)| (entity.index_u32(), entity.to_bits()));
    let agents: Vec<Value> = agents.into_iter().map(|(_, value)| value).collect();
    Ok(json!({ "stats": summary, "settings": settings, "agents": agents }))
}

fn shared_json(fact: &SharedStimulus) -> Value {
    json!({
        "entity": entity_json(fact.entity),
        "sense": fact.sense.name(),
        "source_team": fact.team,
        "position": vec3_json(fact.position),
        "velocity": vec3_json(fact.velocity),
        "confidence": fact.confidence,
        "strength": fact.strength,
        "sensed_tick": fact.sensed_tick,
        "reporter": entity_json(fact.reporter),
    })
}

/// `ai.team_knowledge`: facts one attitude-group team has shared. A team with no facts is an empty
/// list. The plugin is checked before `team`, so a missing plugin is `ai_perception_unavailable`
/// even when `team` is absent.
pub fn team_knowledge(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<PerceptionStats>().is_none() {
        return Err(unavailable(
            "ai_perception_unavailable",
            "perception",
            "PerceptionPlugin",
        ));
    }
    let Some(team) = params
        .get("team")
        .and_then(Value::as_u64)
        .and_then(|team| u8::try_from(team).ok())
    else {
        return Err(named_code(
            "unknown_team",
            "`team` must be an integer from 0 to 255",
            "see book/agents/ai-perception.md",
        ));
    };
    let facts = world
        .get_resource::<TeamKnowledge>()
        .map(|knowledge| knowledge.team(team).iter().map(shared_json).collect::<Vec<_>>())
        .unwrap_or_default();
    Ok(json!({ "team": team, "facts": facts }))
}

fn sense_json(id: u16, name: &str, range: f32, interval_ticks: u32, half_life_ticks: Option<u32>) -> Value {
    json!({
        "id": id,
        "name": name,
        "range": range,
        "interval_ticks": interval_ticks,
        "half_life_ticks": half_life_ticks,
    })
}

/// `ai.senses`: the custom-sense registry. No `name` lists every registered sense (an empty
/// registry is an empty list). A `name` that is not registered is `unknown_sense`.
pub fn senses(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<PerceptionStats>().is_none() {
        return Err(unavailable(
            "ai_perception_unavailable",
            "perception",
            "PerceptionPlugin",
        ));
    }
    let registry = world.get_resource::<PerceptionSenses>();
    if let Some(name) = params.get("name").filter(|value| !value.is_null()) {
        let Some(name) = name.as_str() else {
            return Err(named_code(
                "unknown_sense",
                "`name` must be the registered sense name",
                "ai.senses with no name lists the registry",
            ));
        };
        let Some(listed) = registry.as_ref().and_then(|senses| senses.by_name(name)) else {
            return Err(named_code(
                "unknown_sense",
                format!("no sense named `{name}` is registered"),
                "ai.senses with no name lists the registry",
            ));
        };
        return Ok(json!({
            "senses": [sense_json(
                listed.id,
                listed.name,
                listed.range,
                listed.interval_ticks,
                listed.half_life_ticks,
            )],
        }));
    }
    let senses = registry
        .as_ref()
        .map(|senses| {
            senses
                .iter()
                .map(|listed| {
                    sense_json(
                        listed.id,
                        listed.name,
                        listed.range,
                        listed.interval_ticks,
                        listed.half_life_ticks,
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(json!({ "senses": senses }))
}

fn team_param(params: &Value, key: &str) -> Result<u8, RpcError> {
    let bad = || invalid("bad_team", format!("`{key}` must be a team number from 0 to 255"));
    let team = params.get(key).and_then(Value::as_u64).ok_or_else(bad)?;
    u8::try_from(team).map_err(|_| bad())
}

/// `ai.attitude_set`: how team `from` regards team `to`, and with `symmetric` the other way as well.
pub fn attitude_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<Attitudes>().is_none() {
        return Err(unavailable(
            "ai_perception_unavailable",
            "perception",
            "PerceptionPlugin",
        ));
    }
    let from = team_param(params, "from")?;
    let to = team_param(params, "to")?;
    let name = params.get("attitude").and_then(Value::as_str).ok_or_else(|| {
        invalid(
            "bad_attitude",
            "`attitude` is required: hostile, neutral or friendly",
        )
    })?;
    let attitude = Attitude::from_name(name).ok_or_else(|| {
        invalid(
            "bad_attitude",
            format!("unknown attitude `{name}`: hostile, neutral or friendly"),
        )
    })?;
    let symmetric = crate::params::opt_bool(params, "symmetric")?.unwrap_or(false);
    let mut table = world.resource_mut::<Attitudes>();
    let previous = table.set(from, to, attitude);
    if symmetric {
        table.set(to, from, attitude);
    }
    Ok(json!({
        "from": from,
        "to": to,
        "attitude": attitude.name(),
        "previous": previous.name(),
        "reverse": table.get(to, from).name(),
    }))
}

fn utility_entry(entity: Entity, reasoner: &UtilityReasoner, has_blackboard: bool) -> Value {
    json!({
        "entity": entity.to_bits(),
        "path": reasoner.path,
        "interval_ticks": reasoner.interval_ticks,
        "action_key": reasoner.action_key,
        "current_action": reasoner.current_action,
        "decisions": reasoner.decisions,
        "generation": reasoner.generation,
        "loaded": reasoner.reasoner().is_some(),
        "has_blackboard": has_blackboard,
        "actions": reasoner.reasoner().map_or_else(Vec::new, regia_ai::Reasoner::list_decisions),
    })
}

/// `ai.utility_list`: every reasoner and every loaded `.utility.ron`, with the last load error.
pub fn utility_list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(defs) = world.get_resource::<UtilityDefs>() else {
        return Err(unavailable(
            "ai_utility_unavailable",
            "utility AI",
            "UtilityPlugin",
        ));
    };
    let definitions: Vec<Value> = defs
        .iter()
        .map(|(path, slot)| {
            json!({
                "path": path,
                "generation": slot.generation,
                "loaded": slot.definition.is_some(),
                "error": slot.error.as_ref().map(|e| json!({ "reason": e.reason, "detail": e.detail })),
            })
        })
        .collect();
    let mut rows: Vec<(Entity, Value)> = Vec::new();
    let mut query = world.query::<(Entity, &UtilityReasoner, Option<&Blackboard>)>();
    for (entity, reasoner, blackboard) in query.iter(world) {
        rows.push((entity, utility_entry(entity, reasoner, blackboard.is_some())));
    }
    rows.sort_by_key(|(entity, _)| (entity.index_u32(), entity.to_bits()));
    let reasoners: Vec<Value> = rows.into_iter().map(|(_, value)| value).collect();
    Ok(json!({ "reasoners": reasoners, "definitions": definitions }))
}

/// `ai.utility_explain`: what the reasoner would pick now, per consideration, and the report of the
/// last decision it took. Changes nothing.
pub fn utility_explain(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(clock) = world.get_resource::<UtilityClock>() else {
        return Err(unavailable(
            "ai_utility_unavailable",
            "utility AI",
            "UtilityPlugin",
        ));
    };
    let (tick, now) = (clock.tick, clock.now());
    let entity = required_entity(world, params)?;
    let Some(reasoner) = world.get::<UtilityReasoner>(entity) else {
        return Err(missing(
            "not_a_reasoner",
            format!("entity {} has no UtilityReasoner", entity.to_bits()),
        ));
    };
    let Some(blackboard) = world.get::<Blackboard>(entity) else {
        return Err(missing(
            "no_blackboard",
            format!(
                "entity {} has no Blackboard for the considerations to read",
                entity.to_bits()
            ),
        ));
    };
    let Some(report) = reasoner.explain(blackboard, now) else {
        return Err(missing(
            "not_loaded",
            format!(
                "the reasoner of entity {} has no definition yet",
                entity.to_bits()
            ),
        ));
    };
    let to_json = |report: &regia_ai::DecisionReport| -> Result<Value, RpcError> {
        serde_json::to_value(report)
            .map_err(|e| RpcError::new(protocol::INTERNAL_ERROR, "F0001", e.to_string(), "file a bug"))
    };
    Ok(json!({
        "entity": entity.to_bits(),
        "tick": tick,
        "now": now,
        "current_action": reasoner.current_action,
        "explain": to_json(&report)?,
        "last_decision": reasoner.last_report().map(to_json).transpose()?,
    }))
}

const fn cmp_name(cmp: IntCmp) -> &'static str {
    match cmp {
        IntCmp::Eq => "eq",
        IntCmp::Ne => "ne",
        IntCmp::Lt => "lt",
        IntCmp::Le => "le",
        IntCmp::Gt => "gt",
        IntCmp::Ge => "ge",
    }
}

/// The agent's facts and goal under their blackboard names.
fn goap_facts(agent: &GoapAgent) -> (Value, Value) {
    let table = agent.facts();
    let mut facts = Map::new();
    if let Some(state) = agent.last_state() {
        for (index, name) in table.bool_names().iter().enumerate() {
            facts.insert(name.clone(), json!(state.get(index)));
        }
        for (index, name) in table.int_names().iter().enumerate() {
            facts.insert(name.clone(), json!(state.get_int(index)));
        }
    }
    let conditions = agent.goal().bool_conditions();
    let mut wanted = Map::new();
    for (index, name) in table.bool_names().iter().enumerate() {
        if let Some(value) = conditions.get(index) {
            wanted.insert(name.clone(), json!(value));
        }
    }
    let ints: Vec<Value> = agent
        .goal()
        .int_conditions()
        .iter()
        .map(|condition| {
            json!({
                "fact": table.int_names().get(usize::from(condition.fact)),
                "cmp": cmp_name(condition.cmp),
                "value": condition.value,
            })
        })
        .collect();
    (Value::Object(facts), json!({ "facts": wanted, "ints": ints }))
}

fn goap_entry(entity: Entity, agent: &GoapAgent, status: Option<&GoapStatus>) -> Value {
    let (facts, goal) = goap_facts(agent);
    let steps: Vec<Value> = agent.plan().map_or_else(Vec::new, |plan| {
        plan.steps()
            .iter()
            .enumerate()
            .map(|(index, step)| {
                json!({
                    "name": step.name,
                    "cost": step.cost,
                    "done": index < agent.next_step(),
                    "current": index == agent.next_step(),
                })
            })
            .collect()
    });
    json!({
        "entity": entity.to_bits(),
        "state": status.map(|s| s.state.name()),
        "error": status.map(|s| s.error.clone()),
        "plans_started": status.map(|s| s.plans_started),
        "replans": status.map(|s| s.replans),
        "goal": goal,
        "facts": facts,
        "steps": steps,
        "next_step": agent.next_step(),
        "current_action": agent.current_action(),
        "total_cost": agent.plan().map(regia_ai::goap::Plan::total_cost),
        "nodes_expanded": agent.plan().map(regia_ai::goap::Plan::nodes_expanded),
        "budget": { "max_nodes": agent.options.max_nodes, "max_depth": agent.options.max_depth },
        "actions": agent.planner().actions().iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
    })
}

/// `ai.goap_plan`: one agent's plan, facts and goal, or with no `entity` every agent's.
pub fn goap_plan(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(stats) = world.get_resource::<GoapStats>() else {
        return Err(unavailable("ai_goap_unavailable", "GOAP", "GoapPlugin"));
    };
    let summary = json!({
        "tick": stats.tick,
        "plans_started_last_tick": stats.plans_started_last_tick,
        "plans_started_total": stats.plans_started_total,
    });
    if let Some(entity) = entity_param(world, params)? {
        let Some(agent) = world.get::<GoapAgent>(entity) else {
            return Err(missing(
                "not_a_goap_agent",
                format!("entity {} has no GoapAgent", entity.to_bits()),
            ));
        };
        return Ok(json!({
            "stats": summary,
            "agent": goap_entry(entity, agent, world.get::<GoapStatus>(entity)),
        }));
    }
    let mut rows: Vec<(Entity, Value)> = Vec::new();
    let mut query = world.query::<(Entity, &GoapAgent, Option<&GoapStatus>)>();
    for (entity, agent, status) in query.iter(world) {
        rows.push((entity, goap_entry(entity, agent, status)));
    }
    rows.sort_by_key(|(entity, _)| (entity.index_u32(), entity.to_bits()));
    let agents: Vec<Value> = rows.into_iter().map(|(_, value)| value).collect();
    Ok(json!({ "stats": summary, "agents": agents }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use regia_ai::goap::{Action, FactTable, Goal, Planner};
    use regia_ai::goap_ecs::GoapPlugin;
    use regia_ai::perception_system::{Perceivable, PerceptionPlugin, Sense};
    use regia_ai::utility::UtilityAiDef;
    use regia_ai::utility_ecs::UtilityPlugin;
    use regia_core::math::Vec3;
    use regia_scene::Transform;

    use super::*;

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .map_or_else(|| err.message.clone(), str::to_string)
    }

    fn reason(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, PerceptionPlugin, UtilityPlugin, GoapPlugin));
        regia_app::enable_fixed_step(&mut app, 60.0);
        app
    }

    fn steps(app: &mut App, n: usize) {
        for _ in 0..n {
            app.update();
        }
    }

    #[test]
    fn every_method_says_when_its_plugin_is_missing() {
        let mut bare = App::new();
        bare.add_plugins(MinimalPlugins);
        let world = bare.world_mut();
        let ok = json!({ "from": 0, "to": 1, "attitude": "hostile", "entity": 1 });
        type Method = fn(&mut World, &Value) -> Result<Value, RpcError>;
        let methods: [(Method, &str); 7] = [
            (perception_get, "ai_perception_unavailable"),
            (team_knowledge, "ai_perception_unavailable"),
            (senses, "ai_perception_unavailable"),
            (attitude_set, "ai_perception_unavailable"),
            (utility_list, "ai_utility_unavailable"),
            (utility_explain, "ai_utility_unavailable"),
            (goap_plan, "ai_goap_unavailable"),
        ];
        for (method, expected) in methods {
            let err = method(world, &ok).expect_err("no plugin");
            assert_eq!(code(&err), expected);
        }
    }

    #[test]
    fn perception_get_lists_what_an_agent_perceives() {
        let mut app = app();
        {
            let mut settings = app.world_mut().resource_mut::<PerceptionSettings>();
            settings.sight_interval_ticks = 1;
        }
        let agent = app
            .world_mut()
            .spawn((Transform::default(), Perception::default()))
            .id();
        let target = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 0.0, -5.0)),
                Perceivable {
                    team: 1,
                    ..Perceivable::default()
                },
            ))
            .id();
        steps(&mut app, 3);
        let answer = perception_get(app.world_mut(), &json!({ "entity": agent.to_bits() })).expect("answers");
        let one = &answer["agent"];
        assert_eq!(one["entity"], agent.to_bits());
        assert_eq!(one["target"], target.to_bits());
        let stimulus = &one["stimuli"][0];
        assert_eq!(stimulus["entity"], target.to_bits());
        assert_eq!(stimulus["sense"], Sense::Sight.name());
        assert_eq!(stimulus["team"], 1);
        assert_eq!(stimulus["last_known_position"], json!([0.0, 0.0, -5.0]));
        assert_eq!(stimulus["confidence"], 1.0);
        assert_eq!(stimulus["last_known_velocity"], json!([0.0, 0.0, 0.0]));
        assert!(one["meter"].is_array());
        assert!(answer["stats"]["tick"].as_u64().is_some_and(|tick| tick > 0));
        crate::contract::assert_result_named("ai.perception_get", &answer);

        let all = perception_get(app.world_mut(), &json!({})).expect("answers");
        assert_eq!(all["agents"].as_array().map(Vec::len), Some(1));
        assert_eq!(all["agents"][0], *one);

        let err =
            perception_get(app.world_mut(), &json!({ "entity": target.to_bits() })).expect_err("no memory");
        assert_eq!(
            (code(&err).as_str(), reason(&err).as_str()),
            ("F0003", "not_a_perceiving_agent")
        );
        let err = perception_get(app.world_mut(), &json!({ "entity": "x" })).expect_err("bad");
        assert_eq!(reason(&err), "bad_entity");
    }

    #[test]
    fn attitude_set_changes_the_table_and_reports_what_it_was() {
        let mut app = app();
        let answer = attitude_set(
            app.world_mut(),
            &json!({ "from": 1, "to": 2, "attitude": "friendly" }),
        )
        .expect("sets");
        assert_eq!(answer["previous"], "hostile");
        assert_eq!(answer["attitude"], "friendly");
        assert_eq!(answer["reverse"], "hostile", "directional unless asked");
        assert_eq!(app.world().resource::<Attitudes>().get(1, 2), Attitude::Friendly);
        assert_eq!(app.world().resource::<Attitudes>().get(2, 1), Attitude::Hostile);

        let answer = attitude_set(
            app.world_mut(),
            &json!({ "from": 3, "to": 4, "attitude": "neutral", "symmetric": true }),
        )
        .expect("sets");
        assert_eq!(answer["reverse"], "neutral");

        for (params, expected) in [
            (json!({ "to": 1, "attitude": "hostile" }), "bad_team"),
            (json!({ "from": 256, "to": 1, "attitude": "hostile" }), "bad_team"),
            (json!({ "from": -1, "to": 1, "attitude": "hostile" }), "bad_team"),
            (json!({ "from": 0, "to": 1 }), "bad_attitude"),
            (
                json!({ "from": 0, "to": 1, "attitude": "grumpy" }),
                "bad_attitude",
            ),
        ] {
            let err = attitude_set(app.world_mut(), &params).expect_err("refused");
            assert_eq!(
                (code(&err).as_str(), reason(&err).as_str()),
                ("F0002", expected),
                "{params}"
            );
        }
    }

    #[test]
    fn utility_list_and_explain_show_the_scores_of_the_decision() {
        let text = r#"(
            format_version: 1,
            actions: [
                (name: "attack", considerations: [(key: "health", min: 0.0, max: 100.0)]),
                (name: "flee", considerations: [(key: "threat", min: 0.0, max: 100.0)]),
            ],
        )"#;
        let def = UtilityAiDef::from_ron(text).expect("parses");
        let mut app = app();
        let mut blackboard = Blackboard::new();
        blackboard.insert("health", 90.0f32);
        blackboard.insert("threat", 10.0f32);
        let mut reasoner = UtilityReasoner::from_def(&def).expect("valid");
        reasoner.interval_ticks = 1;
        let agent = app.world_mut().spawn((reasoner, blackboard)).id();
        steps(&mut app, 3);

        let list = utility_list(app.world_mut(), &json!({})).expect("answers");
        let row = &list["reasoners"][0];
        assert_eq!(row["entity"], agent.to_bits());
        assert_eq!(row["current_action"], "attack");
        assert_eq!(row["actions"], json!(["attack", "flee"]));
        assert_eq!(row["loaded"], true);

        let answer =
            utility_explain(app.world_mut(), &json!({ "entity": agent.to_bits() })).expect("answers");
        assert_eq!(answer["explain"]["selected"], "attack");
        assert_eq!(
            answer["explain"]["actions"], answer["last_decision"]["actions"],
            "same scores"
        );
        let attack = &answer["explain"]["actions"][0];
        assert_eq!(attack["name"], "attack");
        let consideration = &attack["considerations"][0];
        assert_eq!(consideration["key"], "health");
        assert_eq!(consideration["raw_input"], 90.0);
        assert!(consideration["curve_output"].as_f64().is_some_and(|v| v > 0.89));
    }

    #[test]
    fn utility_explain_refuses_what_it_cannot_score() {
        let mut app = app();
        let plain = app.world_mut().spawn_empty().id();
        let err =
            utility_explain(app.world_mut(), &json!({ "entity": plain.to_bits() })).expect_err("not one");
        assert_eq!(reason(&err), "not_a_reasoner");
        let err = utility_explain(app.world_mut(), &json!({})).expect_err("required");
        assert_eq!(reason(&err), "missing_param");
        let no_board = app
            .world_mut()
            .spawn(UtilityReasoner::from_path("a.utility.ron"))
            .id();
        let err =
            utility_explain(app.world_mut(), &json!({ "entity": no_board.to_bits() })).expect_err("board");
        assert_eq!(reason(&err), "no_blackboard");
        let unloaded = app
            .world_mut()
            .spawn((UtilityReasoner::from_path("a.utility.ron"), Blackboard::new()))
            .id();
        let err =
            utility_explain(app.world_mut(), &json!({ "entity": unloaded.to_bits() })).expect_err("loaded");
        assert_eq!(reason(&err), "not_loaded");
    }

    #[test]
    fn goap_plan_shows_the_plan_the_facts_and_a_replan() {
        let planner = Arc::new(Planner::new(vec![
            Action::new("fetch", 1.0).requires(1, true).sets(0, true),
            Action::new("make", 5.0).sets(0, true),
        ]));
        let mut facts = FactTable::new();
        facts.add_bool("has_item").expect("fact");
        facts.add_bool("item_nearby").expect("fact");
        let mut app = app();
        let mut blackboard = Blackboard::new();
        blackboard.insert("item_nearby", true);
        let agent = app
            .world_mut()
            .spawn((
                GoapAgent::new(planner, facts, Goal::new().require(0, true)),
                blackboard,
            ))
            .id();
        steps(&mut app, 4);
        let answer = goap_plan(app.world_mut(), &json!({ "entity": agent.to_bits() })).expect("answers");
        let one = &answer["agent"];
        assert_eq!(one["state"], "executing");
        assert_eq!(one["steps"][0]["name"], "fetch");
        assert_eq!(one["steps"][0]["current"], true);
        assert_eq!(one["current_action"], "fetch");
        assert_eq!(one["goal"]["facts"]["has_item"], true);
        assert_eq!(one["facts"]["item_nearby"], true);
        assert_eq!(one["replans"], 0);

        app.world_mut()
            .get_mut::<Blackboard>(agent)
            .expect("board")
            .insert("item_nearby", false);
        steps(&mut app, 4);
        let answer = goap_plan(app.world_mut(), &json!({})).expect("answers");
        let one = &answer["agents"][0];
        assert_eq!(one["steps"][0]["name"], "make", "replanned");
        assert_eq!(one["replans"], 1);

        let plain = app.world_mut().spawn_empty().id();
        let err = goap_plan(app.world_mut(), &json!({ "entity": plain.to_bits() })).expect_err("not one");
        assert_eq!(
            (code(&err).as_str(), reason(&err).as_str()),
            ("F0003", "not_a_goap_agent")
        );
    }

    fn scent_strength(_world: &World, _observer: Entity, _candidate: Entity) -> Option<f32> {
        Some(0.4)
    }

    #[test]
    fn team_knowledge_lists_a_shared_target_and_refuses_a_bad_team() {
        let mut app = app();
        {
            let mut settings = app.world_mut().resource_mut::<PerceptionSettings>();
            settings.sight_interval_ticks = 1;
        }
        let _agent = app
            .world_mut()
            .spawn((
                Transform::default(),
                Perception {
                    share_knowledge: true,
                    ..Perception::default()
                },
            ))
            .id();
        let target = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 0.0, -5.0)),
                Perceivable {
                    team: 1,
                    ..Perceivable::default()
                },
            ))
            .id();
        steps(&mut app, 3);
        let answer = team_knowledge(app.world_mut(), &json!({ "team": 0 })).expect("facts");
        let facts = answer["facts"].as_array().expect("list");
        assert!(facts.iter().any(|fact| fact["entity"] == json!(target.to_bits())));
        crate::contract::assert_result_named("ai.team_knowledge", &answer);

        let empty = team_knowledge(app.world_mut(), &json!({ "team": 4 })).expect("empty team");
        assert_eq!(empty["facts"].as_array().map(Vec::len), Some(0));

        for params in [
            json!({}),
            json!({ "team": "scouts" }),
            json!({ "team": 256 }),
            json!({ "team": -1 }),
        ] {
            let err = team_knowledge(app.world_mut(), &params).expect_err("refused");
            assert_eq!(code(&err), "unknown_team", "{params}");
            crate::contract::assert_listed("ai.team_knowledge", "unknown_team");
        }
    }

    #[test]
    fn senses_lists_the_registry_and_refuses_an_unknown_name() {
        let mut app = app();
        let answer = senses(app.world_mut(), &json!({})).expect("empty registry");
        assert_eq!(answer["senses"].as_array().map(Vec::len), Some(0));
        crate::contract::assert_result_named("ai.senses", &answer);

        app.world_mut()
            .resource_mut::<PerceptionSenses>()
            .register("scent", 12.0, 2, scent_strength)
            .expect("registers");
        let answer = senses(app.world_mut(), &json!({})).expect("listed");
        assert_eq!(answer["senses"][0]["name"], "scent");
        assert_eq!(answer["senses"][0]["id"], 1);
        assert_eq!(answer["senses"][0]["range"], 12.0);
        assert_eq!(answer["senses"][0]["interval_ticks"], 2);
        crate::contract::assert_result_named("ai.senses", &answer);

        let one = senses(app.world_mut(), &json!({ "name": "scent" })).expect("one");
        assert_eq!(one["senses"].as_array().map(Vec::len), Some(1));

        for params in [json!({ "name": "nope" }), json!({ "name": 1 })] {
            let err = senses(app.world_mut(), &params).expect_err("refused");
            assert_eq!(code(&err), "unknown_sense", "{params}");
            crate::contract::assert_listed("ai.senses", "unknown_sense");
        }
    }
}
