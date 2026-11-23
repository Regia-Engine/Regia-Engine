// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for squads, attack tokens, formations, the AI tick budget and custom query
//! implementations (ADR-2709, `book/agents/ai-squads.md`): `ai.squads`, `ai.squad_set`,
//! `ai.tick_budget_get`, `ai.tick_budget_set` and `ai.query_impls`.
//!
//! Each answers `ai_squads_unavailable`, `ai_tick_budget_unavailable` or `ai_query_unavailable` when its
//! plugin is not installed, `F0002` with `data.reason` for a bad parameter and `F0003` with `data.reason`
//! for an entity that is not there or is not that kind of thing.

use regia_ai::attack_tokens::{AiAttackTokens, set_token_capacity};
use regia_ai::blackboard::{BbValue, Blackboard};
use regia_ai::formation::{AiFormation, FormationAnchor, FormationShape, resolve_formation};
use regia_ai::formation_asset::{FormationAsset, FormationFile};
use regia_ai::spatial_query::SpatialQueryManager;
use regia_ai::squad::{
    AiDead, AiSquad, AiSquadClock, AiSquadMember, SquadError, claim_squad_role, release_squad_role,
    set_squad_leader, set_squad_role,
};
use regia_ai::state_tree::StateTreeReloads;
use regia_ai::tick_budget::{AiTickBudget, AiTickStats, AiTickTier, ai_tick_agent_info};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_nav::NavCrowd;
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
            "see book/agents/ai-squads.md for the parameters",
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
            "ai.squads lists the squads and their members",
        ),
        reason,
    )
}

fn squads_unavailable() -> RpcError {
    RpcError::new(
        protocol::UNSUPPORTED,
        "ai_squads_unavailable",
        "squads are not installed in this app",
        "add regia_ai::AiSquadPlugin",
    )
}

fn budget_unavailable() -> RpcError {
    RpcError::new(
        protocol::UNSUPPORTED,
        "ai_tick_budget_unavailable",
        "the AI tree systems are not installed in this app",
        "add regia_ai::AiPlugin",
    )
}

/// A required or optional value that must be an entity id naming a live entity.
fn live_entity(world: &World, value: &Value, what: &str) -> Result<Entity, RpcError> {
    let entity = value
        .as_u64()
        .and_then(Entity::try_from_bits)
        .ok_or_else(|| invalid("bad_entity", format!("`{what}` must be an entity id")))?;
    if world.get_entity(entity).is_err() {
        return Err(missing(
            "entity_not_found",
            format!("`{what}`: entity {} does not exist", entity.to_bits()),
        ));
    }
    Ok(entity)
}

fn u32_field(value: &Value, reason: &str, what: &str) -> Result<u32, RpcError> {
    value.as_u64().and_then(|v| u32::try_from(v).ok()).ok_or_else(|| {
        invalid(
            reason,
            format!("`{what}` must be a whole number from 0 to {}", u32::MAX),
        )
    })
}

fn positive(value: Option<&Value>, reason: &str, what: &str) -> Result<f32, RpcError> {
    value
        .and_then(Value::as_f64)
        .map(|v| v as f32)
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or_else(|| invalid(reason, format!("`{what}` must be a number above 0")))
}

