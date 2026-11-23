// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for smart objects v2 (`docs/specs/39-smart-objects-v2.md` § 10,
//! `book/agents/smart-objects.md`): `smartobject.search`, `smartobject.begin_use`,
//! `smartobject.set_enabled`, `smartobject.tags`, `smartobject.messages` and
//! `smartobject.set_definition`, plus the helpers the extended `smartobject.claim` /
//! `smartobject.release` share.
//!
//! Errors are `F0002` with `data.reason` (`slot_not_found`, `slot_claimed`, `slot_in_use`,
//! `slot_disabled`, `stale_handle`, `not_holder`, `entity_not_found`, `bad_region`, `bad_priority`,
//! `bad_query`, `tag_unknown`, `missing_param`, `definition_not_found`, `bad_id`). Without a `regia_ai::SmartObjectPlugin` searches and
//! the message log are empty and the rest answer `slot_not_found` / `entity_not_found`.

use regia_ai::smart_object_load::{SmartObjectDefs, load_definition};
use regia_ai::smart_object_v2::{
    ClaimHandle, ClaimPriority, HitStatus, QueryRegion, SlotQuery, SmartObjectEvent, SmartObjectEvents,
    add_runtime_tag, begin_use, remove_runtime_tag, set_enabled, slot_state,
};
use regia_ai::{RejectionLog, SmartObject, SmartObjectError, SmartObjectIndex, SmartObjectSlot};
use regia_app::prelude::*;
use regia_asset::{AssetId, AssetRef, AssetServer};
use regia_core::math::Vec3;
use regia_framework::tags::{Tag, TagContainer, TagQuery, TagRegistry};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// The most rejection entries `smartobject.search` reports with `explain`.
const MAX_REJECTIONS: usize = 256;

/// `F0002` with `data.reason`.
pub(crate) fn refused(reason: &str, message: impl Into<String>) -> RpcError {
    let mut error = RpcError::new(
        protocol::INVALID_PARAMS,
        "F0002",
        message,
        "see book/agents/smart-objects.md for the parameters and reasons",
    );
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn missing(param: &str) -> RpcError {
    refused("missing_param", format!("`{param}` is required"))
}

fn entity_param(params: &Value, field: &str) -> Result<Entity, RpcError> {
    let bits = params
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| missing(field))?;
    Entity::try_from_bits(bits)
        .ok_or_else(|| refused("entity_not_found", format!("{bits} is not an entity id")))
}

/// Reads an optional priority name; absent or null is `Normal`.
pub(crate) fn priority_param(params: &Value) -> Result<ClaimPriority, RpcError> {
    match params.get("priority") {
        None | Some(Value::Null) => Ok(ClaimPriority::Normal),
        Some(value) => value.as_str().and_then(ClaimPriority::from_name).ok_or_else(|| {
            refused(
                "bad_priority",
                "`priority` must be background, low, normal, high or urgent",
            )
        }),
    }
}

/// Reads a `{ "slot": bits, "generation": n }` handle.
pub(crate) fn handle_from(value: &Value) -> Result<ClaimHandle, RpcError> {
    let slot = entity_param(value, "slot")?;
    let generation = crate::params::opt_u64(value, "generation")?
        .and_then(|g| u32::try_from(g).ok())
        .ok_or_else(|| missing("generation"))?;
    Ok(ClaimHandle { slot, generation })
}

/// A handle as JSON.
pub(crate) fn handle_json(handle: ClaimHandle) -> Value {
    json!({ "slot": handle.slot.to_bits(), "generation": handle.generation })
}

/// Maps a smart-object error to the protocol's `F0002` reasons.
pub(crate) fn error_refusal(error: SmartObjectError) -> RpcError {
    match error {
        SmartObjectError::SlotNotFound(slot) => refused(
            "slot_not_found",
            format!("`{}` is not a smart-object slot", slot.to_bits()),
        ),
        SmartObjectError::SlotDisabled(_) => refused("slot_disabled", "the slot is disabled"),
        SmartObjectError::SlotClaimed(_, _) => {
            refused("slot_claimed", "the slot is held at an equal or higher priority")
        }
        SmartObjectError::NotClaimedBy(_, _) | SmartObjectError::NotHolder => {
            refused("not_holder", "the user does not hold the claim")
        }
        SmartObjectError::StaleHandle => refused("stale_handle", "the claim handle is stale"),
        SmartObjectError::SlotInUse(_) => refused("slot_in_use", "the slot is in use"),
    }
}

