// SPDX-License-Identifier: MPL-2.0
//! Skinning on the agent protocol (`book/agents/skinning.md`, Tincal B2): `render.skinning`
//! reports the compute skinning path (app-wide switch, device support, what the last frame
//! skinned) and the entities with [`SkinningOptions`]; `render.set_skinning` flips the app-wide
//! switch ([`SkinningConfig`]) or sets, changes or clears one entity's options.

use regia_app::prelude::*;
use regia_render::{RuntimeRenderer, SkinningConfig, SkinningMethod, SkinningOptions, SkinningStats};
use serde_json::{Value, json};

use crate::protocol::RpcError;
use crate::scene::entity_from;

fn stats_json(stats: &SkinningStats) -> Value {
    json!({
        "instances": stats.instances,
        "dispatched": stats.dispatched,
        "vertices": stats.vertices,
        "dual_quaternion": stats.dual_quaternion,
        "eight_influences": stats.eight_influences,
        "influence_mismatch": stats.influence_mismatch,
        "resident": stats.resident,
        "options": stats.options,
        "counted": stats.counted,
    })
}

fn options_json(entity: Entity, options: &SkinningOptions) -> Value {
    json!({
        "entity": entity.to_bits(),
        "method": options.method.name(),
        "compute": options.compute,
        "influences8": options.influences8.as_ref().map(|t| t.len()),
        "on_compute_path": options.wants_compute(),
    })
}

fn state(world: &mut World) -> Value {
    let compute_all = world.get_resource::<SkinningConfig>().is_some_and(|c| c.compute);
    let stats = world
        .get_resource::<RuntimeRenderer>()
        .map(|r| r.0.skinning_stats());
    let mut query = world.query::<(Entity, &SkinningOptions)>();
    let mut entities: Vec<Value> = query
        .iter(world)
        .map(|(entity, options)| options_json(entity, options))
        .collect();
    entities.sort_by_key(|e| e["entity"].as_u64());
    json!({
        "compute_all": compute_all,
        "supported": stats.as_ref().map(|s| s.unsupported.is_none()),
        "unsupported": stats.as_ref().and_then(|s| s.unsupported.clone()),
        "last_frame": stats.as_ref().map(stats_json),
        "gpu_bytes": world.get_resource::<RuntimeRenderer>().map(|r| r.0.skinning_gpu_bytes()),
        "entities": entities,
    })
}

/// `render.skinning`: the app-wide switch, device support, the last frame's compute skinning
/// counts and every entity with options.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    Ok(state(world))
}

/// `render.set_skinning { compute_all? , entity?, method?, compute?, clear? }`.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let compute_all = params.get("compute_all").map(Value::as_bool);
    if compute_all == Some(None) {
        return Err(RpcError::invalid_params("`compute_all` must be a boolean"));
    }
    let has_entity = params.get("entity").is_some();
    if compute_all.is_none() && !has_entity {
        return Err(RpcError::invalid_params(
            "give `compute_all` (every skinned mesh on the compute path) or an `entity` to set options on",
        ));
    }
    let method = match params.get("method") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .and_then(SkinningMethod::from_name)
                .ok_or_else(|| RpcError::invalid_params("`method` is \"linear\" or \"dual_quaternion\""))?,
        ),
    };
    let compute = match params.get("compute") {
        None => None,
        Some(value) => Some(
            value
                .as_bool()
                .ok_or_else(|| RpcError::invalid_params("`compute` must be a boolean"))?,
        ),
    };
    let clear = crate::params::opt_bool(params, "clear")?.unwrap_or(false);
    let entity = if has_entity {
        let entity = entity_from(params)?;
        if world.get_entity(entity).is_err() {
            return Err(RpcError::not_found(
                format!("entity {} does not exist", entity.to_bits()),
                "use scene.query_entities (a skinned mesh, or the root of a character: children inherit)",
            ));
        }
        Some(entity)
    } else {
        if method.is_some() || compute.is_some() || clear {
            return Err(RpcError::invalid_params(
                "`method`, `compute` and `clear` apply to an `entity`",
            ));
        }
        None
    };
    let before = state(world);
    if let Some(Some(enabled)) = compute_all {
        world.insert_resource(SkinningConfig { compute: enabled });
    }
    if let Some(entity) = entity {
        if clear {
            world.entity_mut(entity).remove::<SkinningOptions>();
        } else {
            let mut options = world.get::<SkinningOptions>(entity).cloned().unwrap_or_default();
            if let Some(method) = method {
                options.method = method;
            }
            if let Some(compute) = compute {
                options.compute = compute;
            }
            world.entity_mut(entity).insert(options);
        }
    }
    let after = state(world);
    Ok(json!({
        "before": before,
        "after": after,
        "note": "applies from the next extracted frame; options on an entity cover its children \
                 (the eight-influence table excepted). Dual quaternions and eight influences need \
                 the compute path; a device without compute shaders draws linear, four influences",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_and_entity_options_round_trip() {
        let mut world = World::new();
        let got = get(&mut world).expect("get");
        assert_eq!(got["compute_all"], false);
        assert!(got["last_frame"].is_null(), "no renderer: no stats");
        let set_all = set(&mut world, &json!({ "compute_all": true })).expect("set");
        assert_eq!(set_all["after"]["compute_all"], true);
        assert!(world.resource::<SkinningConfig>().compute);
        let entity = world.spawn_empty().id();
        let bits = entity.to_bits();
        let dq = set(
            &mut world,
            &json!({ "entity": bits, "method": "dual_quaternion" }),
        )
        .expect("dq");
        assert_eq!(dq["after"]["entities"][0]["method"], "dual_quaternion");
        assert_eq!(dq["after"]["entities"][0]["on_compute_path"], true);
        assert_eq!(
            world.get::<SkinningOptions>(entity).map(|o| o.method),
            Some(SkinningMethod::DualQuaternion)
        );
        set(
            &mut world,
            &json!({ "entity": bits, "method": "lbs", "compute": true }),
        )
        .expect("alias");
        let options = world.get::<SkinningOptions>(entity).cloned().expect("options");
        assert_eq!((options.method, options.compute), (SkinningMethod::Linear, true));
        let cleared = set(&mut world, &json!({ "entity": bits, "clear": true })).expect("clear");
        assert_eq!(cleared["after"]["entities"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn bad_params_are_rejected() {
        let mut world = World::new();
        assert!(set(&mut world, &json!({})).is_err());
        assert!(set(&mut world, &json!({ "compute_all": 1 })).is_err());
        assert!(set(&mut world, &json!({ "method": "dual_quaternion" })).is_err());
        let entity = world.spawn_empty().id().to_bits();
        assert!(set(&mut world, &json!({ "entity": entity, "method": "spline" })).is_err());
        assert!(set(&mut world, &json!({ "entity": entity, "compute": "yes" })).is_err());
        assert!(set(&mut world, &json!({ "entity": u64::MAX })).is_err());
    }

    /// ADR-7721: `last_frame.counted` reports the instances skinned from a counted mesh's upload.
    #[test]
    fn last_frame_reports_counted_instances() {
        let stats = SkinningStats {
            instances: 3,
            counted: 2,
            ..SkinningStats::default()
        };
        let got = stats_json(&stats);
        assert_eq!(
            (got["instances"].as_u64(), got["counted"].as_u64()),
            (Some(3), Some(2)),
            "{got}"
        );
    }
}