fn vec3_json(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

fn vec3_param(value: &Value) -> Option<Vec3> {
    let items = value.as_array()?;
    let [x, y, z] = items.as_slice() else {
        return None;
    };
    let v = Vec3::new(x.as_f64()? as f32, y.as_f64()? as f32, z.as_f64()? as f32);
    v.is_finite().then_some(v)
}

fn bb_json(value: &BbValue) -> Value {
    match value {
        BbValue::Bool(v) => json!(v),
        BbValue::Int(v) => json!(v),
        BbValue::Float(v) => json!(v),
        BbValue::String(v) => json!(v),
        BbValue::Vec3(v) => vec3_json(*v),
        BbValue::Entity(bits) => json!({ "entity": bits }),
    }
}

/// A blackboard value from JSON: a bool, whole number, other number, string, `[x, y, z]` or
/// `{ "entity": id }`.
fn bb_param(value: &Value) -> Option<BbValue> {
    match value {
        Value::Bool(v) => Some(BbValue::Bool(*v)),
        Value::Number(n) => match n.as_i64() {
            Some(v) => Some(BbValue::Int(v)),
            None => n
                .as_f64()
                .map(|v| v as f32)
                .filter(|v| v.is_finite())
                .map(BbValue::Float),
        },
        Value::String(v) => Some(BbValue::String(v.clone())),
        Value::Array(_) => vec3_param(value).map(BbValue::Vec3),
        Value::Object(map) if map.len() == 1 => map
            .get("entity")?
            .as_u64()
            .and_then(Entity::try_from_bits)
            .map(|entity| BbValue::Entity(entity.to_bits())),
        _ => None,
    }
}

fn formation_json(formation: &AiFormation) -> Value {
    let shape = match formation.shape {
        FormationShape::Ring { radius, spacing } => {
            json!({ "kind": "ring", "radius": radius, "spacing": spacing })
        }
        FormationShape::Line { spacing, row_length } => {
            json!({ "kind": "line", "spacing": spacing, "row_length": row_length })
        }
        FormationShape::Wedge { spacing } => json!({ "kind": "wedge", "spacing": spacing }),
    };
    let anchor = match formation.anchor {
        FormationAnchor::Leader => json!("leader"),
        FormationAnchor::Entity(entity) => json!({ "entity": entity.to_bits() }),
    };
    json!({
        "anchor": anchor,
        "shape": shape,
        "interval_steps": formation.interval_steps,
        "extents": vec3_json(formation.extents),
        "min_separation": formation.min_separation,
        "require_path": formation.require_path,
        "slot_key": formation.slot_key,
        "slots": formation.slots.iter().map(|slot| json!({
            "index": slot.index,
            "position": vec3_json(slot.position),
            "member": slot.member.to_bits(),
        })).collect::<Vec<_>>(),
        "unplaced": formation.unplaced.iter().map(|e| e.to_bits()).collect::<Vec<_>>(),
    })
}

fn squad_json(world: &World, entity: Entity, squad: &AiSquad) -> Value {
    let blackboard: Map<String, Value> = world
        .get::<Blackboard>(entity)
        .map(|board| {
            board
                .values
                .iter()
                .map(|(key, value)| (key.clone(), bb_json(value)))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "entity": entity.to_bits(),
        "name": squad.name,
        "leader": squad.leader.map(Entity::to_bits),
        "elections": squad.elections,
        "members": squad.members.iter().map(|member| {
            let data = world.get::<AiSquadMember>(*member);
            json!({
                "entity": member.to_bits(),
                "rank": data.map(|d| d.rank),
                "role": data.and_then(|d| d.role.clone()),
            })
        }).collect::<Vec<_>>(),
        "roles": squad.roles.iter().map(|role| json!({
            "name": role.name,
            "capacity": role.capacity,
            "auto_fill": role.auto_fill,
            "holders": role.holders.iter().map(|e| e.to_bits()).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "blackboard": blackboard,
        "formation": world.get::<AiFormation>(entity).map(formation_json),
    })
}

fn pool_json(entity: Entity, pool: &AiAttackTokens) -> Value {
    json!({
        "target": entity.to_bits(),
        "capacity": pool.capacity,
        "preempt": pool.preempt,
        "waiter_ttl_steps": pool.waiter_ttl_steps,
        "holders": pool.holders.iter().map(|hold| json!({
            "entity": hold.holder.to_bits(),
            "priority": hold.priority,
        })).collect::<Vec<_>>(),
        "waiters": pool.waiters.iter().map(|waiter| json!({
            "entity": waiter.agent.to_bits(),
            "priority": waiter.priority,
            "last_request_step": waiter.last_request_step,
        })).collect::<Vec<_>>(),
        "requests": pool.requests,
        "granted": pool.granted,
        "denied": pool.denied,
        "preempted": pool.preempted,
    })
}

fn sorted(mut rows: Vec<(Entity, Value)>) -> Vec<Value> {
    rows.sort_by_key(|(entity, _)| (entity.index_u32(), entity.to_bits()));
    rows.into_iter().map(|(_, value)| value).collect()
}

/// `ai.squads`: one squad, or every squad and every attack-token pool, in entity order.
pub fn squads(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(step) = world.get_resource::<AiSquadClock>().map(|clock| clock.step) else {
        return Err(squads_unavailable());
    };
    if let Some(value) = params.get("squad").filter(|v| !v.is_null()) {
        let entity = live_entity(world, value, "squad")?;
        let Some(squad) = world.get::<AiSquad>(entity) else {
            return Err(missing(
                "not_a_squad",
                format!("entity {} has no AiSquad", entity.to_bits()),
            ));
        };
        return Ok(json!({ "step": step, "squad": squad_json(world, entity, squad) }));
    }
    let squads: Vec<(Entity, AiSquad)> = world
        .query::<(Entity, &AiSquad)>()
        .iter(world)
        .map(|(entity, squad)| (entity, squad.clone()))
        .collect();
    let rows = squads
        .iter()
        .map(|(entity, squad)| (*entity, squad_json(world, *entity, squad)))
        .collect();
    let pools = world
        .query::<(Entity, &AiAttackTokens)>()
        .iter(world)
        .map(|(entity, pool)| (entity, pool_json(entity, pool)))
        .collect();
    Ok(json!({ "step": step, "squads": sorted(rows), "token_pools": sorted(pools) }))
}

/// A role row of `ai.squad_set`.
fn role_param(value: &Value) -> Result<(String, u32, bool), RpcError> {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| invalid("bad_role", "each entry of `roles` needs a non-empty `name`"))?;
    let capacity = u32_field(
        value.get("capacity").unwrap_or(&Value::Null),
        "bad_role",
        "roles[].capacity",
    )?;
    let auto_fill = match value.get("auto_fill") {
        None | Some(Value::Null) => true,
        Some(flag) => flag
            .as_bool()
            .ok_or_else(|| invalid("bad_role", "`roles[].auto_fill` must be a boolean"))?,
    };
    Ok((name.to_string(), capacity, auto_fill))
}

/// The `formation` parameter, over the formation in place or the default.
fn formation_param(
    world: &World,
    value: &Value,
    current: Option<&AiFormation>,
) -> Result<AiFormation, RpcError> {
    let bad = |message: &str| invalid("bad_formation", message.to_string());
    if !value.is_object() {
        return Err(bad("`formation` must be an object or null"));
    }
    let mut formation = current.cloned().unwrap_or_default();
    match value.get("anchor") {
        None => {}
        Some(Value::String(text)) if text == "leader" => formation.anchor = FormationAnchor::Leader,
        Some(anchor) => {
            let entity = anchor
                .get("entity")
                .ok_or_else(|| bad("`formation.anchor` must be \"leader\" or { \"entity\": id }"))?;
            formation.anchor =
                FormationAnchor::Entity(live_entity(world, entity, "formation.anchor.entity")?);
        }
    }
    if let Some(shape) = value.get("shape") {
        let kind = crate::params::opt_str(shape, "kind")?.unwrap_or_default();
        let spacing = shape.get("spacing");
        formation.shape = match kind {
            "ring" => FormationShape::Ring {
                radius: positive(shape.get("radius"), "bad_formation", "formation.shape.radius")?,
                spacing: positive(spacing, "bad_formation", "formation.shape.spacing")?,
            },
            "line" => FormationShape::Line {
                spacing: positive(spacing, "bad_formation", "formation.shape.spacing")?,
                row_length: u32_field(
                    shape.get("row_length").unwrap_or(&Value::Null),
                    "bad_formation",
                    "formation.shape.row_length",
                )?
                .max(1),
            },
            "wedge" => FormationShape::Wedge {
                spacing: positive(spacing, "bad_formation", "formation.shape.spacing")?,
            },
            _ => return Err(bad("`formation.shape.kind` must be ring, line or wedge")),
        };
    }
    if let Some(interval) = value.get("interval_steps") {
        formation.interval_steps = u32_field(interval, "bad_formation", "formation.interval_steps")?;
    }
    if let Some(separation) = value.get("min_separation") {
        formation.min_separation = positive(Some(separation), "bad_formation", "formation.min_separation")?;
    }
    if let Some(flag) = value.get("require_path") {
        formation.require_path = flag
            .as_bool()
            .ok_or_else(|| bad("`formation.require_path` must be a boolean"))?;
    }
    if let Some(key) = value.get("slot_key") {
        formation.slot_key = key
            .as_str()
            .filter(|key| !key.is_empty())
            .ok_or_else(|| bad("`formation.slot_key` must be a non-empty string"))?
            .to_string();
    }
    if let Some(extents) = value.get("extents") {
        formation.extents = vec3_param(extents)
            .filter(|v| v.min_element() > 0.0)
            .ok_or_else(|| bad("`formation.extents` must be [x, y, z] with every value above 0"))?;
    }
    formation.dirty = true;
    Ok(formation)
}

/// The checked `token_pool` parameter.
struct PoolParams {
    target: Entity,
    capacity: Option<u32>,
    preempt: Option<bool>,
    ttl: Option<u32>,
}

/// Checks the `token_pool` parameter; nothing changes.
fn token_pool_param(world: &World, value: &Value) -> Result<PoolParams, RpcError> {
    let bad = |message: &str| invalid("bad_tokens", message.to_string());
    if !value.is_object() {
        return Err(bad("`token_pool` must be an object"));
    }
    let target_value = value
        .get("target")
        .ok_or_else(|| bad("`token_pool.target` is required"))?;
    let target = live_entity(world, target_value, "token_pool.target")?;
    let capacity = value
        .get("capacity")
        .map(|v| u32_field(v, "bad_tokens", "token_pool.capacity"))
        .transpose()?;
    let preempt = value
        .get("preempt")
        .map(|v| {
            v.as_bool()
                .ok_or_else(|| bad("`token_pool.preempt` must be a boolean"))
        })
        .transpose()?;
    let ttl = value
        .get("waiter_ttl_steps")
        .map(|v| u32_field(v, "bad_tokens", "token_pool.waiter_ttl_steps"))
        .transpose()?;
    if capacity.is_none() && world.get::<AiAttackTokens>(target).is_none() {
        return Err(bad("a new `token_pool` needs `capacity`"));
    }
    Ok(PoolParams {
        target,
        capacity,
        preempt,
        ttl,
    })
}

/// Applies a checked `token_pool` parameter and returns the pool's row.
fn apply_token_pool(world: &mut World, params: PoolParams) -> Result<Value, RpcError> {
    let PoolParams {
        target,
        capacity,
        preempt,
        ttl,
    } = params;
    match capacity {
        Some(capacity) if world.get::<AiAttackTokens>(target).is_some() => {
            set_token_capacity(world, target, capacity);
        }
        Some(capacity) => {
            world.entity_mut(target).insert(AiAttackTokens::new(capacity));
        }
        None => {}
    }
    let mut pool = world
        .get_mut::<AiAttackTokens>(target)
        .ok_or_else(|| missing("not_a_pool", "the token pool went away"))?;
    if let Some(preempt) = preempt {
        pool.preempt = preempt;
    }
    if let Some(ttl) = ttl {
        pool.waiter_ttl_steps = ttl;
    }
    Ok(pool_json(target, &pool))
}

fn squad_error(error: SquadError) -> RpcError {
    match error {
        SquadError::RoleFull => invalid(error.reason(), error.to_string()),
        _ => missing(error.reason(), error.to_string()),
    }
}

/// `ai.squad_set`: creates or edits a squad and sets up attack-token pools. Parameters are checked before
/// anything changes, except `role_full`, which is only known once the roster has changed.
pub fn squad_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !world.contains_resource::<AiSquadClock>() {
        return Err(squads_unavailable());
    }
    if !params.is_object() {
        return Err(invalid("bad_params", "the parameters must be an object"));
    }
    let pool_param = params
        .get("token_pool")
        .filter(|v| !v.is_null())
        .map(|value| token_pool_param(world, value))
        .transpose()?;
    let create = match params.get("create") {
        None | Some(Value::Null) => false,
        Some(flag) => flag
            .as_bool()
            .ok_or_else(|| invalid("bad_create", "`create` must be a boolean"))?,
    };
    let name = match params.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err(invalid("bad_name", "`name` must be a string")),
    };
    let squad_value = params.get("squad").filter(|v| !v.is_null());
    if create && squad_value.is_some() {
        return Err(invalid("bad_create", "pass `create` or `squad`, not both"));
    }
    if squad_value.is_none() && !create {
        let squad_fields = [
            "name",
            "roles",
            "add",
            "remove",
            "leader",
            "role",
            "blackboard",
            "formation",
        ];
        if let Some(pool) = pool_param
            && !squad_fields.iter().any(|field| params.get(*field).is_some())
        {
            let pool = apply_token_pool(world, pool)?;
            return Ok(json!({ "token_pool": pool }));
        }
        return Err(invalid(
            "missing_param",
            "`squad` is required (or `create: true`)",
        ));
    }
    let existing = match squad_value {
        Some(value) => {
            let entity = live_entity(world, value, "squad")?;
            if world.get::<AiSquad>(entity).is_none() {
                return Err(missing(
                    "not_a_squad",
                    format!("entity {} has no AiSquad", entity.to_bits()),
                ));
            }
            Some(entity)
        }
        None => None,
    };

    let mut roles = Vec::new();
    if let Some(value) = params.get("roles").filter(|v| !v.is_null()) {
        let rows = value
            .as_array()
            .ok_or_else(|| invalid("bad_role", "`roles` must be an array"))?;
        for row in rows {
            roles.push(role_param(row)?);
        }
    }
    let mut add: Vec<(Entity, Option<i32>)> = Vec::new();
    if let Some(value) = params.get("add").filter(|v| !v.is_null()) {
        let rows = value
            .as_array()
            .ok_or_else(|| invalid("bad_entity", "`add` must be an array"))?;
        for row in rows {
            let (entity, rank) = match row {
                Value::Object(_) => (
                    row.get("entity").unwrap_or(&Value::Null),
                    row.get("rank").unwrap_or(&Value::Null),
                ),
                _ => (row, &Value::Null),
            };
            let rank = match rank {
                Value::Null => None,
                value => Some(
                    value
                        .as_i64()
                        .and_then(|v| i32::try_from(v).ok())
                        .ok_or_else(|| invalid("bad_rank", "`add[].rank` must be a whole number"))?,
                ),
            };
            add.push((live_entity(world, entity, "add[].entity")?, rank));
        }
    }
    let mut remove: Vec<Entity> = Vec::new();
    if let Some(value) = params.get("remove").filter(|v| !v.is_null()) {
        let rows = value
            .as_array()
            .ok_or_else(|| invalid("bad_entity", "`remove` must be an array"))?;
        for row in rows {
            let entity = row
                .as_u64()
                .and_then(Entity::try_from_bits)
                .ok_or_else(|| invalid("bad_entity", "`remove` must hold entity ids"))?;
            if world.get_entity(entity).is_err() {
                return Err(missing(
                    "entity_not_found",
                    format!("`remove`: entity {} does not exist", entity.to_bits()),
                ));
            }
            let member_here = existing.is_some_and(|squad| {
                world
                    .get::<AiSquad>(squad)
                    .is_some_and(|s| s.members.contains(&entity))
            });
            if !member_here {
                return Err(missing(
                    "not_a_member",
                    format!("entity {} is not a member of the squad", entity.to_bits()),
                ));
            }
            remove.push(entity);
        }
    }
    let mut blackboard: Vec<(String, Option<BbValue>)> = Vec::new();
    if let Some(value) = params.get("blackboard").filter(|v| !v.is_null()) {
        let rows = value
            .as_object()
            .ok_or_else(|| invalid("bad_blackboard", "`blackboard` must be an object"))?;
        for (key, value) in rows {
            let parsed = if value.is_null() {
                None
            } else {
                Some(bb_param(value).ok_or_else(|| {
                    invalid(
                        "bad_blackboard",
                        format!(
                            "`blackboard.{key}` must be a bool, number, string, [x, y, z], {{ \"entity\": id }} or null"
                        ),
                    )
                })?)
            };
            blackboard.push((key.clone(), parsed));
        }
    }
    let current_formation = existing.and_then(|squad| world.get::<AiFormation>(squad));
    let formation = match params.get("formation") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(value) => Some(Some(formation_param(world, value, current_formation)?)),
    };
    // The leader and the role must be members once `add` and `remove` have run, and the role must exist
    // once `roles` has run.
    let will_be_member = |entity: Entity, world: &World| {
        world.get::<AiDead>(entity).is_none()
            && (add.iter().any(|(e, _)| *e == entity)
                || (!remove.contains(&entity)
                    && existing.is_some_and(|squad| {
                        world
                            .get::<AiSquad>(squad)
                            .is_some_and(|s| s.members.contains(&entity))
                    })))
    };
    let leader = match params.get("leader").filter(|v| !v.is_null()) {
        Some(value) => {
            let entity = live_entity(world, value, "leader")?;
            if !will_be_member(entity, world) {
                return Err(missing(
                    "not_a_member",
                    format!("leader {} is not a member of the squad", entity.to_bits()),
                ));
            }
            Some(entity)
        }
        None => None,
    };
    let role = match params.get("role").filter(|v| !v.is_null()) {
        Some(value) => {
            let member = value
                .get("member")
                .ok_or_else(|| invalid("bad_role", "`role.member` is required"))?;
            let member = live_entity(world, member, "role.member")?;
            if !will_be_member(member, world) {
                return Err(missing(
                    "not_a_member",
                    format!("role.member {} is not a member of the squad", member.to_bits()),
                ));
            }
            let name = match value.get("name") {
                None | Some(Value::Null) => None,
                Some(Value::String(text)) => Some(text.clone()),
                Some(_) => return Err(invalid("bad_role", "`role.name` must be a string or null")),
            };
            if let Some(name) = &name {
                let known = roles.iter().any(|(n, _, _)| n == name)
                    || existing.is_some_and(|squad| {
                        world
                            .get::<AiSquad>(squad)
                            .is_some_and(|s| s.roles.iter().any(|r| &r.name == name))
                    });
                if !known {
                    return Err(missing("unknown_role", format!("the squad has no role `{name}`")));
                }
            }
            Some((member, name))
        }
        None => None,
    };

    // Apply.
    let squad = match existing {
        Some(entity) => entity,
        None => world.spawn(AiSquad::default()).id(),
    };
    if let Some(name) = name
        && let Some(mut data) = world.get_mut::<AiSquad>(squad)
    {
        data.name = name;
    }
    for (name, capacity, auto_fill) in roles {
        set_squad_role(world, squad, &name, capacity, auto_fill).map_err(squad_error)?;
    }
    for entity in remove {
        world.entity_mut(entity).remove::<AiSquadMember>();
    }
    for (entity, rank) in add {
        // Already here: a re-insert would be leaving and rejoining, so only the rank changes.
        let member_here = world
            .get::<AiSquad>(squad)
            .is_some_and(|s| s.members.contains(&entity));
        if member_here {
            if let Some(rank) = rank
                && let Some(mut data) = world.get_mut::<AiSquadMember>(entity)
            {
                data.rank = rank;
            }
        } else {
            world
                .entity_mut(entity)
                .insert(AiSquadMember::new(squad).with_rank(rank.unwrap_or(0)));
        }
    }
    if let Some(leader) = leader {
        set_squad_leader(world, squad, leader).map_err(squad_error)?;
    }
    if let Some((member, name)) = role {
        match name {
            Some(name) => claim_squad_role(world, member, &name),
            None => release_squad_role(world, member),
        }
        .map_err(squad_error)?;
    }
    if !blackboard.is_empty()
        && let Some(mut board) = world.get_mut::<Blackboard>(squad)
    {
        for (key, value) in blackboard {
            match value {
                Some(value) => {
                    board.insert(key, value);
                }
                None => {
                    board.remove(&key);
                }
            }
        }
    }
    match formation {
        None => {}
        Some(None) => {
            world.entity_mut(squad).remove::<AiFormation>();
        }
        Some(Some(formation)) => {
            world.entity_mut(squad).insert(formation);
        }
    }
    let pool = match pool_param {
        Some(pool) => Some(apply_token_pool(world, pool)?),
        None => None,
    };
    let row = world
        .get::<AiSquad>(squad)
        .map(|data| squad_json(world, squad, data))
        .unwrap_or(Value::Null);
    Ok(json!({ "squad": row, "token_pool": pool }))
}

