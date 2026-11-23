// SPDX-License-Identifier: MPL-2.0
//! Per-object shadow LOD hints on the agent protocol (`book/agents/shadow-lod-hint.md`,
//! ADR-1622): `render.shadow_lod_hints` lists the entities with a [`ShadowLodHint`] and
//! `render.set_shadow_lod_hint` sets, changes or clears one.

use regia_app::prelude::*;
use regia_render::{MeshLodChain, ShadowLodHint};
use serde_json::{Value, json};

use crate::protocol::RpcError;
use crate::scene::entity_from;

fn hint_json(world: &World, entity: Entity, hint: &ShadowLodHint) -> Value {
    let levels = world.get::<MeshLodChain>(entity).map(|chain| chain.levels.len());
    json!({
        "entity": entity.to_bits(),
        "max_lod": (hint.max_lod != u8::MAX).then_some(hint.max_lod),
        "min_lod": hint.min_lod,
        "neutral": hint.is_neutral(),
        "chain_levels": levels,
    })
}

fn state(world: &mut World) -> Value {
    let mut query = world.query::<(Entity, &ShadowLodHint)>();
    let found: Vec<(Entity, ShadowLodHint)> = query.iter(world).map(|(e, h)| (e, *h)).collect();
    let mut entities: Vec<Value> = found
        .iter()
        .map(|(entity, hint)| hint_json(world, *entity, hint))
        .collect();
    entities.sort_by_key(|e| e["entity"].as_u64());
    json!({ "entities": entities })
}

/// `render.shadow_lod_hints`: every entity with a shadow LOD hint.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    Ok(state(world))
}

/// A LOD index parameter: `None` when absent, `Some(None)` for `null` (no bound).
fn lod_param(params: &Value, name: &str) -> Result<Option<Option<u8>>, RpcError> {
    match params.get(name) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(value) => value
            .as_u64()
            .filter(|lod| *lod < u64::from(u8::MAX))
            .map(|lod| Some(Some(lod as u8)))
            .ok_or_else(|| {
                RpcError::invalid_params(format!("`{name}` is a LOD index 0 to 254, or null for no bound"))
            }),
    }
}

/// `render.set_shadow_lod_hint { entity, max_lod?, min_lod?, clear? }`.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let max_lod = lod_param(params, "max_lod")?;
    let min_lod = lod_param(params, "min_lod")?;
    let clear = match params.get("clear") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`clear` must be a boolean"))?,
    };
    if !clear && max_lod.is_none() && min_lod.is_none() {
        return Err(RpcError::invalid_params(
            "give `max_lod` (coarsest shadow LOD), `min_lod` (finest shadow LOD) or `clear`",
        ));
    }
    let entity = entity_from(params)?;
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", entity.to_bits()),
            "use scene.query_entities (an entity with a mesh LOD chain)",
        ));
    }
    let before = world
        .get::<ShadowLodHint>(entity)
        .copied()
        .map(|hint| hint_json(world, entity, &hint));
    if clear {
        world.entity_mut(entity).remove::<ShadowLodHint>();
    } else {
        let mut hint = world.get::<ShadowLodHint>(entity).copied().unwrap_or_default();
        if let Some(max) = max_lod {
            hint.max_lod = max.unwrap_or(u8::MAX);
        }
        if let Some(min) = min_lod {
            hint.min_lod = min.unwrap_or(0);
        }
        world.entity_mut(entity).insert(hint);
    }
    let after = world
        .get::<ShadowLodHint>(entity)
        .copied()
        .map(|hint| hint_json(world, entity, &hint));
    let chained = world.get::<MeshLodChain>(entity).is_some();
    Ok(json!({
        "before": before,
        "after": after,
        "note": if chained {
            "applies from the next extracted frame: shadow draws use the selected shadow LOD \
             clamped to min_lod..=max_lod (min_lod wins a conflict) and to the chain's last level"
        } else {
            "stored, but the entity has no mesh LOD chain (MeshLodChain), so its shadow has only \
             one level and the hint changes nothing"
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_round_trip_and_clear() {
        let mut world = World::new();
        assert_eq!(get(&mut world).map(|v| v["entities"].clone()), Ok(json!([])));
        let entity = world.spawn_empty().id();
        let bits = entity.to_bits();
        let set_cap = set(&mut world, &json!({ "entity": bits, "max_lod": 1 })).map_err(|e| e.message);
        assert_eq!(set_cap.as_ref().map(|v| v["before"].is_null()), Ok(true));
        assert_eq!(world.get::<ShadowLodHint>(entity), Some(&ShadowLodHint::cap(1)));
        let set_floor = set(&mut world, &json!({ "entity": bits, "min_lod": 1 })).map_err(|e| e.message);
        assert_eq!(set_floor.map(|v| v["after"]["min_lod"].clone()), Ok(json!(1)));
        assert_eq!(world.get::<ShadowLodHint>(entity), Some(&ShadowLodHint::fixed(1)));
        let listed = get(&mut world).map(|v| v["entities"][0]["max_lod"].clone());
        assert_eq!(listed, Ok(json!(1)));
        assert!(set(&mut world, &json!({ "entity": bits, "max_lod": null })).is_ok());
        assert_eq!(
            world.get::<ShadowLodHint>(entity).map(|h| h.max_lod),
            Some(u8::MAX)
        );
        let cleared = set(&mut world, &json!({ "entity": bits, "clear": true })).map_err(|e| e.message);
        assert_eq!(cleared.map(|v| v["after"].is_null()), Ok(true));
        assert!(world.get::<ShadowLodHint>(entity).is_none());
    }

    #[test]
    fn bad_params_are_rejected() {
        let mut world = World::new();
        let entity = world.spawn_empty().id().to_bits();
        assert!(set(&mut world, &json!({ "entity": entity })).is_err());
        assert!(set(&mut world, &json!({ "entity": entity, "max_lod": -1 })).is_err());
        assert!(set(&mut world, &json!({ "entity": entity, "max_lod": 255 })).is_err());
        assert!(set(&mut world, &json!({ "entity": entity, "min_lod": "2" })).is_err());
        assert!(set(&mut world, &json!({ "entity": entity, "clear": 1 })).is_err());
        assert!(set(&mut world, &json!({ "max_lod": 1 })).is_err());
        assert!(set(&mut world, &json!({ "entity": u64::MAX, "max_lod": 1 })).is_err());
    }
}