/// A tag by name, checked against the project tag table when one is loaded.
fn tag_param(world: &World, name: &str) -> Result<Tag, RpcError> {
    let tag =
        Tag::new(name).map_err(|e| refused("bad_query", format!("`{name}` is not a valid tag: {e}")))?;
    if let Some(registry) = world.get_resource::<TagRegistry>()
        && !registry.is_empty()
        && registry.get(name).is_none()
    {
        return Err(refused(
            "tag_unknown",
            format!("`{name}` is not declared in the tag table"),
        ));
    }
    Ok(tag)
}

fn tag_list(world: &World, params: &Value, field: &str) -> Result<Vec<Tag>, RpcError> {
    let Some(value) = params.get(field).filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| refused("bad_query", format!("`{field}` must be an array of tag names")))?;
    items
        .iter()
        .map(|item| {
            let name = item
                .as_str()
                .ok_or_else(|| refused("bad_query", format!("`{field}` must hold strings")))?;
            tag_param(world, name)
        })
        .collect()
}

fn vec3_of(value: &Value) -> Option<Vec3> {
    let items = value.as_array()?;
    if items.len() != 3 {
        return None;
    }
    let component = |i: usize| items[i].as_f64().map(|f| f as f32);
    Some(Vec3::new(component(0)?, component(1)?, component(2)?))
}

fn region_param(params: &Value) -> Result<QueryRegion, RpcError> {
    let region = params.get("region").ok_or_else(|| missing("region"))?;
    let bad = || {
        refused(
            "bad_region",
            "`region` is { center: [x,y,z], radius } or { min: [x,y,z], max: [x,y,z] }",
        )
    };
    let parsed = if let (Some(center), Some(radius)) =
        (region.get("center").and_then(vec3_of), region.get("radius"))
    {
        QueryRegion::Sphere {
            center,
            radius: radius.as_f64().ok_or_else(bad)? as f32,
        }
    } else if let (Some(min), Some(max)) = (
        region.get("min").and_then(vec3_of),
        region.get("max").and_then(vec3_of),
    ) {
        QueryRegion::Aabb { min, max }
    } else {
        return Err(bad());
    };
    if parsed.is_valid() { Ok(parsed) } else { Err(bad()) }
}

fn bool_param(params: &Value, field: &str, default: bool) -> Result<bool, RpcError> {
    Ok(crate::params::opt_bool(params, field)?.unwrap_or(default))
}