fn budget_json(budget: &AiTickBudget) -> Value {
    json!({
        "enabled": budget.enabled,
        "max_trees_per_step": budget.max_trees_per_step,
        "focus": budget.focus.map(Entity::to_bits),
        "tiers": budget.tiers.iter().map(|tier| json!({
            "max_distance": tier.max_distance,
            "cadence_steps": tier.cadence_steps,
            "max_trees_per_step": tier.max_trees_per_step,
        })).collect::<Vec<_>>(),
    })
}

fn stats_json(stats: Option<&AiTickStats>) -> Value {
    let default = AiTickStats::default();
    let stats = stats.unwrap_or(&default);
    json!({
        "steps": stats.steps,
        "trees": stats.trees,
        "ticks_last_step": stats.ticks_last_step,
        "deferred_last_step": stats.deferred_last_step,
        "ticks_total": stats.ticks_total,
        "late_ticks_total": stats.late_ticks_total,
        "max_late_steps": stats.max_late_steps,
        "tier_trees": stats.tier_trees,
        "tier_ticks": stats.tier_ticks,
    })
}

/// `ai.tick_budget_get`: the settings (the default ones, off, when no budget was set), the counters and,
/// with `entity`, what the budget thinks of that agent.
pub fn tick_budget_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !world.contains_resource::<StateTreeReloads>() {
        return Err(budget_unavailable());
    }
    let agent = match params.get("entity").filter(|v| !v.is_null()) {
        Some(value) => {
            let entity = live_entity(world, value, "entity")?;
            let info = ai_tick_agent_info(world, entity);
            Some(json!({
                "entity": entity.to_bits(),
                "tier": info.as_ref().map(|i| i.tier),
                "distance": info.as_ref().and_then(|i| i.distance),
                "always_on": info.as_ref().map(|i| i.always_on),
                "behavior_tree_age": info.as_ref().and_then(|i| i.behavior_tree_age),
                "state_tree_age": info.as_ref().and_then(|i| i.state_tree_age),
            }))
        }
        None => None,
    };
    let default = AiTickBudget::default();
    let budget = world.get_resource::<AiTickBudget>().unwrap_or(&default);
    Ok(json!({
        "budget": budget_json(budget),
        "stats": stats_json(world.get_resource::<AiTickStats>()),
        "agent": agent,
    }))
}

