// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of abilities, attributes and effects
//! (`docs/specs/53-abilities-attributes-effects.md` § 5, `regia_framework::abilities`):
//! `abilities.{list, inspect, check, grant, revoke, activate, charge, end, cancel, signal,
//! apply_effect, remove_effect, set_base, add_attributes, cues, validate, stats}`.
//!
//! The methods are extension methods: a host that adds `AbilityPlugin` calls
//! [`install_ability_methods`] once to register them on the world's [`AgentExtension`]. Without
//! the plugin every method answers `{ "available": false }`. Refused activations and applications
//! are results (`started: false`, `cause`), not errors. Documented on `book/agents/abilities.md`.
//!
//! Error reasons (`data.reason`): `entity_not_found`, `no_attributes`, `unknown_attribute`,
//! `attribute_derived`, `attribute_not_finite`, `effect_not_found`, `grant_not_found`,
//! `asset_not_found`, `not_cancelable`, `bad_query`, `format_too_new`, `reference_missing`.

use regia_app::prelude::*;
use regia_framework::abilities::{
    self as ab, ATTRIBUTE_QUANTUM, AbilityAssets, AbilityBook, AbilityCounters, AbilityError, AbilitySignal,
    AbilityStats, AppliedEffects, ApplyEffect, AttrId, AttributeBreakdown, Attributes, CueLog, EffectId,
    GrantId, GrantOptions, Pick, Picks, Refusal, StartContext, TagCounts, abilities_state_hash,
};
use regia_framework::tags::{Tag, TagContainer, TagQuery, TagRegistry};
use regia_scene::components::Name;
use serde_json::{Map, Value, json};

use crate::extension::AgentExtension;
use crate::protocol::{self, RpcError};
use crate::schema::{MethodSchema, ParamSchema, Permission};

const HINT: &str = "see book/agents/abilities.md";

fn error(code: i64, regia_code: &str, reason: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: Some(json!({ "code": regia_code, "reason": reason, "hint": HINT })),
    }
}

fn invalid(reason: &str, message: impl Into<String>) -> RpcError {
    error(protocol::INVALID_PARAMS, "F0002", reason, message)
}

fn missing(reason: &str, message: impl Into<String>) -> RpcError {
    error(protocol::NOT_FOUND, "F0003", reason, message)
}

/// Maps a module error to `F0002`/`F0003` with its reason.
fn from_error(e: &AbilityError) -> RpcError {
    let reason = e.reason();
    match reason {
        "entity_not_found" | "no_attributes" | "unknown_attribute" | "effect_not_found"
        | "grant_not_found" | "asset_not_found" | "reference_missing" => missing(reason, e.to_string()),
        _ => invalid(reason, e.to_string()),
    }
}

fn object(params: &Value) -> Result<Map<String, Value>, RpcError> {
    match params {
        Value::Object(map) => Ok(map.clone()),
        Value::Null => Ok(Map::new()),
        _ => Err(invalid("entity_not_found", "params must be an object")),
    }
}

/// A required entity id that names a live entity.
fn entity(world: &World, map: &Map<String, Value>, key: &str) -> Result<Entity, RpcError> {
    let e = crate::params::opt_u64(map, key)?
        .and_then(Entity::try_from_bits)
        .ok_or_else(|| invalid("entity_not_found", format!("`{key}` must be an entity id")))?;
    if world.get_entity(e).is_err() {
        return Err(missing(
            "entity_not_found",
            format!("entity {} does not exist", e.to_bits()),
        ));
    }
    Ok(e)
}

fn opt_entity(world: &World, map: &Map<String, Value>, key: &str) -> Result<Option<Entity>, RpcError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => entity(world, map, key).map(Some),
    }
}

fn grant(world: &World, map: &Map<String, Value>, owner: Entity) -> Result<GrantId, RpcError> {
    let id = crate::params::opt_u64(map, "grant")?
        .and_then(|v| u32::try_from(v).ok())
        .map(GrantId)
        .ok_or_else(|| invalid("grant_not_found", "`grant` must be a grant id"))?;
    if world.get::<AbilityBook>(owner).and_then(|b| b.get(id)).is_none() {
        return Err(missing(
            "grant_not_found",
            format!("entity has no grant {}", id.0),
        ));
    }
    Ok(id)
}

fn string<'a>(map: &'a Map<String, Value>, key: &str, reason: &str) -> Result<&'a str, RpcError> {
    map.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(reason, format!("`{key}` must be a string")))
}

fn tag(text: &str) -> Result<Tag, RpcError> {
    Tag::new(text).map_err(|e| invalid("bad_query", format!("bad tag `{text}`: {e}")))
}

fn available(world: &World) -> bool {
    world.contains_resource::<AbilityAssets>()
}

fn unavailable() -> Value {
    json!({ "available": false })
}

fn attr_name(attrs: Option<&Attributes>, id: AttrId) -> Value {
    attrs
        .and_then(|a| a.attribute(id))
        .map_or_else(|| json!(format!("{:#x}", id.0)), |a| json!(a.name))
}

fn refusal_json(world: &World, owner: Entity, r: &Refusal) -> Value {
    json!({
        "cause": r.cause.code(),
        "tag": r.tag.map(|t| t.name()),
        "attribute": r.attribute.map(|a| attr_name(world.get::<Attributes>(owner), a)),
    })
}

fn state_code(world: &World, owner: Entity, g: GrantId) -> &'static str {
    ab::ability_state(world, owner, g).map_or("revoked", |s| s.code())
}