/// `smartobject.search`: slots matching a region and filters, nearest first, with optional
/// per-candidate rejection reasons (`explain`).
pub fn search(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut query = SlotQuery::new(region_param(params)?);
    if let Some(text) = params.get("activity").filter(|v| !v.is_null()) {
        let text = text
            .as_str()
            .ok_or_else(|| refused("bad_query", "`activity` must be a tag query as RON text"))?;
        let parsed = TagQuery::from_ron(text)
            .map_err(|e| refused("bad_query", format!("`activity` does not parse: {e}")))?;
        for tag in parsed.tags() {
            tag_param(world, tag.name())?;
        }
        query.activity = Some(parsed);
    }
    query.user_tags = tag_list(world, params, "user_tags")?
        .into_iter()
        .collect::<TagContainer>();
    if let Some(families) = params.get("families").filter(|v| !v.is_null()) {
        query.families = families
            .as_array()
            .ok_or_else(|| refused("bad_query", "`families` must be an array of strings"))?
            .iter()
            .map(|f| {
                f.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| refused("bad_query", "`families` must hold strings"))
            })
            .collect::<Result<_, _>>()?;
    }
    query.priority = priority_param(params)?;
    query.allow_preempt = bool_param(params, "allow_preempt", false)?;
    query.include_disabled = bool_param(params, "include_disabled", false)?;
    query.check_conditions = bool_param(params, "check_conditions", true)?;
    // A present `limit` or `user` of the wrong shape is refused, not read as "no limit" / "no
    // user" (muse L, ADR-1809).
    query.limit = match params.get("limit") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
                .ok_or_else(|| refused("bad_query", "`limit` must be a non-negative integer"))?,
        ),
    };
    query.user = match params.get("user") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let user = v
                .as_u64()
                .and_then(Entity::try_from_bits)
                .ok_or_else(|| refused("bad_query", "`user` must be an entity id (its bits)"))?;
            if world.get_entity(user).is_err() {
                return Err(refused("entity_not_found", "`user` must be a live entity id"));
            }
            Some(user)
        }
    };
    let explain = bool_param(params, "explain", false)?;

    let Some(index) = world.get_resource::<SmartObjectIndex>() else {
        return Ok(json!({ "hits": [] }));
    };
    let mut log = RejectionLog::default();
    let hits = index.search(world, &query, explain.then_some(&mut log));
    let hits: Vec<Value> = hits
        .iter()
        .map(|hit| {
            let mut out = json!({
                "slot": hit.slot.to_bits(),
                "object": hit.object.map_or(Value::Null, |o| json!(o.to_bits())),
                "distance": hit.distance,
                "status": match hit.status {
                    HitStatus::Free => "free",
                    HitStatus::Preemptable { .. } => "preemptable",
                },
            });
            if let (HitStatus::Preemptable { holder }, Some(map)) = (hit.status, out.as_object_mut()) {
                map.insert("holder".into(), json!(holder.to_bits()));
            }
            out
        })
        .collect();
    let mut result = json!({ "hits": hits });
    if explain && let Some(map) = result.as_object_mut() {
        let total = log.entries.len();
        let entries: Vec<Value> = log
            .entries
            .iter()
            .take(MAX_REJECTIONS)
            .map(|r| {
                json!({
                    "object": r.object.map_or(Value::Null, |o| json!(o.to_bits())),
                    "slot": r.slot.map_or(Value::Null, |s| json!(s.to_bits())),
                    "stage": r.stage.name(),
                    "detail": r.detail,
                })
            })
            .collect();
        map.insert("rejections".into(), json!(entries));
        map.insert("rejections_total".into(), json!(total));
    }
    Ok(result)
}

/// `smartobject.begin_use`: `Claimed -> InUse` for a claim handle.
pub fn begin_use_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let handle = handle_from(params.get("handle").ok_or_else(|| missing("handle"))?)?;
    begin_use(world, handle).map_err(error_refusal)?;
    let state = slot_state(world, handle.slot).map_or("free", |s| s.name());
    Ok(json!({ "state": state }))
}

/// `smartobject.set_enabled`: switches a slot or object on or off for a reason.
pub fn set_enabled_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(params, "entity")?;
    let enabled = params
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| missing("enabled"))?;
    let reason =
        crate::params::opt_str(params, "reason")?.unwrap_or(regia_ai::smart_object_v2::DEFAULT_OFF_REASON);
    let effective = set_enabled(world, entity, enabled, reason).ok_or_else(|| {
        refused(
            "entity_not_found",
            "the entity is neither a smart-object slot nor an object",
        )
    })?;
    Ok(json!({ "enabled": effective }))
}

/// `smartobject.tags`: adds and removes runtime tags on a slot or object and reports the result.
pub fn tags(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(params, "entity")?;
    if world.get::<SmartObjectSlot>(entity).is_none() && world.get::<SmartObject>(entity).is_none() {
        return Err(refused(
            "entity_not_found",
            "the entity is neither a smart-object slot nor an object",
        ));
    }
    let add = tag_list(world, params, "add")?;
    let remove = tag_list(world, params, "remove")?;
    for tag in add {
        add_runtime_tag(world, entity, tag);
    }
    for tag in remove {
        remove_runtime_tag(world, entity, tag);
    }
    let names: Vec<&'static str> = match (
        world.get::<SmartObjectSlot>(entity),
        world.get::<SmartObject>(entity),
    ) {
        (Some(slot), _) => slot.runtime_tags.names(),
        (None, Some(object)) => object.runtime_tags.names(),
        (None, None) => Vec::new(),
    };
    Ok(json!({ "tags": names }))
}