/// The `tiers` parameter of `ai.tick_budget_set`.
fn tiers_param(value: &Value) -> Result<Vec<AiTickTier>, RpcError> {
    let rows = value
        .as_array()
        .filter(|rows| !rows.is_empty())
        .ok_or_else(|| invalid("bad_tiers", "`tiers` must be a non-empty array"))?;
    let mut tiers: Vec<AiTickTier> = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        if !row.is_object() {
            return Err(invalid(
                "bad_tiers",
                format!("`tiers[{index}]` must be an object"),
            ));
        }
        let last = index + 1 == rows.len();
        let max_distance = match row.get("max_distance").filter(|v| !v.is_null()) {
            Some(value) => positive(Some(value), "bad_tiers", "tiers[].max_distance")?,
            None if last => f32::MAX,
            None => {
                return Err(invalid(
                    "bad_tiers",
                    "only the last tier may leave out `max_distance`",
                ));
            }
        };
        if tiers
            .last()
            .is_some_and(|before| before.max_distance >= max_distance)
        {
            return Err(invalid("bad_tiers", "`max_distance` must ascend, tier by tier"));
        }
        let cadence_steps = u32_field(
            row.get("cadence_steps").unwrap_or(&Value::Null),
            "bad_tiers",
            "tiers[].cadence_steps",
        )?;
        if cadence_steps == 0 {
            return Err(invalid("bad_tiers", "`tiers[].cadence_steps` must be at least 1"));
        }
        let max_trees_per_step = match row.get("max_trees_per_step") {
            None => 0,
            Some(value) => u32_field(value, "bad_tiers", "tiers[].max_trees_per_step")?,
        };
        tiers.push(AiTickTier {
            max_distance,
            cadence_steps,
            max_trees_per_step,
        });
    }
    Ok(tiers)
}