fn breakdown_json(b: &AttributeBreakdown) -> Value {
    json!({
        "attribute": b.name,
        "base": b.base,
        "phases": b.phases.iter().map(|p| json!({
            "phase": p.phase,
            "value_after": p.value_after,
            "modifiers": p.modifiers.iter().map(|m| json!({
                "effect": m.effect.0,
                "op": m.op.code(),
                "amount": m.amount,
                "stacks": m.stacks,
                "contribution": m.contribution,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "replace_winner": b.replace_winner.map(|e| e.0),
        "unclamped": b.unclamped,
        "clamp": [b.clamp.0, b.clamp.1],
        "current": b.current,
    })
}

fn grants_json(world: &World, owner: Entity) -> Vec<Value> {
    world
        .get::<AbilityBook>(owner)
        .map(|b| {
            b.grants
                .iter()
                .map(|g| {
                    json!({
                        "id": g.id.0,
                        "asset": g.asset,
                        "level": g.level,
                        "state": state_code(world, owner, g.id),
                        "input_code": g.input_code,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// --- read -----------------------------------------------------------------------------------

fn list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !available(world) {
        return Ok(unavailable());
    }
    let map = object(params)?;
    let only = opt_entity(world, &map, "entity")?;
    let mut query = world.query_filtered::<Entity, Or<(With<Attributes>, With<AbilityBook>)>>();
    let mut entities: Vec<Entity> = query
        .iter(world)
        .filter(|e| only.is_none_or(|o| o == *e))
        .collect();
    entities.sort_by_key(|e| e.to_bits());
    let out: Vec<Value> = entities
        .into_iter()
        .map(|e| {
            json!({
                "entity": e.to_bits(),
                "name": world.get::<Name>(e).map(|n| n.as_str().to_owned()),
                "attributes": world.get::<Attributes>(e).map_or(0, Attributes::len),
                "live_effects": world.get::<AppliedEffects>(e).map_or(0, |l| l.list.len()),
                "grants": grants_json(world, e),
            })
        })
        .collect();
    Ok(json!({ "available": true, "entities": out }))
}

fn inspect(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !available(world) {
        return Ok(unavailable());
    }
    let map = object(params)?;
    let e = entity(world, &map, "entity")?;
    let attrs = world.get::<Attributes>(e);
    let only: Option<AttrId> = match map.get("attribute") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let name = v
                .as_str()
                .ok_or_else(|| invalid("unknown_attribute", "`attribute` must be a name"))?;
            let id = AttrId::of(name);
            if attrs.and_then(|a| a.attribute(id)).is_none() {
                return Err(missing("unknown_attribute", format!("no attribute `{name}`")));
            }
            Some(id)
        }
    };
    let attributes: Vec<Value> = attrs
        .map(|a| {
            a.list()
                .iter()
                .filter(|x| only.is_none_or(|o| o == x.id))
                .map(|x| {
                    json!({
                        "name": x.name, "base": x.base, "current": x.current, "min": x.min, "max": x.max,
                        "derived": x.derived.is_some(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let breakdowns: Vec<Value> = attrs
        .map(|a| {
            a.list()
                .iter()
                .filter(|x| only.is_none_or(|o| o == x.id))
                .filter_map(|x| ab::explain_attribute(world, e, x.id))
                .map(|b| breakdown_json(&b))
                .collect()
        })
        .unwrap_or_default();
    let assets = world.get_resource::<AbilityAssets>();
    let effects: Vec<Value> = world
        .get::<AppliedEffects>(e)
        .map(|l| {
            l.list
                .iter()
                .map(|x| {
                    let status: Vec<&str> = assets
                        .and_then(|a| a.effect(&x.asset))
                        .map(|a| a.status_tags.iter().map(Tag::name).collect())
                        .unwrap_or_default();
                    json!({
                        "id": x.id.0, "asset": x.asset, "level": x.level,
                        "source": x.source.map(Entity::to_bits), "stacks": x.stacks,
                        "remaining_ticks": x.remaining_ticks, "next_pulse_tick": x.next_pulse_tick,
                        "suspended": x.suspended, "status_tags": status, "applied_tick": x.applied_tick,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let mut tags: Vec<Value> = world
        .get::<TagContainer>(e)
        .map(|c| {
            c.iter()
                .map(|t| json!({ "tag": t.name(), "source": "explicit", "count": 1 }))
                .collect()
        })
        .unwrap_or_default();
    if let Some(counts) = world.get::<TagCounts>(e) {
        tags.extend(
            counts
                .iter()
                .iter()
                .map(|(t, n)| json!({ "tag": t.name(), "source": "counted", "count": n })),
        );
    }
    Ok(json!({
        "available": true,
        "entity": e.to_bits(),
        "attributes": attributes,
        "effects": effects,
        "tags": tags,
        "grants": grants_json(world, e),
        "breakdown": breakdowns,
    }))
}

fn check(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !available(world) {
        return Ok(unavailable());
    }
    let map = object(params)?;
    let e = entity(world, &map, "entity")?;
    let g = grant(world, &map, e)?;
    Ok(
        match ab::check_activation(world, e, g, &StartContext::default()) {
            Ok(()) => json!({ "ok": true }),
            Err(r) => {
                let mut v = refusal_json(world, e, &r);
                v["ok"] = json!(false);
                v
            }
        },
    )
}

fn cues(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !available(world) {
        return Ok(unavailable());
    }
    let map = object(params)?;
    let since = crate::params::opt_u64(&map, "since_tick")?.unwrap_or(0);
    let limit =
        crate::params::opt_u64(&map, "limit")?.map_or(ab::CUE_RING, |l| l.min(ab::CUE_RING as u64) as usize);
    let list: Vec<Value> = world
        .get_resource::<CueLog>()
        .map(|log| {
            log.since(since, limit)
                .into_iter()
                .map(|(tick, c)| {
                    json!({
                        "tick": tick, "entity": c.entity.to_bits(), "tag": c.tag.name(), "kind": c.kind.code(),
                        "source": c.source.map(Entity::to_bits), "amount": c.amount,
                        "effect": c.effect.map(|x| x.0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({ "available": true, "cues": list }))
}

/// Ability files are text; 8 MiB is far above any real one.
fn read_text_capped(file: &std::path::Path) -> std::io::Result<String> {
    let bytes = crate::req_path::read_file_capped(file, 8 << 20)?;
    String::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn validate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !available(world) {
        return Ok(unavailable());
    }
    let map = object(params)?;
    let path = string(&map, "path", "asset_not_found")?;
    let file = crate::req_path::allowed_path(world, "path", path)?;
    let text =
        read_text_capped(&file).map_err(|_| missing("asset_not_found", format!("cannot read `{path}`")))?;
    let mut scratch = world.get_resource::<AbilityAssets>().cloned().unwrap_or_default();
    let diags = scratch.load_text(
        path,
        &text,
        world.get_resource::<TagRegistry>(),
        world.get_resource::<ab::RunStateTypes>(),
    );
    let list: Vec<Value> = diags
        .iter()
        .map(|d| {
            json!({
                "code": d.code, "severity": d.severity.code(), "message": d.message,
                "span": d.span.map(|(l, c)| json!({ "line": l, "column": c })),
            })
        })
        .collect();
    Ok(json!({ "available": true, "ok": !ab::has_errors(&diags), "diagnostics": list }))
}

fn stats(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    if !available(world) {
        return Ok(unavailable());
    }
    let tick = world.get_resource::<AbilityCounters>().map_or(0, |c| c.tick);
    let mut v = world
        .get_resource::<AbilityStats>()
        .and_then(|s| serde_json::to_value(s).ok())
        .unwrap_or_else(|| json!({}));
    v["available"] = json!(true);
    v["tick"] = json!(tick);
    // What the startup loader did per registered path (ADR-9370): from the pak's cooked form or
    // the loose RON, and the error codes of a refused asset. Empty when the game loads its assets
    // itself.
    let loads: Vec<Value> = world
        .get_resource::<ab::AbilityAssetRegistrations>()
        .map(|r| {
            r.loads
                .iter()
                .map(|(path, load)| {
                    json!({
                        "path": path,
                        "cooked": load.cooked,
                        "loaded": load.loaded(),
                        "errors": load
                            .diagnostics
                            .iter()
                            .filter(|d| d.severity == ab::Severity::Error)
                            .map(|d| d.code)
                            .collect::<Vec<_>>(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    v["asset_loads"] = json!(loads);
    Ok(v)
}

/// `abilities.state_hash`: the cross-platform hash of spec 53 § 7, as 16 hex digits (a `u64` does
/// not survive a JSON number).
fn state_hash(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    if !available(world) {
        return Ok(unavailable());
    }
    let hash = abilities_state_hash(world);
    Ok(json!({ "available": true, "hash": format!("{hash:016x}"), "quantum": ATTRIBUTE_QUANTUM }))
}

// --- play -----------------------------------------------------------------------------------

fn grant_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !available(world) {
        return Ok(unavailable());
    }
    let map = object(params)?;
    let e = entity(world, &map, "entity")?;
    let path = string(&map, "ability", "asset_not_found")?.to_owned();
    let asset = world
        .resource::<AbilityAssets>()
        .ability(&path)
        .ok_or_else(|| missing("asset_not_found", format!("ability `{path}` is not loaded")))?;
    for reference in asset.cost.iter().chain(&asset.cooldown) {
        if world.resource::<AbilityAssets>().effect(reference).is_none() {
            return Err(missing(
                "reference_missing",
                format!("`{path}` names `{reference}`, which is not loaded"),
            ));
        }
    }
    // A level or code that is not an exact integer in range is refused, not coerced (a 3.5 level
    // used to grant level 1, a 99999 code to bind the asset's code; ADR-1809).
    let level = match map.get("level") {
        None | Some(Value::Null) => 1,
        Some(v) => v
            .as_u64()
            .and_then(|l| u32::try_from(l).ok())
            .ok_or_else(|| invalid("bad_value", "`level` must be an integer in 0..=4294967295"))?,
    };
    let input_code = match map.get("input_code") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|c| u16::try_from(c).ok())
                .ok_or_else(|| invalid("bad_value", "`input_code` must be an integer in 0..=65535"))?,
        ),
    };
    let opts = GrantOptions {
        level,
        input_code,
        try_now: crate::params::opt_bool(&map, "try_now")?.unwrap_or(false),
        one_shot: crate::params::opt_bool(&map, "one_shot")?.unwrap_or(false),
        extra_triggers: Vec::new(),
    };
    match ab::grant_ability(world, e, &path, opts) {
        Ok(g) => Ok(json!({ "grant": g.0 })),
        Err(AbilityError::StartRefused(cause)) => Ok(json!({ "grant": null, "cause": cause.code() })),
        Err(err) => Err(from_error(&err)),
    }
}

fn revoke(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !available(world) {
        return Ok(unavailable());
    }
    let map = object(params)?;
    let e = entity(world, &map, "entity")?;
    let g = grant(world, &map, e)?;
    let defer = crate::params::opt_bool(&map, "defer")?.unwrap_or(false);
    ab::revoke_ability(world, e, g, defer).map_err(|err| from_error(&err))?;
    Ok(json!({ "revoked": true }))
}

/// Every item must be an entity id or `{ point: [x, y, z] }` of finite numbers: a malformed item is
/// refused (`bad_value`), never skipped or zeroed (ADR-1809).
fn picks(world: &World, value: Option<&Value>) -> Result<Option<Picks>, RpcError> {
    let items = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(invalid("bad_value", "`picks` must be an array")),
    };
    let mut out = Vec::new();
    for item in items {
        if let Some(bits) = item.as_u64() {
            let e = Entity::try_from_bits(bits)
                .filter(|e| world.get_entity(*e).is_ok())
                .ok_or_else(|| missing("entity_not_found", format!("pick {bits} is not an entity")))?;
            out.push(Pick::Entity(e));
            continue;
        }
        let point = crate::params::opt_array(item, "point")?
            .filter(|a| a.len() == 3)
            .and_then(|p| {
                let v: Vec<f32> = p
                    .iter()
                    .filter_map(Value::as_f64)
                    .map(|x| x as f32)
                    .filter(|x| x.is_finite())
                    .collect();
                (v.len() == 3).then(|| regia_core::math::Vec3::new(v[0], v[1], v[2]))
            })
            .ok_or_else(|| {
                invalid(
                    "bad_value",
                    format!("pick {item} is not an entity id or {{ point: [x, y, z] }} of finite numbers"),
                )
            })?;
        out.push(Pick::Point(point));
    }
    Ok(Some(Picks::new(out)))
}

fn activate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !available(world) {
        return Ok(unavailable());
    }
    let map = object(params)?;
    let e = entity(world, &map, "entity")?;
    let g = grant(world, &map, e)?;
    let picks = picks(world, map.get("picks"))?;
    let signal = match crate::params::opt_str(&map, "signal")? {
        Some(name) => Some(AbilitySignal::new(e, tag(name)?)),
        None => None,
    };
    let ctx = StartContext {
        picks,
        signal,
        intent: ab::IntentSeq(0),
    };
    Ok(match ab::activate(world, e, g, ctx) {
        Ok(()) => json!({ "started": true, "state": state_code(world, e, g) }),
        Err(r) => {
            let mut v = refusal_json(world, e, &r);
            v["started"] = json!(false);
            v["state"] = json!(state_code(world, e, g));
            v
        }
    })
}

fn charge(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !available(world) {
        return Ok(unavailable());
    }
    let map = object(params)?;
    let e = entity(world, &map, "entity")?;
    let g = grant(world, &map, e)?;
    Ok(match ab::charge(world, e, g) {
        Ok(()) => json!({ "charged": true }),
        Err(r) => {
            let mut v = refusal_json(world, e, &r);
            v["charged"] = json!(false);
            v
        }
    })
}

fn end(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !available(world) {
        return Ok(unavailable());
    }
    let map = object(params)?;
    let e = entity(world, &map, "entity")?;
    let g = grant(world, &map, e)?;
    ab::end(world, e, g).map_err(|err| from_error(&err))?;
    Ok(json!({ "state": state_code(world, e, g) }))
}

fn cancel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !available(world) {
        return Ok(unavailable());
    }
    let map = object(params)?;
    let e = entity(world, &map, "entity")?;
    let g = grant(world, &map, e)?;
    let before = state_code(world, e, g);
    ab::cancel(world, e, g).map_err(|err| from_error(&err))?;
    let after = state_code(world, e, g);
    Ok(json!({ "cancelled": before == "running" && after == "ending", "state": after }))
}

fn signal(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !available(world) {
        return Ok(unavailable());
    }
    let map = object(params)?;
    let e = entity(world, &map, "entity")?;
    let mut s = AbilitySignal::new(e, tag(string(&map, "tag", "bad_query")?)?);
    s.source = opt_entity(world, &map, "source")?;
    s.amount = crate::params::opt_f64(&map, "amount")?.unwrap_or(0.0) as f32;
    let started: Vec<u32> = ab::signal_now(world, s).into_iter().map(|g| g.0).collect();
    Ok(json!({ "started": started }))
}

fn apply_effect(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !available(world) {
        return Ok(unavailable());
    }
    let map = object(params)?;
    let target = entity(world, &map, "target")?;
    // Same rule as `abilities.grant` (ADR-1809): a level that is not an exact u32 is refused.
    // Checked before the asset lookup so a bad level does not depend on the effect being loaded.
    let level = match map.get("level") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|l| u32::try_from(l).ok())
                .ok_or_else(|| invalid("bad_value", "`level` must be an integer in 0..=4294967295"))?,
        ),
    };
    let path = string(&map, "effect", "effect_not_found")?.to_owned();
    if world.resource::<AbilityAssets>().effect(&path).is_none() {
        return Err(missing(
            "effect_not_found",
            format!("effect `{path}` is not loaded"),
        ));
    }
    let mut req = ApplyEffect::new(&path, target);
    req.source = opt_entity(world, &map, "source")?;
    if let Some(level) = level {
        req.level = level;
    }
    if let Some(Value::Object(p)) = map.get("params") {
        for (k, v) in p {
            let value = v
                .as_f64()
                .ok_or_else(|| invalid("bad_query", format!("param `{k}` must be a number")))?;
            req.params.push((tag(k)?, value as f32));
        }
    }
    let out = ab::apply_effect(world, req).map_err(|err| from_error(&err))?;
    Ok(json!({ "outcome": out.outcome.code(), "id": out.id.map(|i| i.0), "stacks": out.stacks }))
}

fn remove_effect(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !available(world) {
        return Ok(unavailable());
    }
    let map = object(params)?;
    let e = entity(world, &map, "entity")?;
    if let Some(id) = map.get("id") {
        let id = id
            .as_u64()
            .map(EffectId)
            .ok_or_else(|| invalid("effect_not_found", "`id` must be an effect id"))?;
        ab::remove_effect(world, e, id).map_err(|err| from_error(&err))?;
        return Ok(json!({ "removed": 1 }));
    }
    let text = string(&map, "query", "bad_query")?;
    let query = TagQuery::from_ron(text).map_err(|err| invalid("bad_query", err.to_string()))?;
    let limit = crate::params::opt_u64(&map, "limit")?.map(|l| l.min(u64::from(u32::MAX)) as u32);
    Ok(json!({ "removed": ab::remove_effects_matching(world, e, &query, limit) }))
}

// --- edit_scene -----------------------------------------------------------------------------

fn set_base(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !available(world) {
        return Ok(unavailable());
    }
    let map = object(params)?;
    let e = entity(world, &map, "entity")?;
    let name = string(&map, "attribute", "unknown_attribute")?;
    let value = map
        .get("base")
        .and_then(Value::as_f64)
        .ok_or_else(|| invalid("attribute_not_finite", "`base` must be a number"))? as f32;
    let id = AttrId::of(name);
    match world.get::<Attributes>(e) {
        None => return Err(missing("no_attributes", "the entity has no attributes")),
        Some(a) if a.attribute(id).is_none() => {
            return Err(missing("unknown_attribute", format!("no attribute `{name}`")));
        }
        Some(_) => {}
    }
    let (old, new) = ab::set_base(world, e, id, value).map_err(|err| from_error(&err))?;
    Ok(json!({ "old": old, "new": new }))
}

/// The path `abilities.add_attributes` reads an unloaded attribute set from, resolved like the
/// abilities panel does (`regia_editor::abilities_panel`): a relative key names a file under the
/// project's `assets/` folder, spelled bare (`attrs/x.attrs.ron`) or with the `assets/` prefix; an
/// absolute path is left as it is (the request-path rules decide whether it is allowed).
fn assets_relative(path: &str) -> String {
    if std::path::Path::new(path).is_absolute() {
        return path.to_owned();
    }
    let under_assets = path.strip_prefix("assets/").unwrap_or(path);
    format!("assets/{under_assets}")
}

fn add_attributes(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !available(world) {
        return Ok(unavailable());
    }
    let map = object(params)?;
    let e = entity(world, &map, "entity")?;
    let path = string(&map, "asset", "asset_not_found")?.to_owned();
    if world.resource::<AbilityAssets>().attributes(&path).is_none() {
        let file = crate::req_path::allowed_path(world, "asset", &assets_relative(&path))?;
        let text = read_text_capped(&file).map_err(|_| {
            missing(
                "asset_not_found",
                format!("`{path}` is not loaded and cannot be read"),
            )
        })?;
        let tags = world.get_resource::<TagRegistry>().cloned();
        let diags = world
            .resource_mut::<AbilityAssets>()
            .load_text(&path, &text, tags.as_ref(), None);
        if let Some(d) = diags.iter().find(|d| d.severity == ab::Severity::Error) {
            let reason = if d.code == "format_too_new" {
                "format_too_new"
            } else {
                "asset_not_found"
            };
            return Err(invalid(reason, format!("`{path}`: {} ({})", d.message, d.code)));
        }
    }
    let n = ab::add_attributes(world, e, &path).map_err(|err| from_error(&err))?;
    Ok(json!({ "attributes": n }))
}

// --- registration ---------------------------------------------------------------------------

const fn opt(name: &'static str, kind: &'static str, description: &'static str) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required: false,
        description,
    }
}

const fn req(name: &'static str, kind: &'static str, description: &'static str) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required: true,
        description,
    }
}

const ENTITY: ParamSchema = req("entity", "number", "entity id");
const GRANT: ParamSchema = req("grant", "number", "grant id from abilities.grant");

static LIST_PARAMS: [ParamSchema; 1] = [opt("entity", "number", "only this entity")];
static INSPECT_PARAMS: [ParamSchema; 2] = [ENTITY, opt("attribute", "string", "only this attribute")];
static ENTITY_GRANT: [ParamSchema; 2] = [ENTITY, GRANT];
static GRANT_PARAMS: [ParamSchema; 6] = [
    ENTITY,
    req("ability", "string", "ability asset path"),
    opt("level", "number", "grant level (default 1)"),
    opt("input_code", "number", "input code to bind"),
    opt("try_now", "boolean", "start at once; keep nothing when refused"),
    opt("one_shot", "boolean", "remove after the first run"),
];
static REVOKE_PARAMS: [ParamSchema; 3] = [ENTITY, GRANT, opt("defer", "boolean", "remove when the run ends")];
static ACTIVATE_PARAMS: [ParamSchema; 4] = [
    ENTITY,
    GRANT,
    opt("picks", "array", "entity ids or { point: [x, y, z] }"),
    opt("signal", "string", "a signal tag carried by the run"),
];
static SIGNAL_PARAMS: [ParamSchema; 4] = [
    ENTITY,
    req("tag", "string", "signal tag"),
    opt("source", "number", "sender entity id"),
    opt("amount", "number", "a number carried by the signal"),
];
static APPLY_PARAMS: [ParamSchema; 5] = [
    req("target", "number", "target entity id"),
    req("effect", "string", "effect asset path"),
    opt("source", "number", "source entity id"),
    opt("level", "number", "level (default 1)"),
    opt("params", "object", "{ tag: number } for Param amounts"),
];
static REMOVE_PARAMS: [ParamSchema; 4] = [
    ENTITY,
    opt("id", "number", "effect id"),
    opt("query", "string", "tag query (RON) over kind_tags"),
    opt("limit", "number", "most effects removed by a query"),
];
static SET_BASE_PARAMS: [ParamSchema; 3] = [
    ENTITY,
    req("attribute", "string", "attribute name"),
    req("base", "number", "new base (clamped)"),
];
static ADD_PARAMS: [ParamSchema; 2] = [
    ENTITY,
    req(
        "asset",
        "string",
        ".attrs.ron asset key (a path under assets/; an assets/ prefix is accepted)",
    ),
];
static CUES_PARAMS: [ParamSchema; 2] = [
    opt("since_tick", "number", "only cues at or after this tick"),
    opt("limit", "number", "most cues returned (256 max)"),
];
static VALIDATE_PARAMS: [ParamSchema; 1] = [req(
    "path",
    "string",
    ".attrs.ron, .effect.ron or .ability.ron file",
)];

fn schema(
    name: &'static str,
    tool: &'static str,
    description: &'static str,
    params: &'static [ParamSchema],
    result: &'static str,
    permission: Permission,
) -> MethodSchema {
    MethodSchema {
        name,
        tool,
        description,
        params,
        result,
        mutating: permission != Permission::Read,
        permission,
    }
}

/// Every `abilities.*` method name, in registration order.
pub const METHODS: &[&str] = &[
    "abilities.list",
    "abilities.inspect",
    "abilities.check",
    "abilities.grant",
    "abilities.revoke",
    "abilities.activate",
    "abilities.charge",
    "abilities.end",
    "abilities.cancel",
    "abilities.signal",
    "abilities.apply_effect",
    "abilities.remove_effect",
    "abilities.set_base",
    "abilities.add_attributes",
    "abilities.cues",
    "abilities.validate",
    "abilities.stats",
    "abilities.state_hash",
];

/// Registers the `abilities.*` methods on the world's [`AgentExtension`] (creating the resource
/// when the app has none yet). Call it once after adding `AbilityPlugin`.
pub fn install_ability_methods(world: &mut World) {
    use Permission::{EditScene, Play, Read};
    let mut extension = world
        .get_resource::<AgentExtension>()
        .cloned()
        .unwrap_or_default();
    let rows: Vec<(MethodSchema, crate::extension::AgentHandler)> = vec![
        (
            schema(
                "abilities.list",
                "abilities_list",
                "Entities with attributes or grants: counts and grant states.",
                &LIST_PARAMS,
                "{ available, entities: [{ entity, name, attributes, live_effects, grants }] }",
                Read,
            ),
            list,
        ),
        (
            schema(
                "abilities.inspect",
                "abilities_inspect",
                "Attributes, live effects, effective tags, grants and the modifier breakdown of one entity.",
                &INSPECT_PARAMS,
                "{ available, attributes, effects, tags, grants, breakdown }",
                Read,
            ),
            inspect,
        ),
        (
            schema(
                "abilities.check",
                "abilities_check",
                "Whether a grant could start now, and the first refusal.",
                &ENTITY_GRANT,
                "{ ok, cause?, tag?, attribute? }",
                Read,
            ),
            check,
        ),
        (
            schema(
                "abilities.grant",
                "abilities_grant",
                "Grant an ability asset to an entity.",
                &GRANT_PARAMS,
                "{ grant } or { grant: null, cause }",
                Play,
            ),
            grant_method,
        ),
        (
            schema(
                "abilities.revoke",
                "abilities_revoke",
                "Remove a grant (cancelling a run, or after it with defer).",
                &REVOKE_PARAMS,
                "{ revoked }",
                Play,
            ),
            revoke,
        ),
        (
            schema(
                "abilities.activate",
                "abilities_activate",
                "Start a grant; a refusal is a result with its cause.",
                &ACTIVATE_PARAMS,
                "{ started, cause?, tag?, attribute?, state }",
                Play,
            ),
            activate,
        ),
        (
            schema(
                "abilities.charge",
                "abilities_charge",
                "Pay cost and cooldown of a running Explicit grant.",
                &ENTITY_GRANT,
                "{ charged, cause? }",
                Play,
            ),
            charge,
        ),
        (
            schema(
                "abilities.end",
                "abilities_end",
                "End a run (ready at the next step).",
                &ENTITY_GRANT,
                "{ state }",
                Play,
            ),
            end,
        ),
        (
            schema(
                "abilities.cancel",
                "abilities_cancel",
                "Cancel a run.",
                &ENTITY_GRANT,
                "{ cancelled, state }",
                Play,
            ),
            cancel,
        ),
        (
            schema(
                "abilities.signal",
                "abilities_signal",
                "Send a signal to an entity's Signal triggers now.",
                &SIGNAL_PARAMS,
                "{ started: [grant] }",
                Play,
            ),
            signal,
        ),
        (
            schema(
                "abilities.apply_effect",
                "abilities_apply_effect",
                "Apply an effect asset to a target.",
                &APPLY_PARAMS,
                "{ outcome, id?, stacks? }",
                Play,
            ),
            apply_effect,
        ),
        (
            schema(
                "abilities.remove_effect",
                "abilities_remove_effect",
                "Remove a live effect by id or by kind-tag query.",
                &REMOVE_PARAMS,
                "{ removed }",
                Play,
            ),
            remove_effect,
        ),
        (
            schema(
                "abilities.set_base",
                "abilities_set_base",
                "Set an attribute's base (clamped).",
                &SET_BASE_PARAMS,
                "{ old, new }",
                EditScene,
            ),
            set_base,
        ),
        (
            schema(
                "abilities.add_attributes",
                "abilities_add_attributes",
                "Give an entity the attributes of an .attrs.ron asset.",
                &ADD_PARAMS,
                "{ attributes }",
                EditScene,
            ),
            add_attributes,
        ),
        (
            schema(
                "abilities.cues",
                "abilities_cues",
                "Recent cue events from a ring of 256.",
                &CUES_PARAMS,
                "{ available, cues: [{ tick, entity, tag, kind, source, amount, effect }] }",
                Read,
            ),
            cues,
        ),
        (
            schema(
                "abilities.validate",
                "abilities_validate",
                "Validate an ability-system asset file.",
                &VALIDATE_PARAMS,
                "{ available, ok, diagnostics: [{ code, severity, message, span }] }",
                Read,
            ),
            validate,
        ),
        (
            schema(
                "abilities.stats",
                "abilities_stats",
                "Budget counters: entities, live effects, applications, removals (and ended effects by reason), modifier evaluations; asset_loads lists what the startup loader did per registered asset path (from the pak's cooked form or the loose RON, error codes).",
                &[],
                "{ available, tick, entities, live_effects, applications, removals, modifier_evaluations, entities_skipped, signals_unmatched, warnings, ended, asset_loads: [{ path, cooked, loaded, errors }] }",
                Read,
            ),
            stats,
        ),
        (
            schema(
                "abilities.state_hash",
                "abilities_state_hash",
                "A hash of the abilities state two platforms agree on: counts exact, attributes on a 1e-4 grid.",
                &[],
                "{ available, hash, quantum }",
                Read,
            ),
            state_hash,
        ),
    ];
    for (schema, handler) in rows {
        extension.register(schema, handler);
    }
    world.insert_resource(extension);
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::abilities::{
        AbilityAsset, AbilityPlugin, Attribute, EffectAsset, Lifetime, Magnitude, ModOp, Modifier,
    };

    fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let handler = world
            .resource::<AgentExtension>()
            .method(method)
            .map(|(_, handler)| handler)
            .expect("registered");
        handler(world, &params)
    }

    fn reason(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .unwrap_or_default()
            .to_owned()
    }

    fn step(app: &mut App) {
        let before = app.world().resource::<AbilityCounters>().tick;
        for _ in 0..4 {
            app.update();
            if app.world().resource::<AbilityCounters>().tick > before {
                return;
            }
        }
        panic!("no fixed step");
    }

    fn modifier(attr: &str, op: ModOp, amount: f32) -> Modifier {
        Modifier {
            attribute: attr.to_owned(),
            op,
            amount: Magnitude::Const(amount),
            per_stack: false,
            when: None,
        }
    }

    fn fire_bolt_app() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        enable_fixed_step(&mut app, 60.0);
        app.add_plugins(AbilityPlugin::default());
        install_ability_methods(app.world_mut());
        let mut cost = EffectAsset::new(Lifetime::Instant);
        cost.base_edits = vec![modifier("mana", ModOp::AddPre, -10.0)];
        let mut cd = EffectAsset::new(Lifetime::Timed(Magnitude::Const(0.5)));
        cd.status_tags = vec![Tag::new("Cooldown.FireBolt").expect("tag")];
        let mut burn = EffectAsset::new(Lifetime::Timed(Magnitude::Const(1.0)));
        burn.live_mods = vec![
            modifier("armor", ModOp::AddPre, -2.0),
            modifier("armor", ModOp::Scale, 0.5),
        ];
        burn.cues = vec![ab::CueBinding {
            tag: Tag::new("Cue.Burn").expect("tag"),
            at: ab::CueAt::Sustained,
            params: ab::CueParams::default(),
        }];
        let mut bolt = AbilityAsset::new("fire_bolt");
        bolt.cost = Some("effects/mana_10.effect.ron".into());
        bolt.cooldown = Some("effects/cd_fire_bolt.effect.ron".into());
        bolt.max_run_ticks = Some(2);
        {
            let mut assets = app.world_mut().resource_mut::<AbilityAssets>();
            assets.insert_effect("effects/mana_10.effect.ron", cost);
            assets.insert_effect("effects/cd_fire_bolt.effect.ron", cd);
            assets.insert_effect("effects/burn.effect.ron", burn);
            assets.insert_ability("abilities/fire_bolt.ability.ron", bolt);
        }
        let hero = app
            .world_mut()
            .spawn(
                Attributes::from_list([
                    Attribute::new("mana", 50.0, 0.0, 100.0),
                    Attribute::new("armor", 10.0, 0.0, 100.0),
                ])
                .expect("attrs"),
            )
            .id();
        step(&mut app);
        (app, hero)
    }

    #[test]
    fn stats_list_the_startup_loads_through_the_dispatcher() {
        let (mut app, _) = fire_bolt_app();
        let no_loads = crate::dispatch::call_in_process(app.world_mut(), "abilities.stats", json!({}))
            .expect("answers at once")
            .expect("stats");
        assert_eq!(no_loads["asset_loads"], json!([]));
        let mut registrations = ab::AbilityAssetRegistrations::default();
        registrations.add("effects/gone.effect.ron");
        app.world_mut().insert_resource(registrations);
        ab::load_registered_ability_assets(app.world_mut());
        let v = crate::dispatch::call_in_process(app.world_mut(), "abilities.stats", json!({}))
            .expect("answers at once")
            .expect("stats");
        assert_eq!(
            v["asset_loads"],
            json!([{ "path": "effects/gone.effect.ron", "cooked": false, "loaded": false, "errors": ["asset_not_found"] }]),
            "{v}"
        );
    }

    #[test]
    fn methods_answer_unavailable_without_the_plugin() {
        let mut world = World::new();
        install_ability_methods(&mut world);
        for method in METHODS {
            let v = call(&mut world, method, json!({})).expect("ok");
            assert_eq!(v, json!({ "available": false }), "{method}");
        }
        let ext = world.resource::<AgentExtension>();
        for (method, perm) in [
            ("abilities.list", "read"),
            ("abilities.grant", "play"),
            ("abilities.set_base", "edit_scene"),
            ("abilities.validate", "read"),
        ] {
            assert_eq!(ext.method(method).expect("m").0.permission.name(), perm);
        }
    }

    /// ADR-1809: a malformed pick, level or input code is refused, never coerced.
    #[test]
    fn malformed_picks_levels_and_input_codes_are_refused() {
        let (mut app, hero) = fire_bolt_app();
        let id = hero.to_bits();
        let w = app.world_mut();
        let ability = "abilities/fire_bolt.ability.ron";
        for (field, value) in [
            ("level", json!(3.5)),
            ("level", json!(-1)),
            ("level", json!(4_294_967_296_u64)),
            ("input_code", json!(99_999)),
            ("input_code", json!(1.5)),
        ] {
            let mut params = json!({ "entity": id, "ability": ability });
            params[field] = value.clone();
            let err = call(w, "abilities.grant", params).expect_err("refused");
            assert_eq!(reason(&err), "bad_value", "{field} = {value}");
        }
        for level in [json!(3.5), json!(-1), json!(4_294_967_296_u64)] {
            let err = call(
                w,
                "abilities.apply_effect",
                json!({ "target": id, "effect": "effects/nope.effect.ron", "level": level }),
            )
            .expect_err("level");
            assert_eq!(reason(&err), "bad_value", "{level}");
        }
        let g = call(
            w,
            "abilities.grant",
            json!({ "entity": id, "ability": ability, "level": 3, "input_code": 65_535 }),
        )
        .expect("grant")["grant"]
            .as_u64()
            .expect("id");
        for picks in [
            json!([{ "point": [1, 2] }]),
            json!([{ "point": [1e40, 0, 0] }]),
            json!([{ "point": [1, "x", 0] }]),
            json!(["self"]),
            json!({ "point": [0, 0, 0] }),
        ] {
            let err = call(
                w,
                "abilities.activate",
                json!({ "entity": id, "grant": g, "picks": picks }),
            )
            .expect_err("refused");
            assert_eq!(reason(&err), "bad_value", "{picks}");
        }
        let err = call(
            w,
            "abilities.activate",
            json!({ "entity": id, "grant": g, "picks": [123_456_789] }),
        )
        .expect_err("not an entity");
        assert_eq!(reason(&err), "entity_not_found");
        call(
            w,
            "abilities.activate",
            json!({ "entity": id, "grant": g, "picks": [id, { "point": [1, 2, 3] }] }),
        )
        .expect("valid picks");
    }

    #[test]
    fn fire_bolt_walkthrough() {
        let (mut app, hero) = fire_bolt_app();
        let id = hero.to_bits();
        let w = app.world_mut();
        let g = call(
            w,
            "abilities.grant",
            json!({ "entity": id, "ability": "abilities/fire_bolt.ability.ron" }),
        )
        .expect("grant")["grant"]
            .as_u64()
            .expect("id");
        let v = call(w, "abilities.activate", json!({ "entity": id, "grant": g })).expect("activate");
        assert_eq!(v["started"], true);
        assert_eq!(v["state"], "running");
        let v = call(w, "abilities.check", json!({ "entity": id, "grant": g })).expect("check");
        assert_eq!(v["cause"], "already_running");
        step(&mut app);
        step(&mut app);
        step(&mut app);
        let w = app.world_mut();
        let v = call(w, "abilities.check", json!({ "entity": id, "grant": g })).expect("check");
        assert_eq!(v["ok"], false);
        assert_eq!(v["cause"], "on_cooldown");
        assert_eq!(v["tag"], "Cooldown.FireBolt");
        let v = call(
            w,
            "abilities.apply_effect",
            json!({ "target": id, "effect": "effects/burn.effect.ron", "source": id }),
        )
        .expect("apply");
        assert_eq!(v["outcome"], "applied");
        step(&mut app);
        let w = app.world_mut();
        let v = call(
            w,
            "abilities.inspect",
            json!({ "entity": id, "attribute": "armor" }),
        )
        .expect("inspect");
        let b = &v["breakdown"][0];
        assert_eq!(b["phases"][0]["value_after"], 8.0);
        assert_eq!(b["phases"][2]["value_after"], 4.0);
        assert_eq!(b["phases"][0]["modifiers"][0]["op"], "add_pre");
        assert_eq!(b["current"], 4.0);
        assert_eq!(v["attributes"][0]["current"], 4.0);
        let v = call(w, "abilities.inspect", json!({ "entity": id })).expect("inspect");
        assert_eq!(v["attributes"][0]["base"], 40.0, "mana paid");
        assert_eq!(v["effects"].as_array().map(Vec::len), Some(2));
        let v = call(w, "abilities.cues", json!({})).expect("cues");
        assert_eq!(v["cues"][0]["tag"], "Cue.Burn");
        assert_eq!(v["cues"][0]["kind"], "begin");
        let hash = call(w, "abilities.state_hash", json!({})).expect("hash");
        assert_eq!(hash["available"], true);
        assert_eq!(hash["hash"].as_str().map(str::len), Some(16));
        let v = call(w, "abilities.stats", json!({})).expect("stats");
        assert_eq!(v["available"], true);
        assert!(v["applications"].as_u64().is_some_and(|n| n >= 3));
        let v = call(w, "abilities.list", json!({})).expect("list");
        assert_eq!(v["entities"][0]["grants"][0]["state"], "cooling");
        let v = call(
            w,
            "abilities.set_base",
            json!({ "entity": id, "attribute": "mana", "base": 500.0 }),
        )
        .expect("set");
        assert_eq!(v["new"], 100.0);
        let v = call(
            w,
            "abilities.remove_effect",
            json!({ "entity": id, "query": "Has(\"Status\")" }),
        )
        .expect("remove");
        assert_eq!(v["removed"], 0);
        let v = call(
            w,
            "abilities.signal",
            json!({ "entity": id, "tag": "Event.Nothing" }),
        )
        .expect("signal");
        assert_eq!(v["started"], json!([]));
        let v = call(w, "abilities.revoke", json!({ "entity": id, "grant": g })).expect("revoke");
        assert_eq!(v["revoked"], true);
    }

    /// `abilities.charge` pays a running grant once (a second charge is a no-op success) and is
    /// refused with `not_granted` when nothing runs; `abilities.end` moves a run to `ending`.
    #[test]
    fn a_full_book_refuses_the_next_grant_with_grant_limit() {
        let (mut app, hero) = fire_bolt_app();
        let id = hero.to_bits();
        let w = app.world_mut();
        let params = json!({ "entity": id, "ability": "abilities/fire_bolt.ability.ron" });
        for _ in 0..regia_framework::abilities::MAX_GRANTS_PER_BOOK {
            call(w, "abilities.grant", params.clone()).expect("room left");
        }
        let err = call(w, "abilities.grant", params).expect_err("book full");
        assert_eq!(reason(&err), "grant_limit");
        crate::contract::assert_listed("abilities.grant", "grant_limit");
    }

    #[test]
    fn charge_and_end_a_running_grant() {
        let (mut app, hero) = fire_bolt_app();
        let id = hero.to_bits();
        let w = app.world_mut();
        let g = call(
            w,
            "abilities.grant",
            json!({ "entity": id, "ability": "abilities/fire_bolt.ability.ron" }),
        )
        .expect("grant")["grant"]
            .as_u64()
            .expect("id");
        let v = call(w, "abilities.activate", json!({ "entity": id, "grant": g })).expect("activate");
        assert_eq!(v["started"], true);
        for _ in 0..2 {
            let v = call(w, "abilities.charge", json!({ "entity": id, "grant": g })).expect("charge");
            assert_eq!(v["charged"], true, "{v}");
        }
        let v = call(w, "abilities.end", json!({ "entity": id, "grant": g })).expect("end");
        assert_eq!(v["state"], "ending");
        let v = call(w, "abilities.charge", json!({ "entity": id, "grant": g })).expect("charge");
        assert_eq!(v["charged"], false);
        assert_eq!(v["cause"], "not_granted");
    }

    #[test]
    fn each_reason_once() {
        let (mut app, hero) = fire_bolt_app();
        let id = hero.to_bits();
        let w = app.world_mut();
        let bare = w.spawn_empty().id().to_bits();
        let mut stuck = AbilityAsset::new("stuck");
        stuck.cancelable = false;
        let mut dangling = AbilityAsset::new("dangling");
        dangling.cost = Some("nope.effect.ron".into());
        w.resource_mut::<AbilityAssets>().insert_ability("stuck", stuck);
        w.resource_mut::<AbilityAssets>()
            .insert_ability("dangling", dangling);
        let g =
            call(w, "abilities.grant", json!({ "entity": id, "ability": "stuck" })).expect("grant")["grant"]
                .clone();
        call(w, "abilities.activate", json!({ "entity": id, "grant": g })).expect("activate");
        let dir = regia_test_utils::unique_temp_dir("agent_abilities");
        let newer = dir.join("new.attrs.ron");
        std::fs::write(&newer, "(format_version: 9, attributes: [])").expect("write");
        let derived = dir.join("d.attrs.ron");
        std::fs::write(
            &derived,
            "(format_version: 1, attributes: [(name: \"a\", base: 1.0, min: 0.0, max: 9.0), (name: \"b\", base: 0.0, min: 0.0, max: 9.0, derived: Some((inputs: [(\"a\", 1.0)])))])",
        )
        .expect("write");
        let cases: Vec<(&str, Value, &str)> = vec![
            (
                "abilities.inspect",
                json!({ "entity": 999_999_999u64 }),
                "entity_not_found",
            ),
            (
                "abilities.set_base",
                json!({ "entity": bare, "attribute": "x", "base": 1.0 }),
                "no_attributes",
            ),
            (
                "abilities.set_base",
                json!({ "entity": id, "attribute": "nope", "base": 1.0 }),
                "unknown_attribute",
            ),
            (
                "abilities.set_base",
                json!({ "entity": id, "attribute": "mana", "base": 1e39 }),
                "attribute_not_finite",
            ),
            (
                "abilities.apply_effect",
                json!({ "target": id, "effect": "missing" }),
                "effect_not_found",
            ),
            (
                "abilities.check",
                json!({ "entity": id, "grant": 77 }),
                "grant_not_found",
            ),
            (
                "abilities.grant",
                json!({ "entity": id, "ability": "missing" }),
                "asset_not_found",
            ),
            (
                "abilities.cancel",
                json!({ "entity": id, "grant": g }),
                "not_cancelable",
            ),
            (
                "abilities.remove_effect",
                json!({ "entity": id, "query": "Has(" }),
                "bad_query",
            ),
            (
                "abilities.add_attributes",
                json!({ "entity": id, "asset": newer.to_string_lossy() }),
                "format_too_new",
            ),
            (
                "abilities.grant",
                json!({ "entity": id, "ability": "dangling" }),
                "reference_missing",
            ),
        ];
        for (method, params, why) in cases {
            let err = call(w, method, params.clone()).expect_err(method);
            assert_eq!(reason(&err), why, "{method} {params}");
        }
        let v = call(
            w,
            "abilities.add_attributes",
            json!({ "entity": bare, "asset": derived.to_string_lossy() }),
        )
        .expect("add");
        assert_eq!(v["attributes"], 2);
        let err = call(
            w,
            "abilities.set_base",
            json!({ "entity": bare, "attribute": "b", "base": 1.0 }),
        )
        .expect_err("derived");
        assert_eq!(reason(&err), "attribute_derived");
        let v = call(
            w,
            "abilities.validate",
            json!({ "path": newer.to_string_lossy() }),
        )
        .expect("validate");
        assert_eq!(v["diagnostics"][0]["code"], "format_too_new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// w10-editor-agent-smalls (1): an unloaded attribute set is read from the project's `assets/`
    /// folder like the abilities panel reads it, spelled bare (`attrs/x.attrs.ron`) or with the
    /// `assets/` prefix; the key the world records stays as given.
    #[test]
    fn add_attributes_reads_unloaded_sets_from_the_project_assets_folder() {
        let (mut app, _hero) = fire_bolt_app();
        let dir = regia_test_utils::unique_temp_dir("agent_abilities_assets");
        std::fs::create_dir_all(dir.join("assets/attrs")).expect("dirs");
        std::fs::write(
            dir.join("assets/attrs/x.attrs.ron"),
            "(format_version: 1, attributes: [(name: \"a\", base: 1.0, min: 0.0, max: 9.0)])",
        )
        .expect("write");
        let w = app.world_mut();
        w.insert_resource(crate::AgentSettings {
            project_dir: Some(dir.clone()),
        });
        for (key, expected) in [("attrs/x.attrs.ron", 1), ("assets/attrs/x.attrs.ron", 1)] {
            let e = w.spawn_empty().id().to_bits();
            let v = call(
                w,
                "abilities.add_attributes",
                json!({ "entity": e, "asset": key }),
            )
            .unwrap_or_else(|err| panic!("{key}: {err:?}"));
            assert_eq!(v["attributes"], expected, "{key}");
            assert!(w.resource::<AbilityAssets>().attributes(key).is_some(), "{key}");
        }
        // A file only at the project root (the old, wrong place) is not found.
        std::fs::write(
            dir.join("root_only.attrs.ron"),
            "(format_version: 1, attributes: [])",
        )
        .expect("write");
        let e = w.spawn_empty().id().to_bits();
        let err = call(
            w,
            "abilities.add_attributes",
            json!({ "entity": e, "asset": "root_only.attrs.ron" }),
        )
        .expect_err("root only");
        assert_eq!(reason(&err), "asset_not_found");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn book_names_every_method_reason_cause_and_outcome() {
        let book = include_str!("../../../book/agents/abilities.md");
        for name in METHODS.iter().copied().chain([
            "entity_not_found",
            "no_attributes",
            "unknown_attribute",
            "attribute_derived",
            "attribute_not_finite",
            "effect_not_found",
            "grant_not_found",
            "asset_not_found",
            "not_cancelable",
            "bad_query",
            "format_too_new",
            "reference_missing",
            "not_granted",
            "owner_locked",
            "input_blocked",
            "level_zero",
            "already_running",
            "owner_missing_tag",
            "owner_forbidden_tag",
            "blocked_by_ability",
            "on_cooldown",
            "cannot_afford",
            "target_rejected",
            "pick_timeout",
            "applied",
            "stacked",
            "failed_chance",
            "blocked_condition",
            "blocked_immunity",
            "rejected_full",
            "spilled",
            "limit_reached",
            "chain_too_deep",
        ]) {
            assert!(
                book.contains(&format!("`{name}`")),
                "book/agents/abilities.md lacks `{name}`"
            );
        }
    }

    /// An `Explicit` ability pays only when `abilities.charge` is called; a charge is idempotent within a run,
    /// refused when the run is not going or the owner cannot pay; `abilities.end` moves the run to `ending`.
    #[test]
    fn explicit_charge_and_end_drive_a_run() {
        let (mut app, hero) = fire_bolt_app();
        let id = hero.to_bits();
        let mut drain = AbilityAsset::new("drain");
        drain.cost = Some("effects/mana_10.effect.ron".into());
        drain.charge = ab::ChargeMode::Explicit;
        app.world_mut()
            .resource_mut::<AbilityAssets>()
            .insert_ability("abilities/drain.ability.ron", drain);
        let mana = |w: &mut World| {
            call(w, "abilities.inspect", json!({ "entity": id })).expect("inspect")["attributes"][0]["base"]
                .as_f64()
        };
        let w = app.world_mut();
        let g = call(
            w,
            "abilities.grant",
            json!({ "entity": id, "ability": "abilities/drain.ability.ron" }),
        )
        .expect("grant")["grant"]
            .as_u64()
            .expect("id");
        let v = call(w, "abilities.charge", json!({ "entity": id, "grant": g })).expect("charge");
        assert_eq!(v["charged"], false, "no run to charge");
        assert_eq!(v["cause"], "not_granted");
        let v = call(w, "abilities.activate", json!({ "entity": id, "grant": g })).expect("activate");
        assert_eq!(v["started"], true);
        assert_eq!(mana(w), Some(50.0), "an explicit run does not pay on start");
        let v = call(w, "abilities.charge", json!({ "entity": id, "grant": g })).expect("charge");
        assert_eq!(v, json!({ "charged": true }));
        assert_eq!(mana(w), Some(40.0));
        let v = call(w, "abilities.charge", json!({ "entity": id, "grant": g })).expect("again");
        assert_eq!(v, json!({ "charged": true }));
        assert_eq!(
            mana(w),
            Some(40.0),
            "a second charge in the same run pays nothing"
        );
        let v = call(w, "abilities.end", json!({ "entity": id, "grant": g })).expect("end");
        assert_eq!(v["state"], "ending");
        step(&mut app);
        let w = app.world_mut();
        assert_eq!(
            state_code(w, hero, GrantId(u32::try_from(g).expect("u32"))),
            "ready"
        );
        let v = call(w, "abilities.activate", json!({ "entity": id, "grant": g })).expect("activate");
        assert_eq!(v["started"], true);
        // The owner can pay at the start; the mana is gone by the time the driver charges.
        call(
            w,
            "abilities.set_base",
            json!({ "entity": id, "attribute": "mana", "base": 5.0 }),
        )
        .expect("set");
        let v = call(w, "abilities.charge", json!({ "entity": id, "grant": g })).expect("charge");
        assert_eq!(v["charged"], false);
        assert_eq!(v["cause"], "cannot_afford");
        assert_eq!(v["attribute"], "mana");
        assert_eq!(mana(w), Some(5.0));
        let err =
            call(w, "abilities.end", json!({ "entity": id, "grant": 999_999 })).expect_err("unknown grant");
        assert_eq!(reason(&err), "grant_not_found");
    }

    /// Spec 53 § 2.4: a target that despawns ends its live effects with `target_gone`; agents read
    /// it from `abilities.stats` (`ended`) and see the sustained cue end in `abilities.cues`.
    #[test]
    fn a_despawned_target_reports_target_gone_through_stats_and_cues() {
        let (mut app, _hero) = fire_bolt_app();
        let victim = app
            .world_mut()
            .spawn(Attributes::from_list([Attribute::new("armor", 10.0, 0.0, 100.0)]).expect("attrs"))
            .id();
        let id = victim.to_bits();
        let v = call(
            app.world_mut(),
            "abilities.apply_effect",
            json!({ "target": id, "effect": "effects/burn.effect.ron" }),
        )
        .expect("apply");
        assert_eq!(v["outcome"], "applied");
        step(&mut app);
        let since = app.world().resource::<AbilityCounters>().tick;
        app.world_mut().despawn(victim);
        step(&mut app);
        let w = app.world_mut();
        let v = call(w, "abilities.stats", json!({})).expect("stats");
        assert_eq!(v["ended"]["target_gone"], 1, "{v}");
        let v = call(w, "abilities.cues", json!({ "since_tick": since })).expect("cues");
        let ends: Vec<&Value> = v["cues"]
            .as_array()
            .map(|c| c.iter().filter(|c| c["kind"] == "end").collect())
            .unwrap_or_default();
        assert_eq!(ends.len(), 1, "{v}");
        assert_eq!(ends[0]["tag"], "Cue.Burn");
    }
}
