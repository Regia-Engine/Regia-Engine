// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for smart objects (`docs/specs/31-smart-objects.md`, `book/agents/smart-objects.md`):
//! `smartobject.list` (objects and their slots), `smartobject.claim` and `smartobject.release`.
//!
//! Read-only when there is no `regia_ai::SmartObjectPlugin`: the list is empty and claim/release
//! answer `NOT_FOUND`, so the protocol surface stays uniform.

use regia_ai::smart_object_load::SmartObjectDefs;
use regia_ai::smart_object_v2::release_handle_as;
use regia_ai::{
    SmartObject, SmartObjectError, SmartObjectIndex, SmartObjectSlot, begin_use, claim_with, release_handle,
    release_slot, smart_object_ancestor,
};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::GlobalTransform;
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Reads an entity parameter named `field` (a u64 bit pattern).
fn entity_field(params: &Value, field: &str) -> Result<Entity, RpcError> {
    let bits = params
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` (an entity id) is required")))?;
    Entity::try_from_bits(bits).ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))
}

/// `smartobject.list`: every [`SmartObject`] (entity, tags, enabled) and every [`SmartObjectSlot`]
/// (entity, owning object, role, enabled/claimed state, world position and facing), and how many
/// times the plugin has rebuilt the slot index (`null` without the plugin).
pub fn list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    // Spec 31: without SmartObjectPlugin the protocol stays uniform and lists nothing,
    // even if a slot component was spawned by hand.
    if world.get_resource::<SmartObjectIndex>().is_none() {
        return Ok(json!({
            "total": 0,
            "objects": [],
            "slots": [],
            "index_rebuilds": Value::Null,
        }));
    }
    let mut object_query = world.query::<(Entity, &SmartObject)>();
    let defs = world.get_resource::<SmartObjectDefs>();
    let objects: Vec<Value> = object_query
        .iter(world)
        .map(|(entity, object)| {
            let id = object.definition.as_ref().map(|reference| reference.id);
            let slot = id.and_then(|id| defs.and_then(|defs| defs.slot(id)));
            json!({
                "entity": entity.to_bits(),
                "tags": object.tags.clone(),
                "enabled": object.enabled,
                "definition": id.map(|id| id.to_string()),
                "definition_path": slot.and_then(|slot| slot.path.clone()),
                "definition_cooked": slot.is_some_and(|slot| slot.cooked),
                "definition_error": slot.and_then(|slot| slot.error.as_ref()).map(|error| {
                    json!({ "reason": error.reason, "detail": error.detail })
                }),
            })
        })
        .collect();

    let mut slot_query = world.query::<(Entity, &SmartObjectSlot, &GlobalTransform)>();
    let mut slots: Vec<Value> = Vec::new();
    for (entity, slot, transform) in slot_query.iter(world) {
        let object = smart_object_ancestor(world, entity).filter(|object| *object != entity);
        let position = transform.translation();
        let forward = transform.0.transform_vector3(Vec3::NEG_Z);
        slots.push(json!({
            "entity": entity.to_bits(),
            "object": object.map_or(Value::Null, |o| json!(o.to_bits())),
            "role": slot.role.clone(),
            "enabled": slot.enabled,
            "claimed": slot.claimed_by.is_some(),
            "claimed_by": slot.claimed_by.map_or(Value::Null, |holder| json!(holder.to_bits())),
            "position": [position.x, position.y, position.z],
            "forward": [forward.x, forward.y, forward.z],
        }));
    }
    let index_rebuilds = world
        .get_resource::<SmartObjectIndex>()
        .map(SmartObjectIndex::rebuilds);
    Ok(json!({
        "total": slots.len(),
        "objects": objects,
        "slots": slots,
        "index_rebuilds": index_rebuilds,
    }))
}

/// `smartobject.claim`: claims `slot` for `agent`, optionally at a `priority` (default `normal`) and
/// with `use: true` to begin using it at once. A slot held at an equal or higher priority, in use,
/// or disabled answers `claimed: false` with the current holder and a `reason`; a missing slot
/// answers `NOT_FOUND`. A successful claim returns the `handle` (`{ slot, generation }`).
pub fn claim(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let slot = entity_field(params, "slot")?;
    let agent = entity_field(params, "agent")?;
    let priority = crate::smart_object_v2::priority_param(params)?;
    let begin = crate::params::opt_bool(params, "use")?.unwrap_or(false);
    if world.get_resource::<SmartObjectIndex>().is_none() {
        return Err(RpcError::not_found(
            "no smart-object index: SmartObjectPlugin is not installed",
            "add regia_ai::SmartObjectPlugin",
        ));
    }
    let holder_of = |world: &World| {
        world
            .get::<SmartObjectSlot>(slot)
            .and_then(|s| s.claimed_by)
            .map_or(Value::Null, |holder| json!(holder.to_bits()))
    };
    match claim_with(world, slot, agent, priority) {
        Ok(handle) => {
            if begin {
                begin_use(world, handle).map_err(crate::smart_object_v2::error_refusal)?;
            }
            Ok(json!({
                "claimed": true,
                "held_by": agent.to_bits(),
                "handle": crate::smart_object_v2::handle_json(handle),
            }))
        }
        Err(SmartObjectError::SlotNotFound(_)) => Err(RpcError::not_found(
            "no such smart-object slot",
            "the `slot` entity must carry a regia_ai::SmartObjectSlot",
        )),
        Err(SmartObjectError::SlotDisabled(_)) => Ok(json!({
            "claimed": false,
            "held_by": Value::Null,
            "reason": "disabled",
        })),
        Err(SmartObjectError::SlotClaimed(_, holder)) => Ok(json!({
            "claimed": false,
            "held_by": holder.to_bits(),
            "reason": "claimed",
        })),
        Err(SmartObjectError::SlotInUse(_)) => Ok(json!({
            "claimed": false,
            "held_by": holder_of(world),
            "reason": "in_use",
        })),
        Err(
            SmartObjectError::NotClaimedBy(_, _)
            | SmartObjectError::StaleHandle
            | SmartObjectError::NotHolder,
        ) => Err(RpcError::internal("claim_with returned a release error")),
    }
}

/// `smartobject.release`: releases `slot` held by `agent`, or the claim named by `handle`.
/// Releasing a slot another agent holds answers `released: false` with the current holder; a missing
/// slot answers `NOT_FOUND`; a stale handle is `F0002` with reason `stale_handle`. A `handle` with an
/// `agent` releases only that agent's claim (`F0002` reason `not_holder` otherwise).
pub fn release(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(handle) = params.get("handle").filter(|v| !v.is_null()) {
        let handle = crate::smart_object_v2::handle_from(handle)?;
        if world.get_resource::<SmartObjectIndex>().is_none() {
            return Err(RpcError::not_found(
                "no smart-object index: SmartObjectPlugin is not installed",
                "add regia_ai::SmartObjectPlugin",
            ));
        }
        // With an `agent` too, only that agent's claim is released (`not_holder` otherwise;
        // spec 39 § 10 lists the reason, and a handle alone cannot name who asks).
        let result = match params.get("agent").filter(|v| !v.is_null()) {
            Some(_) => {
                let agent = entity_field(params, "agent")?;
                release_handle_as(world, handle, agent)
            }
            None => release_handle(world, handle),
        };
        return match result {
            Ok(()) => Ok(json!({ "released": true })),
            Err(error) => Err(crate::smart_object_v2::error_refusal(error)),
        };
    }
    let slot = entity_field(params, "slot")?;
    let agent = entity_field(params, "agent")?;
    if world.get_resource::<SmartObjectIndex>().is_none() {
        return Err(RpcError::not_found(
            "no smart-object index: SmartObjectPlugin is not installed",
            "add regia_ai::SmartObjectPlugin",
        ));
    }
    match release_slot(world, slot, agent) {
        Ok(()) => Ok(json!({ "released": true })),
        Err(SmartObjectError::SlotNotFound(_)) => Err(RpcError::not_found(
            "no such smart-object slot",
            "the `slot` entity must carry a regia_ai::SmartObjectSlot",
        )),
        Err(SmartObjectError::NotClaimedBy(_, holder)) => {
            Ok(json!({ "released": false, "held_by": holder.to_bits() }))
        }
        Err(_) => Err(RpcError::internal("release_slot returned an unexpected error")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ai::SmartObjectPlugin;
    use regia_core::math::Affine3A;

    fn app_with_object() -> (App, Entity, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SmartObjectPlugin));
        let object = app.world_mut().spawn(SmartObject::default()).id();
        let slot = app
            .world_mut()
            .spawn((
                SmartObjectSlot::new("sit"),
                GlobalTransform(Affine3A::from_translation(Vec3::new(1.0, 0.0, 0.0))),
                ChildOf(object),
            ))
            .id();
        (app, object, slot)
    }

    #[test]
    fn list_reports_objects_and_slots() {
        let (mut app, object, slot) = app_with_object();
        let result = list(app.world_mut(), &json!({})).expect("lists");
        assert_eq!(result["total"], 1);
        assert_eq!(result["objects"][0]["entity"], object.to_bits());
        let slots = result["slots"].as_array().expect("slots");
        assert_eq!(slots[0]["entity"], slot.to_bits());
        assert_eq!(slots[0]["role"], "sit");
        assert_eq!(slots[0]["object"], object.to_bits());
    }

    /// Two reads either side of a quiet frame agree; a claim moves the count on the next frame.
    #[test]
    fn list_counts_index_rebuilds() {
        let (mut app, _object, slot) = app_with_object();
        app.update();
        app.update();
        let rebuilds =
            |app: &mut App| list(app.world_mut(), &json!({})).expect("lists")["index_rebuilds"].as_u64();
        let settled = rebuilds(&mut app).expect("the plugin keeps a count");
        app.update();
        assert_eq!(rebuilds(&mut app), Some(settled));
        let agent = app.world_mut().spawn_empty().id();
        claim(
            app.world_mut(),
            &json!({ "slot": slot.to_bits(), "agent": agent.to_bits() }),
        )
        .expect("claims");
        app.update();
        assert_eq!(rebuilds(&mut app), Some(settled + 1));
    }

    #[test]
    fn claim_and_release_round_trip() {
        let (mut app, _object, slot) = app_with_object();
        let agent = app.world_mut().spawn_empty().id();

        let claimed = claim(
            app.world_mut(),
            &json!({ "slot": slot.to_bits(), "agent": agent.to_bits() }),
        )
        .expect("claims");
        assert_eq!(claimed["claimed"], true);

        let released = release(
            app.world_mut(),
            &json!({ "slot": slot.to_bits(), "agent": agent.to_bits() }),
        )
        .expect("releases");
        assert_eq!(released["released"], true);
    }

    /// Spec 31 § 4: a refused claim answers `{ claimed: false, held_by, reason }` (another holder,
    /// a disabled slot), a release by a non-holder answers `{ released: false, held_by }` and the
    /// claim survives, and `list` reports the facing of a rotated slot (its local −Z in world space).
    #[test]
    fn refusals_name_the_holder_and_list_reports_the_facing() {
        let (mut app, _object, slot) = app_with_object();
        let first = app.world_mut().spawn_empty().id();
        let second = app.world_mut().spawn_empty().id();
        let ask = |agent: Entity| json!({ "slot": slot.to_bits(), "agent": agent.to_bits() });

        assert_eq!(
            claim(app.world_mut(), &ask(first)).expect("claims")["claimed"],
            true
        );
        let refused = claim(app.world_mut(), &ask(second)).expect("an answer, not an error");
        assert_eq!(refused["claimed"], false);
        assert_eq!(refused["held_by"], first.to_bits());
        assert_eq!(refused["reason"], "claimed");

        let kept = release(app.world_mut(), &ask(second)).expect("an answer, not an error");
        assert_eq!(kept["released"], false);
        assert_eq!(kept["held_by"], first.to_bits());
        assert_eq!(
            app.world()
                .get::<SmartObjectSlot>(slot)
                .and_then(|s| s.claimed_by),
            Some(first)
        );
        assert_eq!(
            release(app.world_mut(), &ask(first)).expect("releases")["released"],
            true
        );

        app.world_mut()
            .get_mut::<SmartObjectSlot>(slot)
            .expect("slot")
            .enabled = false;
        let disabled = claim(app.world_mut(), &ask(first)).expect("an answer, not an error");
        assert_eq!(disabled["claimed"], false);
        assert_eq!(disabled["held_by"], Value::Null);
        assert_eq!(disabled["reason"], "disabled");

        // A quarter turn about +Y turns local −Z into world −X.
        let turned = Affine3A::from_rotation_translation(
            regia_core::math::Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
            Vec3::new(1.0, 0.0, 0.0),
        );
        app.world_mut().entity_mut(slot).insert(GlobalTransform(turned));
        let listed = list(app.world_mut(), &json!({})).expect("lists");
        let forward = &listed["slots"][0]["forward"];
        let axis = |i: usize| forward[i].as_f64().expect("number");
        assert!((axis(0) + 1.0).abs() < 1e-5, "{forward}");
        assert!(axis(1).abs() < 1e-5 && axis(2).abs() < 1e-5, "{forward}");
        assert_eq!(listed["slots"][0]["position"], json!([1.0, 0.0, 0.0]));
    }

    #[test]
    fn claim_of_a_missing_slot_is_not_found() {
        let (mut app, _object, _slot) = app_with_object();
        let agent = app.world_mut().spawn_empty().id();
        let error = claim(
            app.world_mut(),
            &json!({ "slot": Entity::PLACEHOLDER.to_bits(), "agent": agent.to_bits() }),
        )
        .expect_err("no slot");
        assert!(error.message.contains("smart-object slot"), "{}", error.message);
    }

    #[test]
    fn without_a_plugin_the_list_is_empty() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let result = list(app.world_mut(), &json!({})).expect("empty list");
        assert_eq!(result["total"], 0);
        assert_eq!(result["objects"].as_array().expect("objects").len(), 0);
    }

    /// A slot component without SmartObjectPlugin is not claimable (spec 31).
    #[test]
    fn claim_without_plugin_is_not_found() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let slot = app
            .world_mut()
            .spawn((
                SmartObjectSlot::new("sit"),
                GlobalTransform(Affine3A::from_translation(Vec3::ZERO)),
            ))
            .id();
        let agent = app.world_mut().spawn_empty().id();
        let listed = list(app.world_mut(), &json!({})).expect("empty");
        assert_eq!(listed["total"], 0);
        assert!(listed["objects"].as_array().is_some_and(|o| o.is_empty()));
        assert!(listed["slots"].as_array().is_some_and(|s| s.is_empty()));
        let bad = claim(app.world_mut(), &json!({})).expect_err("params first");
        assert_eq!(bad.code, crate::protocol::INVALID_PARAMS);
        let params = json!({ "slot": slot.to_bits(), "agent": agent.to_bits() });
        let err = claim(app.world_mut(), &params).expect_err("no plugin");
        assert_eq!(err.code, crate::protocol::NOT_FOUND);
        assert_eq!(err.data.as_ref().and_then(|d| d["code"].as_str()), Some("F0003"));
        let err = release(app.world_mut(), &params).expect_err("no plugin");
        assert_eq!(err.code, crate::protocol::NOT_FOUND);
        assert!(
            app.world()
                .get::<SmartObjectSlot>(slot)
                .is_some_and(|s| s.claimed_by.is_none())
        );
    }
}