/// `ai.tick_budget_set`: changes the fields named; the others keep their values.
pub fn tick_budget_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !world.contains_resource::<StateTreeReloads>() {
        return Err(budget_unavailable());
    }
    if !params.is_object() {
        return Err(invalid("bad_params", "the parameters must be an object"));
    }
    let enabled = params
        .get("enabled")
        .map(|v| {
            v.as_bool()
                .ok_or_else(|| invalid("bad_enabled", "`enabled` must be a boolean"))
        })
        .transpose()?;
    let max_trees = params
        .get("max_trees_per_step")
        .map(|v| u32_field(v, "bad_max_trees", "max_trees_per_step"))
        .transpose()?;
    let tiers = params.get("tiers").map(tiers_param).transpose()?;
    let focus = match params.get("focus") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(value) => Some(Some(live_entity(world, value, "focus").map_err(|error| {
            if error.data.as_ref().and_then(|d| d.get("reason")) == Some(&json!("bad_entity")) {
                invalid("bad_focus", "`focus` must be an entity id or null")
            } else {
                error
            }
        })?)),
    };
    if enabled.is_none() && max_trees.is_none() && tiers.is_none() && focus.is_none() {
        return Err(invalid(
            "nothing_to_set",
            "name at least one of enabled, max_trees_per_step, tiers, focus",
        ));
    }
    let mut budget = world.get_resource_or_insert_with(AiTickBudget::default);
    if let Some(enabled) = enabled {
        budget.enabled = enabled;
    }
    if let Some(max_trees) = max_trees {
        budget.max_trees_per_step = max_trees;
    }
    if let Some(tiers) = tiers {
        budget.tiers = tiers;
    }
    if let Some(focus) = focus {
        budget.focus = focus;
    }
    Ok(json!({ "budget": budget_json(&budget) }))
}

/// `ai.query_impls`: the Rust generators, tests, anchor providers and tag matcher a query may name.
pub fn query_impls(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(manager) = world.get_resource::<SpatialQueryManager>() else {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "ai_query_unavailable",
            "spatial queries are not installed in this app",
            "add regia_ai::SpatialQueryPlugin",
        ));
    };
    Ok(json!({
        "generators": manager.generator_names(),
        "tests": manager.test_names(),
        "providers": manager.provider_names(),
        "matcher": manager.tag_matcher_name(),
    }))
}

fn formation_code(code: &'static str, message: impl Into<String>) -> RpcError {
    let protocol_code = if code == "unknown_squad" {
        protocol::NOT_FOUND
    } else {
        protocol::INVALID_PARAMS
    };
    RpcError::new(protocol_code, code, message, "see book/agents/ai-squads.md")
}

fn require_squad(world: &World, params: &Value) -> Result<Entity, RpcError> {
    if world.get_resource::<AiSquadClock>().is_none() {
        return Err(squads_unavailable());
    }
    let Some(value) = params.get("squad") else {
        return Err(formation_code("unknown_squad", "`squad` is required"));
    };
    let Some(bits) = value.as_u64() else {
        return Err(invalid("bad_entity", "`squad` must be an entity id"));
    };
    let Some(entity) = Entity::try_from_bits(bits) else {
        return Err(formation_code("unknown_squad", "no squad has that id"));
    };
    if world.get::<AiSquad>(entity).is_none() {
        return Err(formation_code(
            "unknown_squad",
            format!("entity {} is not a squad", entity.to_bits()),
        ));
    }
    Ok(entity)
}

fn shape_label(shape: FormationShape) -> &'static str {
    match shape {
        FormationShape::Ring { .. } => "ring",
        FormationShape::Line { .. } => "line",
        FormationShape::Wedge { .. } => "wedge",
    }
}

fn preset_shape(name: &str) -> Option<FormationShape> {
    match name {
        "ring" => Some(FormationShape::Ring {
            radius: 3.0,
            spacing: 2.0,
        }),
        "line" => Some(FormationShape::Line {
            spacing: 2.0,
            row_length: 5,
        }),
        "wedge" => Some(FormationShape::Wedge { spacing: 2.0 }),
        _ => None,
    }
}

fn formation_report(world: &World, squad: Entity) -> Result<Value, RpcError> {
    let Some(formation) = world.get::<AiFormation>(squad) else {
        return Err(formation_code("unknown_squad", "the squad has no formation"));
    };
    let slots: Vec<Value> = formation
        .slots
        .iter()
        .map(|slot| {
            json!({
                "index": slot.index,
                "position": vec3_json(slot.position),
                "member": slot.member.to_bits(),
            })
        })
        .collect();
    let assignments: Vec<Value> = formation
        .slots
        .iter()
        .map(|slot| json!({ "member": slot.member.to_bits(), "slot": slot.index }))
        .collect();
    let unplaced: Vec<u64> = formation.unplaced.iter().map(|entity| entity.to_bits()).collect();
    let crowd = world.get_resource::<NavCrowd>().map(|crowd| {
        crowd
            .agents()
            .iter()
            .filter(|agent| agent.group().is_some())
            .map(|agent| {
                let group = agent.group();
                json!({
                    "id": agent.id().0,
                    "leader": group.map(|group| group.leader.0),
                    "column": agent.column(),
                    "slot_target": agent.slot_target().map(vec3_json),
                    "offset": group.map(|group| vec3_json(group.offset)),
                })
            })
            .collect::<Vec<_>>()
    });
    Ok(json!({
        "squad": squad.to_bits(),
        "shape": shape_label(formation.shape),
        "name": formation.custom.as_ref().map(|asset| asset.name.clone()),
        "asset": formation.asset_path.clone(),
        "column": formation.column,
        "slots": slots,
        "unplaced": unplaced,
        "assignments": assignments,
        "crowd": crowd,
    }))
}