/// `smartobject.set_definition`: sets `SmartObject::definition` from a `path` or an `id` and reads
/// the definition now, so the answer says whether it loaded. The slots appear in `PostUpdate`.
pub fn set_definition(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(params, "entity")?;
    if world.get::<SmartObject>(entity).is_none() {
        return Err(refused(
            "entity_not_found",
            format!("{} has no SmartObject", entity.to_bits()),
        ));
    }
    let id = match (crate::params::opt_str(params, "path")?, params.get("id")) {
        (Some(path), _) => world
            .get_resource::<AssetServer>()
            .and_then(|server| server.id_for_path(&format!("assets/{path}")))
            .ok_or_else(|| {
                refused(
                    "definition_not_found",
                    format!("no cooked asset at `{path}` (run `regia cook`)"),
                )
            })?,
        (None, Some(id)) => id
            .as_str()
            .and_then(|id| id.parse::<AssetId>().ok())
            .ok_or_else(|| refused("bad_id", "`id` must be an asset id string"))?,
        (None, None) => return Err(missing("path")),
    };
    let loaded = load_definition(world, id);
    if let Some(mut object) = world.get_mut::<SmartObject>(entity) {
        object.definition = Some(AssetRef::new(id));
    }
    let slot = world
        .get_resource::<SmartObjectDefs>()
        .and_then(|defs| defs.slot(id));
    Ok(json!({
        "definition": id.to_string(),
        "path": slot.and_then(|slot| slot.path.clone()),
        "loaded": loaded,
        "cooked": slot.is_some_and(|slot| slot.cooked),
        "error": slot.and_then(|slot| slot.error.as_ref()).map(|error| {
            json!({ "reason": error.reason, "detail": error.detail })
        }),
    }))
}