/// `ai.formation_get`: the squad's shape, asset, slots and any crowd followers.
pub fn formation_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let squad = require_squad(world, params)?;
    formation_report(world, squad)
}

/// `ai.formation_set`: a built-in shape name, or a `.formation.ron` file.
pub fn formation_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let squad = require_squad(world, params)?;
    let shape = params.get("shape").filter(|value| !value.is_null());
    let asset = params.get("asset").filter(|value| !value.is_null());
    match (shape, asset) {
        (Some(_), Some(_)) => {
            return Err(invalid("shape_and_asset", "pass `shape` or `asset`, not both"));
        }
        (None, None) => {
            return Err(invalid("bad_params", "pass `shape` or `asset`"));
        }
        _ => {}
    }
    let (next_shape, custom, path) = if let Some(shape) = shape {
        let name = shape
            .as_str()
            .ok_or_else(|| invalid("bad_params", "`shape` must be a string"))?;
        let preset = FormationAsset::preset(name)
            .ok_or_else(|| formation_code("unknown_shape", format!("unknown formation shape `{name}`")))?;
        let shape = preset_shape(name)
            .ok_or_else(|| formation_code("unknown_shape", format!("unknown formation shape `{name}`")))?;
        (shape, preset, None)
    } else {
        let path = asset
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| formation_code("invalid_asset", "`asset` must be a file path"))?;
        let text = std::fs::read_to_string(path)
            .map_err(|_| formation_code("invalid_asset", format!("could not read `{path}`")))?;
        let file = FormationFile::from_ron(&text)
            .map_err(|error| formation_code("invalid_asset", error.to_string()))?;
        let custom = file
            .to_asset()
            .map_err(|error| formation_code("invalid_asset", error.to_string()))?;
        let shape = world
            .get::<AiFormation>(squad)
            .map(|formation| formation.shape)
            .unwrap_or_default();
        (shape, custom, Some(path.to_string()))
    };
    if world.get::<AiFormation>(squad).is_none() {
        world.entity_mut(squad).insert(AiFormation::new(
            FormationAnchor::Leader,
            FormationShape::default(),
        ));
    }
    {
        let Some(mut formation) = world.get_mut::<AiFormation>(squad) else {
            return Err(formation_code("unknown_squad", "the squad has no formation"));
        };
        formation.shape = next_shape;
        formation.custom = Some(custom);
        formation.asset_path = path;
        formation.dirty = true;
    }
    resolve_formation(world, squad);
    formation_report(world, squad)
}

#[cfg(test)]
mod tests {
    use regia_ai::spatial_query::{
        GeneratedItem, GeneratorImpl, GeneratorInput, SpatialQueryPlugin, TestImpl, TestInput, TestValue,
    };
    use regia_ai::squad::AiSquadPlugin;
    use regia_ai::{AiDead, AiPlugin};

    use super::*;