/// `smartobject.messages`: the most recent `SlotChanged` / `SlotLost` records, oldest first.
pub fn messages(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let last = crate::params::opt_u64(params, "last")?
        .map_or(64, |n| usize::try_from(n).unwrap_or(usize::MAX))
        .min(256);
    let Some(log) = world.get_resource::<SmartObjectEvents>() else {
        return Ok(json!({ "messages": [] }));
    };
    let records: Vec<Value> = log
        .recent(last)
        .into_iter()
        .map(|event| match event {
            SmartObjectEvent::Changed(c) => json!({
                "kind": "changed",
                "object": c.object.map_or(Value::Null, |o| json!(o.to_bits())),
                "slot": c.slot.map_or(Value::Null, |s| json!(s.to_bits())),
                "what": c.what.name(),
                "tag": c.tag.map_or(Value::Null, |t| json!(t.name())),
            }),
            SmartObjectEvent::Lost(l) => json!({
                "kind": "lost",
                "handle": handle_json(l.handle),
                "holder": l.holder.to_bits(),
                "reason": l.reason.name(),
            }),
        })
        .collect();
    Ok(json!({ "messages": records }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smart_object::{claim, release};
    use regia_ai::SmartObjectPlugin;
    use regia_core::math::Affine3A;
    use regia_scene::GlobalTransform;

    fn reason_of(error: &RpcError) -> &str {
        error
            .data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    #[test]
    fn set_definition_names_each_refusal() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SmartObjectPlugin));
        let object = app.world_mut().spawn(SmartObject::default()).id();
        let plain = app.world_mut().spawn_empty().id();
        let call = |app: &mut App, params: Value| set_definition(app.world_mut(), &params);
        let error = call(&mut app, json!({})).expect_err("no entity");
        assert_eq!(reason_of(&error), "missing_param");
        let error = call(&mut app, json!({ "entity": object.to_bits() })).expect_err("no target");
        assert_eq!(reason_of(&error), "missing_param");
        let error = call(
            &mut app,
            json!({ "entity": plain.to_bits(), "path": "a.smartobject.ron" }),
        )
        .expect_err("plain");
        assert_eq!(reason_of(&error), "entity_not_found");
        let error = call(
            &mut app,
            json!({ "entity": object.to_bits(), "path": "a.smartobject.ron" }),
        )
        .expect_err("no asset");
        assert_eq!(reason_of(&error), "definition_not_found");
        let error = call(&mut app, json!({ "entity": object.to_bits(), "id": 7 })).expect_err("bad id");
        assert_eq!(reason_of(&error), "bad_id");
        assert!(
            app.world()
                .get::<SmartObject>(object)
                .is_some_and(|o| o.definition.is_none())
        );
    }

    /// An id nothing resolves still points the object: the result says what went wrong, and the
    /// list shows it, so a scene is not silently left with no slots.
    #[test]
    fn a_definition_that_does_not_load_is_reported_not_refused() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SmartObjectPlugin));
        let object = app.world_mut().spawn(SmartObject::default()).id();
        let id = AssetId::new_random();
        let answer = set_definition(
            app.world_mut(),
            &json!({ "entity": object.to_bits(), "id": id.to_string() }),
        )
        .expect("answer");
        assert_eq!(answer["loaded"], false);
        assert_eq!(answer["error"]["reason"], "definition_not_found");
        let listed = crate::smart_object::list(app.world_mut(), &json!({})).expect("list");
        let row = &listed["objects"][0];
        assert_eq!(row["definition"], id.to_string());
        assert_eq!(row["definition_error"]["reason"], "definition_not_found");
    }

    /// A definition in the resource (as a load leaves it) reads as loaded, and the object gets its
    /// slots on the next update.
    #[test]
    fn a_loaded_definition_materializes_through_the_method() {
        use regia_ai::{SmartObjectDef, SmartObjectIndex};
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SmartObjectPlugin));
        let id = AssetId::new_random();
        let def =
            SmartObjectDef::from_ron(r#"(format_version: 1, slots: [(name: "left"), (name: "right")])"#)
                .expect("definition");
        app.world_mut().resource_mut::<SmartObjectDefs>().insert(id, def);
        let object = app
            .world_mut()
            .spawn((SmartObject::default(), GlobalTransform::default()))
            .id();
        app.world_mut()
            .get_mut::<SmartObject>(object)
            .expect("object")
            .definition = Some(AssetRef::new(id));
        app.update();
        app.update();
        assert_eq!(app.world().resource::<SmartObjectIndex>().slots().count(), 2);
    }

    /// An app with one object and two slots at x = 1 and x = 2, plus two agents.
    fn scene() -> (App, Entity, Entity, Entity, Entity, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SmartObjectPlugin));
        let object = app.world_mut().spawn(SmartObject::default()).id();
        let mut slot_at = |x: f32| {
            app.world_mut()
                .spawn((
                    SmartObjectSlot::new("sit"),
                    GlobalTransform(Affine3A::from_translation(Vec3::new(x, 0.0, 0.0))),
                    ChildOf(object),
                ))
                .id()
        };
        let near = slot_at(1.0);
        let far = slot_at(2.0);
        let a = app.world_mut().spawn_empty().id();
        let b = app.world_mut().spawn_empty().id();
        app.update();
        (app, object, near, far, a, b)
    }

    fn everywhere() -> Value {
        json!({ "center": [0, 0, 0], "radius": 100 })
    }

    #[test]
    fn search_lists_hits_nearest_first_and_explains_rejections() {
        let (mut app, _object, near, far, a, _b) = scene();
        let result = search(app.world_mut(), &json!({ "region": everywhere() })).expect("searches");
        let hits = result["hits"].as_array().expect("hits");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0]["slot"], near.to_bits());
        assert_eq!(hits[1]["slot"], far.to_bits());
        assert_eq!(hits[0]["status"], "free");

        claim(
            app.world_mut(),
            &json!({ "slot": near.to_bits(), "agent": a.to_bits() }),
        )
        .expect("claims");
        let result = search(
            app.world_mut(),
            &json!({ "region": everywhere(), "explain": true }),
        )
        .expect("searches");
        assert_eq!(result["hits"].as_array().expect("hits").len(), 1);
        assert_eq!(result["rejections_total"], 1);
        assert_eq!(result["rejections"][0]["stage"], "state");
        assert_eq!(result["rejections"][0]["slot"], near.to_bits());

        let result = search(
            app.world_mut(),
            &json!({ "region": everywhere(), "allow_preempt": true, "priority": "high", "limit": 1 }),
        )
        .expect("searches");
        let hits = result["hits"].as_array().expect("hits");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["status"], "preemptable");
        assert_eq!(hits[0]["holder"], a.to_bits());
    }

    #[test]
    fn search_refuses_a_malformed_limit_or_user() {
        let (mut app, ..) = scene();
        let world = app.world_mut();
        for limit in [json!(-1), json!("3"), json!(1.5)] {
            let err = search(world, &json!({ "region": everywhere(), "limit": limit })).expect_err("limit");
            assert_eq!(reason_of(&err), "bad_query", "{limit}");
        }
        let err = search(world, &json!({ "region": everywhere(), "user": "me" })).expect_err("user");
        assert_eq!(reason_of(&err), "bad_query", "not an entity id");
        let gone = json!({ "region": everywhere(), "user": 123_456_789_u64 });
        let err = search(world, &gone).expect_err("user");
        assert_eq!(
            reason_of(&err),
            "entity_not_found",
            "a well-formed id of no live entity"
        );
        search(world, &json!({ "region": everywhere(), "limit": 1 })).expect("a valid limit");
    }

    #[test]
    fn search_refuses_bad_parameters_with_reasons() {
        let (mut app, ..) = scene();
        let world = app.world_mut();
        assert_eq!(
            reason_of(&search(world, &json!({})).expect_err("no region")),
            "missing_param"
        );
        assert_eq!(
            reason_of(&search(world, &json!({ "region": { "radius": 3 } })).expect_err("bad region")),
            "bad_region"
        );
        assert_eq!(
            reason_of(
                &search(world, &json!({ "region": { "center": [0, 0, 0], "radius": -1 } }))
                    .expect_err("negative")
            ),
            "bad_region"
        );
        assert_eq!(
            reason_of(
                &search(world, &json!({ "region": everywhere(), "priority": "huge" })).expect_err("priority")
            ),
            "bad_priority"
        );
        assert_eq!(
            reason_of(
                &search(world, &json!({ "region": everywhere(), "activity": "Has(" })).expect_err("query")
            ),
            "bad_query"
        );
        assert_eq!(
            reason_of(
                &search(
                    world,
                    &json!({ "region": everywhere(), "user_tags": ["not a tag"] })
                )
                .expect_err("tags")
            ),
            "bad_query"
        );
        for bad in [
            json!({ "region": everywhere(), "limit": -1 }),
            json!({ "region": everywhere(), "limit": "1" }),
            json!({ "region": everywhere(), "limit": 1.5 }),
            json!({ "region": everywhere(), "user": "a" }),
            json!({ "region": everywhere(), "user": -4 }),
            json!({ "region": everywhere(), "user": 0 }),
        ] {
            assert_eq!(
                reason_of(&search(world, &bad).expect_err("bad")),
                "bad_query",
                "{bad}"
            );
        }
        assert!(
            search(
                world,
                &json!({ "region": everywhere(), "limit": null, "user": null })
            )
            .is_ok()
        );
    }

    #[test]
    fn claim_with_priority_returns_a_handle_and_preemption_is_reported() {
        let (mut app, _object, near, _far, a, b) = scene();
        let first = claim(
            app.world_mut(),
            &json!({ "slot": near.to_bits(), "agent": a.to_bits(), "priority": "low" }),
        )
        .expect("claims");
        assert_eq!(first["claimed"], true);
        let handle = first["handle"].clone();
        // An equal-or-lower claim is refused with the holder; a higher one takes the slot.
        let refused_claim = claim(
            app.world_mut(),
            &json!({ "slot": near.to_bits(), "agent": b.to_bits(), "priority": "low" }),
        )
        .expect("answers");
        assert_eq!(refused_claim["claimed"], false);
        assert_eq!(refused_claim["reason"], "claimed");
        let taken = claim(
            app.world_mut(),
            &json!({ "slot": near.to_bits(), "agent": b.to_bits(), "priority": "urgent" }),
        )
        .expect("preempts");
        assert_eq!(taken["claimed"], true);
        // The first handle is stale now.
        let stale = release(app.world_mut(), &json!({ "handle": handle })).expect_err("stale");
        assert_eq!(reason_of(&stale), "stale_handle");
        let stale = begin_use_method(app.world_mut(), &json!({ "handle": handle })).expect_err("stale");
        assert_eq!(reason_of(&stale), "stale_handle");

        app.update();
        let log = messages(app.world_mut(), &json!({})).expect("messages");
        let lost: Vec<&Value> = log["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter(|m| m["kind"] == "lost")
            .collect();
        assert_eq!(lost.len(), 1);
        assert_eq!(lost[0]["reason"], "preempted");
        assert_eq!(lost[0]["holder"], a.to_bits());

        // Begin use through the new holder's handle; an in-use slot refuses everyone.
        let used = begin_use_method(app.world_mut(), &json!({ "handle": taken["handle"] })).expect("uses");
        assert_eq!(used["state"], "in_use");
        let blocked = claim(
            app.world_mut(),
            &json!({ "slot": near.to_bits(), "agent": a.to_bits(), "priority": "urgent" }),
        )
        .expect("answers");
        assert_eq!(blocked["reason"], "in_use");
        let released = release(app.world_mut(), &json!({ "handle": taken["handle"] })).expect("releases");
        assert_eq!(released["released"], true);
    }

    #[test]
    fn claim_can_begin_use_in_one_call() {
        let (mut app, _object, near, _far, a, _b) = scene();
        let result = claim(
            app.world_mut(),
            &json!({ "slot": near.to_bits(), "agent": a.to_bits(), "use": true }),
        )
        .expect("claims");
        assert_eq!(result["claimed"], true);
        assert_eq!(slot_state(app.world(), near).map(|s| s.name()), Some("in_use"));
        let bad = claim(
            app.world_mut(),
            &json!({ "slot": near.to_bits(), "agent": a.to_bits(), "priority": "x" }),
        )
        .expect_err("priority");
        assert_eq!(reason_of(&bad), "bad_priority");
    }

    #[test]
    fn begin_use_reports_its_reasons() {
        let (mut app, ..) = scene();
        let world = app.world_mut();
        assert_eq!(
            reason_of(&begin_use_method(world, &json!({})).expect_err("none")),
            "missing_param"
        );
        assert_eq!(
            reason_of(
                &begin_use_method(world, &json!({ "handle": { "slot": 4294967399u64 } }))
                    .expect_err("no generation")
            ),
            "missing_param"
        );
        let gone = begin_use_method(
            world,
            &json!({ "handle": { "slot": 4294967399u64, "generation": 0 } }),
        )
        .expect_err("not a slot");
        assert_eq!(reason_of(&gone), "slot_not_found");
    }

    #[test]
    fn set_enabled_and_tags_act_on_slots_and_objects() {
        let (mut app, object, near, _far, _a, _b) = scene();
        let off = set_enabled_method(
            app.world_mut(),
            &json!({ "entity": near.to_bits(), "enabled": false }),
        )
        .expect("off");
        assert_eq!(off["enabled"], false);
        let search_result = search(app.world_mut(), &json!({ "region": everywhere() })).expect("searches");
        assert_eq!(search_result["hits"].as_array().expect("hits").len(), 1);
        let on = set_enabled_method(
            app.world_mut(),
            &json!({ "entity": near.to_bits(), "enabled": true }),
        )
        .expect("on");
        assert_eq!(on["enabled"], true);
        assert_eq!(
            reason_of(
                &set_enabled_method(
                    app.world_mut(),
                    &json!({ "entity": 4294967399u64, "enabled": true })
                )
                .expect_err("none")
            ),
            "entity_not_found"
        );
        assert_eq!(
            reason_of(
                &set_enabled_method(app.world_mut(), &json!({ "entity": near.to_bits() }))
                    .expect_err("missing")
            ),
            "missing_param"
        );

        let added = tags(
            app.world_mut(),
            &json!({ "entity": object.to_bits(), "add": ["Activity.Sit", "Activity.Rest"] }),
        )
        .expect("adds");
        assert_eq!(added["tags"].as_array().expect("tags").len(), 2);
        let removed = tags(
            app.world_mut(),
            &json!({ "entity": object.to_bits(), "remove": ["Activity.Rest"] }),
        )
        .expect("removes");
        assert_eq!(removed["tags"], json!(["Activity.Sit"]));
        assert_eq!(
            reason_of(&tags(app.world_mut(), &json!({ "entity": 4294967399u64 })).expect_err("none")),
            "entity_not_found"
        );
    }

    #[test]
    fn messages_are_empty_without_the_plugin_and_capped() {
        let mut bare = App::new();
        bare.add_plugins(MinimalPlugins);
        let result = messages(bare.world_mut(), &json!({})).expect("answers");
        assert_eq!(result["messages"], json!([]));
        let result = search(bare.world_mut(), &json!({ "region": everywhere() })).expect("answers");
        assert_eq!(result["hits"], json!([]));

        let (mut app, _object, near, _far, a, _b) = scene();
        for _ in 0..3 {
            let handle = claim_for(&mut app, near, a);
            release(app.world_mut(), &json!({ "handle": handle })).expect("releases");
        }
        app.update();
        let two = messages(app.world_mut(), &json!({ "last": 2 })).expect("messages");
        assert_eq!(two["messages"].as_array().expect("messages").len(), 2);
    }

    /// Spec 39 § 10 lists `not_holder`: a release by handle that names an agent releases only
    /// that agent's claim (lane spec-17-39, ADR-5636).
    #[test]
    fn release_by_handle_with_an_agent_checks_the_holder() {
        let (mut app, _object, near, _far, a, b) = scene();
        let handle = claim_for(&mut app, near, a);
        let refused = release(
            app.world_mut(),
            &json!({ "handle": handle, "agent": b.to_bits() }),
        )
        .expect_err("b does not hold it");
        assert_eq!(reason_of(&refused), "not_holder");
        assert_eq!(slot_state(app.world(), near).map(|s| s.name()), Some("claimed"));
        let released = release(
            app.world_mut(),
            &json!({ "handle": handle, "agent": a.to_bits() }),
        )
        .expect("a holds it");
        assert_eq!(released["released"], true);
        let stale = release(
            app.world_mut(),
            &json!({ "handle": handle, "agent": a.to_bits() }),
        )
        .expect_err("released already");
        assert_eq!(reason_of(&stale), "stale_handle");
    }

    /// `begin_use` on a claim whose slot was disabled after the claim is `slot_disabled` (before
    /// the next `PostUpdate` releases the claim automatically).
    #[test]
    fn begin_use_on_a_disabled_slot_is_slot_disabled() {
        let (mut app, _object, near, _far, a, _b) = scene();
        let handle = claim_for(&mut app, near, a);
        set_enabled_method(
            app.world_mut(),
            &json!({ "entity": near.to_bits(), "enabled": false }),
        )
        .expect("off");
        let err = begin_use_method(app.world_mut(), &json!({ "handle": handle })).expect_err("disabled");
        assert_eq!(reason_of(&err), "slot_disabled");
    }

    /// With a project tag table loaded, a valid but undeclared tag is `tag_unknown` in `search`
    /// (`user_tags`, `activity`) and in `tags`.
    #[test]
    fn an_undeclared_tag_is_tag_unknown() {
        let (mut app, object, ..) = scene();
        let mut registry = TagRegistry::new();
        registry.declare("Activity.Sit", "sitting").expect("declares");
        app.world_mut().insert_resource(registry);
        let world = app.world_mut();
        let err = search(
            world,
            &json!({ "region": everywhere(), "user_tags": ["Role.Ghost"] }),
        )
        .expect_err("undeclared");
        assert_eq!(reason_of(&err), "tag_unknown");
        let err = search(
            world,
            &json!({ "region": everywhere(), "activity": "Has(\"Activity.Ghost\")" }),
        )
        .expect_err("undeclared");
        assert_eq!(reason_of(&err), "tag_unknown");
        let err = tags(
            world,
            &json!({ "entity": object.to_bits(), "add": ["Activity.Ghost"] }),
        )
        .expect_err("undeclared");
        assert_eq!(reason_of(&err), "tag_unknown");
        let ok = tags(
            world,
            &json!({ "entity": object.to_bits(), "add": ["Activity.Sit"] }),
        )
        .expect("declared");
        assert_eq!(ok["tags"], json!(["Activity.Sit"]));
    }

    fn claim_for(app: &mut App, slot: Entity, agent: Entity) -> Value {
        claim(
            app.world_mut(),
            &json!({ "slot": slot.to_bits(), "agent": agent.to_bits() }),
        )
        .expect("claims")["handle"]
            .clone()
    }
}