    fn world() -> World {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AiPlugin, AiSquadPlugin, SpatialQueryPlugin));
        std::mem::take(app.world_mut())
    }

    fn reason(error: &RpcError) -> Option<&str> {
        error.data.as_ref()?.get("reason")?.as_str()
    }

    fn code(error: &RpcError) -> Option<&str> {
        error.data.as_ref()?.get("code")?.as_str()
    }

    #[test]
    fn every_method_says_unavailable_without_its_plugin() {
        let mut bare = World::new();
        let empty = json!({});
        let on = json!({ "enabled": true });
        let cases: [(Result<Value, RpcError>, &str); 5] = [
            (squads(&mut bare, &empty), "ai_squads_unavailable"),
            (squad_set(&mut bare, &empty), "ai_squads_unavailable"),
            (tick_budget_get(&mut bare, &empty), "ai_tick_budget_unavailable"),
            (tick_budget_set(&mut bare, &on), "ai_tick_budget_unavailable"),
            (query_impls(&mut bare, &empty), "ai_query_unavailable"),
        ];
        for (result, expected) in cases {
            let error = result.expect_err("unavailable");
            assert_eq!(code(&error), Some(expected));
        }
    }

    #[test]
    fn a_squad_can_be_built_read_and_edited() {
        let mut world = world();
        let a = world.spawn(Blackboard::new()).id();
        let b = world.spawn(Blackboard::new()).id();
        let c = world.spawn(Blackboard::new()).id();
        let made = squad_set(
            &mut world,
            &json!({
                "create": true,
                "name": "alpha",
                "roles": [{ "name": "flanker", "capacity": 1 }, { "name": "support", "capacity": 1, "auto_fill": false }],
                "add": [a.to_bits(), { "entity": b.to_bits(), "rank": 5 }, c.to_bits()],
                "blackboard": { "plan": "push", "where": [1.0, 2.0, 3.0], "who": { "entity": a.to_bits() } },
            }),
        )
        .expect("created");
        let id = made["squad"]["entity"].as_u64().expect("entity");
        assert_eq!(made["squad"]["name"], "alpha");
        assert_eq!(made["squad"]["leader"], json!(a.to_bits()), "first in leads");
        assert_eq!(made["squad"]["blackboard"]["plan"], "push");
        assert_eq!(made["squad"]["blackboard"]["where"], json!([1.0, 2.0, 3.0]));
        assert_eq!(
            made["squad"]["blackboard"]["who"],
            json!({ "entity": a.to_bits() })
        );

        let edited = squad_set(
            &mut world,
            &json!({
                "squad": id,
                "leader": c.to_bits(),
                "role": { "member": c.to_bits(), "name": "support" },
                "blackboard": { "plan": null },
                "formation": { "anchor": "leader", "shape": { "kind": "line", "spacing": 2.0, "row_length": 3 } },
                "token_pool": { "target": a.to_bits(), "capacity": 2, "preempt": true },
            }),
        )
        .expect("edited");
        assert_eq!(edited["squad"]["leader"], json!(c.to_bits()));
        assert_eq!(edited["squad"]["blackboard"].get("plan"), None);
        assert_eq!(edited["squad"]["formation"]["shape"]["kind"], "line");
        assert_eq!(edited["token_pool"]["capacity"], 2);
        assert_eq!(edited["token_pool"]["preempt"], true);

        let read = squads(&mut world, &json!({ "squad": id })).expect("read");
        let roles = read["squad"]["roles"].as_array().expect("roles");
        assert_eq!(roles[1]["holders"], json!([c.to_bits()]));
        let all = squads(&mut world, &json!({})).expect("read all");
        assert_eq!(all["squads"].as_array().map(Vec::len), Some(1));
        assert_eq!(all["token_pools"].as_array().map(Vec::len), Some(1));

        squad_set(
            &mut world,
            &json!({ "squad": id, "remove": [c.to_bits()], "formation": null }),
        )
        .expect("removed");
        let after = squads(&mut world, &json!({ "squad": id })).expect("read");
        assert_eq!(after["squad"]["leader"], json!(b.to_bits()), "rank 5 is next");
        assert_eq!(after["squad"]["formation"], Value::Null);
    }

    #[test]
    fn malformed_squad_params_answer_documented_reasons() {
        let mut world = world();
        let a = world.spawn(Blackboard::new()).id();
        let not_squad = world.spawn_empty().id();
        let gone = world.spawn_empty().id();
        world.despawn(gone);
        let squad = world.spawn(AiSquad::new("s")).id();
        let member = world.spawn(AiSquadMember::new(squad)).id();
        let id = squad.to_bits();
        let cases: Vec<(Value, &str, &str)> = vec![
            (json!([]), "F0002", "bad_params"),
            (json!({}), "F0002", "missing_param"),
            (json!({ "squad": "x" }), "F0002", "bad_entity"),
            (json!({ "squad": gone.to_bits() }), "F0003", "entity_not_found"),
            (json!({ "squad": not_squad.to_bits() }), "F0003", "not_a_squad"),
            (json!({ "create": 1 }), "F0002", "bad_create"),
            (json!({ "create": true, "squad": id }), "F0002", "bad_create"),
            (json!({ "squad": id, "name": 3 }), "F0002", "bad_name"),
            (json!({ "squad": id, "roles": 3 }), "F0002", "bad_role"),
            (
                json!({ "squad": id, "roles": [{ "capacity": 1 }] }),
                "F0002",
                "bad_role",
            ),
            (
                json!({ "squad": id, "roles": [{ "name": "x" }] }),
                "F0002",
                "bad_role",
            ),
            (json!({ "squad": id, "add": 3 }), "F0002", "bad_entity"),
            (
                json!({ "squad": id, "add": [{ "entity": a.to_bits(), "rank": "high" }] }),
                "F0002",
                "bad_rank",
            ),
            (json!({ "squad": id, "remove": ["x"] }), "F0002", "bad_entity"),
            (
                json!({ "squad": id, "remove": [a.to_bits()] }),
                "F0003",
                "not_a_member",
            ),
            (
                json!({ "squad": id, "leader": a.to_bits() }),
                "F0003",
                "not_a_member",
            ),
            (
                json!({ "squad": id, "role": { "name": "x" } }),
                "F0002",
                "bad_role",
            ),
            (
                json!({ "squad": id, "role": { "member": member.to_bits(), "name": "x" } }),
                "F0003",
                "unknown_role",
            ),
            (json!({ "squad": id, "blackboard": 3 }), "F0002", "bad_blackboard"),
            (
                json!({ "squad": id, "blackboard": { "k": [1, 2] } }),
                "F0002",
                "bad_blackboard",
            ),
            (json!({ "squad": id, "formation": 3 }), "F0002", "bad_formation"),
            (
                json!({ "squad": id, "formation": { "shape": { "kind": "blob" } } }),
                "F0002",
                "bad_formation",
            ),
            (
                json!({ "squad": id, "formation": { "shape": { "kind": "ring", "radius": -1.0, "spacing": 1.0 } } }),
                "F0002",
                "bad_formation",
            ),
            (
                json!({ "squad": id, "formation": { "anchor": 7 } }),
                "F0002",
                "bad_formation",
            ),
            (
                json!({ "squad": id, "formation": { "extents": [1.0, 0.0, 1.0] } }),
                "F0002",
                "bad_formation",
            ),
            (json!({ "token_pool": 3 }), "F0002", "bad_tokens"),
            (json!({ "token_pool": {} }), "F0002", "bad_tokens"),
            (
                json!({ "token_pool": { "target": a.to_bits() } }),
                "F0002",
                "bad_tokens",
            ),
            (
                json!({ "token_pool": { "target": a.to_bits(), "capacity": -1 } }),
                "F0002",
                "bad_tokens",
            ),
            (
                json!({ "token_pool": { "target": a.to_bits(), "capacity": 1 }, "name": "x" }),
                "F0002",
                "missing_param",
            ),
            // A bad pool stops the whole call, before a squad is spawned.
            (
                json!({ "create": true, "token_pool": { "target": "x", "capacity": 1 } }),
                "F0002",
                "bad_entity",
            ),
            (
                json!({ "squad": id, "add": [a.to_bits()], "token_pool": { "target": gone.to_bits(), "capacity": 1 } }),
                "F0003",
                "entity_not_found",
            ),
        ];
        for (params, expected_code, expected_reason) in cases {
            let error = squad_set(&mut world, &params).expect_err(&params.to_string());
            assert_eq!(code(&error), Some(expected_code), "{params}");
            assert_eq!(reason(&error), Some(expected_reason), "{params}");
        }
        // Nothing changed on the way.
        let read = squads(&mut world, &json!({ "squad": id })).expect("read");
        assert_eq!(read["squad"]["members"].as_array().map(Vec::len), Some(1));
        let everything = squads(&mut world, &json!({})).expect("read all");
        assert_eq!(
            everything["squads"].as_array().map(Vec::len),
            Some(1),
            "no squad was spawned"
        );

        // A dead agent cannot be made leader, and the roster stays as it was.
        let dead = world.spawn(AiDead).id();
        let error = squad_set(
            &mut world,
            &json!({ "squad": id, "add": [dead.to_bits()], "leader": dead.to_bits() }),
        )
        .expect_err("dead");
        assert_eq!(reason(&error), Some("not_a_member"));

        // A full role is refused.
        let other = world.spawn(AiSquadMember::new(squad)).id();
        squad_set(
            &mut world,
            &json!({ "squad": id, "roles": [{ "name": "one", "capacity": 0, "auto_fill": false }] }),
        )
        .expect("role");
        let error = squad_set(
            &mut world,
            &json!({ "squad": id, "role": { "member": other.to_bits(), "name": "one" } }),
        )
        .expect_err("full");
        assert_eq!(reason(&error), Some("role_full"));

        for params in [json!({ "squad": "x" }), json!({ "squad": gone.to_bits() })] {
            assert!(squads(&mut world, &params).is_err());
        }
        let error = squads(&mut world, &json!({ "squad": not_squad.to_bits() })).expect_err("no squad");
        assert_eq!(reason(&error), Some("not_a_squad"));
    }

    #[test]
    fn adding_a_member_again_changes_its_rank_and_nothing_else() {
        let mut world = world();
        let squad = world.spawn(AiSquad::new("s")).id();
        let leader = world.spawn(AiSquadMember::new(squad)).id();
        let other = world.spawn(AiSquadMember::new(squad)).id();
        let read = squad_set(
            &mut world,
            &json!({ "squad": squad.to_bits(), "add": [{ "entity": leader.to_bits(), "rank": 9 }, other.to_bits()] }),
        )
        .expect("set");
        assert_eq!(
            read["squad"]["leader"],
            json!(leader.to_bits()),
            "a living leader stays"
        );
        assert_eq!(read["squad"]["elections"], 1);
        assert_eq!(read["squad"]["members"][0]["entity"], json!(leader.to_bits()));
        assert_eq!(read["squad"]["members"][0]["rank"], 9);
    }

    #[test]
    fn editing_roles_evicts_and_fills_at_once() {
        let mut world = world();
        let squad = world.spawn(AiSquad::new("s")).id();
        let a = world.spawn(AiSquadMember::new(squad)).id();
        let b = world.spawn(AiSquadMember::new(squad)).id();
        let id = squad.to_bits();
        let up = squad_set(
            &mut world,
            &json!({ "squad": id, "roles": [{ "name": "r", "capacity": 2 }] }),
        )
        .expect("set");
        assert_eq!(
            up["squad"]["roles"][0]["holders"],
            json!([a.to_bits(), b.to_bits()])
        );
        let down = squad_set(
            &mut world,
            &json!({ "squad": id, "roles": [{ "name": "r", "capacity": 1 }] }),
        )
        .expect("set");
        assert_eq!(down["squad"]["roles"][0]["holders"], json!([a.to_bits()]));
        assert_eq!(world.get::<AiSquadMember>(b).expect("member").role, None);
    }

    #[test]
    fn a_dead_member_is_gone_from_the_answer() {
        let mut world = world();
        let squad = world.spawn(AiSquad::new("s")).id();
        let a = world.spawn(AiSquadMember::new(squad)).id();
        let b = world.spawn(AiSquadMember::new(squad)).id();
        world.entity_mut(a).insert(AiDead);
        let read = squads(&mut world, &json!({ "squad": squad.to_bits() })).expect("read");
        assert_eq!(read["squad"]["leader"], json!(b.to_bits()));
        assert_eq!(read["squad"]["members"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn the_tick_budget_round_trips_and_reports_an_agent() {
        let mut world = world();
        let focus = world
            .spawn(regia_scene::Transform::from_translation(Vec3::ZERO))
            .id();
        let agent = world
            .spawn(regia_scene::Transform::from_translation(Vec3::new(
                50.0, 0.0, 0.0,
            )))
            .id();
        let off = tick_budget_get(&mut world, &json!({})).expect("read");
        assert_eq!(off["budget"]["enabled"], false, "off by default");

        let set = tick_budget_set(
            &mut world,
            &json!({
                "enabled": true,
                "max_trees_per_step": 10,
                "focus": focus.to_bits(),
                "tiers": [
                    { "max_distance": 20.0, "cadence_steps": 1 },
                    { "cadence_steps": 8, "max_trees_per_step": 3 },
                ],
            }),
        )
        .expect("set");
        assert_eq!(set["budget"]["enabled"], true);
        assert_eq!(set["budget"]["tiers"][1]["cadence_steps"], 8);

        let read = tick_budget_get(&mut world, &json!({ "entity": agent.to_bits() })).expect("read");
        assert_eq!(read["budget"]["max_trees_per_step"], 10);
        assert_eq!(read["agent"]["tier"], 1);
        assert_eq!(read["agent"]["distance"], json!(50.0));

        let cleared = tick_budget_set(&mut world, &json!({ "focus": null })).expect("cleared");
        assert_eq!(cleared["budget"]["focus"], Value::Null);
        assert_eq!(
            cleared["budget"]["max_trees_per_step"], 10,
            "others keep their values"
        );
    }

    #[test]
    fn malformed_budget_params_answer_documented_reasons() {
        let mut world = world();
        let gone = world.spawn_empty().id();
        world.despawn(gone);
        let cases: Vec<(Value, &str, &str)> = vec![
            (json!([]), "F0002", "bad_params"),
            (json!({}), "F0002", "nothing_to_set"),
            (json!({ "enabled": 1 }), "F0002", "bad_enabled"),
            (json!({ "max_trees_per_step": -1 }), "F0002", "bad_max_trees"),
            (json!({ "max_trees_per_step": 1.5 }), "F0002", "bad_max_trees"),
            (json!({ "tiers": 3 }), "F0002", "bad_tiers"),
            (json!({ "tiers": [] }), "F0002", "bad_tiers"),
            (json!({ "tiers": [3] }), "F0002", "bad_tiers"),
            (
                json!({ "tiers": [{ "max_distance": 1.0 }] }),
                "F0002",
                "bad_tiers",
            ),
            (json!({ "tiers": [{ "cadence_steps": 0 }] }), "F0002", "bad_tiers"),
            (
                json!({ "tiers": [{ "max_distance": -1.0, "cadence_steps": 1 }] }),
                "F0002",
                "bad_tiers",
            ),
            (
                json!({ "tiers": [{ "cadence_steps": 1 }, { "cadence_steps": 2 }] }),
                "F0002",
                "bad_tiers",
            ),
            (
                json!({ "tiers": [{ "max_distance": 9.0, "cadence_steps": 1 }, { "max_distance": 3.0, "cadence_steps": 2 }] }),
                "F0002",
                "bad_tiers",
            ),
            (
                json!({ "tiers": [{ "cadence_steps": 1, "max_trees_per_step": "x" }] }),
                "F0002",
                "bad_tiers",
            ),
            (json!({ "focus": "x" }), "F0002", "bad_focus"),
            (json!({ "focus": gone.to_bits() }), "F0003", "entity_not_found"),
        ];
        for (params, expected_code, expected_reason) in cases {
            let error = tick_budget_set(&mut world, &params).expect_err(&params.to_string());
            assert_eq!(code(&error), Some(expected_code), "{params}");
            assert_eq!(reason(&error), Some(expected_reason), "{params}");
        }
        // A bad call changes nothing.
        assert!(!world.contains_resource::<AiTickBudget>());
        let error = tick_budget_get(&mut world, &json!({ "entity": gone.to_bits() })).expect_err("gone");
        assert_eq!(reason(&error), Some("entity_not_found"));
    }

    struct G;
    impl GeneratorImpl for G {
        fn generate(&self, _input: &GeneratorInput<'_>) -> Vec<GeneratedItem> {
            Vec::new()
        }
    }
    struct T;
    impl TestImpl for T {
        fn evaluate(&self, _input: &TestInput<'_>) -> TestValue {
            TestValue::Bool(true)
        }
    }

    #[test]
    fn query_impls_lists_the_registered_names() {
        let mut world = world();
        let none = query_impls(&mut world, &json!({})).expect("read");
        assert_eq!(none["generators"], json!([]));
        let mut manager = world.resource_mut::<SpatialQueryManager>();
        manager.register_generator("cover", Box::new(G));
        manager.register_test("quiet", Box::new(T));
        let read = query_impls(&mut world, &json!({})).expect("read");
        assert_eq!(read["generators"], json!(["cover"]));
        assert_eq!(read["tests"], json!(["quiet"]));
    }
}
