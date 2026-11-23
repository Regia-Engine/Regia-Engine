// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of gameplay tags (ADR-1463, `book/agents/gameplay-tags.md`):
//! `tags.list`, `tags.get`, `tags.set`, `tags.query` and `tags.validate`.
//!
//! The handlers read the [`TagRegistry`] resource and the [`TagContainer`] components that
//! `regia_framework::tags::TagPlugin` provides; without the plugin they answer `F0003` with a
//! hint naming it. Tag names are always strings on the wire, never ids (the id is shown as hex
//! for debugging).

use regia_app::prelude::*;
use regia_framework::tags::{Tag, TagContainer, TagQuery, TagRegistry};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Most tags `tags.set` takes in one call.
pub const SET_MAX_TAGS: usize = 256;

fn bad(method: &str, code: &str, message: &str) -> RpcError {
    RpcError::invalid_params(format!("{method}: {message} [{code}]"))
}

fn registry<'a>(world: &'a World, method: &str) -> Result<&'a TagRegistry, RpcError> {
    world.get_resource::<TagRegistry>().ok_or_else(|| {
        RpcError::not_found(
            format!("{method}: no TagRegistry in this app"),
            "add regia_framework::tags::TagPlugin to the app",
        )
    })
}

fn existing_entity(world: &World, method: &str, params: &Value) -> Result<Entity, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("{method}: entity {} does not exist", entity.to_bits()),
            "use scene.query_entities",
        ));
    }
    Ok(entity)
}

fn hex(value: u64) -> String {
    format!("{value:016x}")
}

fn names(tags: impl IntoIterator<Item = Tag>) -> Vec<&'static str> {
    tags.into_iter().map(|tag| tag.name()).collect()
}

/// `tags.list { prefix?, implicit? }`: the declared tags, sorted by name.
pub fn list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    const METHOD: &str = "tags.list";
    let registry = registry(world, METHOD)?;
    let prefix = match params.get("prefix") {
        None | Some(Value::Null) => None,
        Some(Value::String(prefix)) => Some(prefix.as_str()),
        Some(_) => return Err(bad(METHOD, "bad_prefix", "`prefix` must be a string")),
    };
    let implicit = match params.get("implicit") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err(bad(METHOD, "bad_implicit", "`implicit` must be a boolean")),
    };
    let tags: Vec<Value> = registry
        .iter()
        .filter(|entry| implicit || !entry.implicit)
        .filter(|entry| prefix.is_none_or(|p| entry.tag.name().starts_with(p)))
        .map(|entry| {
            json!({
                "name": entry.tag.name(),
                "id": hex(entry.tag.id()),
                "depth": entry.tag.depth(),
                "description": entry.description,
                "implicit": entry.implicit,
            })
        })
        .collect();
    Ok(json!({ "count": tags.len(), "tags": tags }))
}

fn container_json(entity: Entity, container: &TagContainer, registry: &TagRegistry) -> Value {
    let undeclared: Vec<&str> = container
        .iter()
        .filter(|tag| !registry.contains(*tag))
        .map(|tag| tag.name())
        .collect();
    json!({
        "entity": entity.to_bits(),
        "tags": container.names(),
        "expanded": names(container.expanded()),
        "undeclared": undeclared,
    })
}

/// `tags.get { entity }`: the tags an entity holds (an entity without a container holds none).
pub fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    const METHOD: &str = "tags.get";
    let registry = registry(world, METHOD)?;
    let entity = existing_entity(world, METHOD, params)?;
    let empty = TagContainer::default();
    let container = world.get::<TagContainer>(entity).unwrap_or(&empty);
    Ok(container_json(entity, container, registry))
}

/// `tags.set { entity, tags, mode?, allow_undeclared?, dry_run? }`: replaces, adds to or removes
/// from an entity's tags. The component changes (and `TagsChanged` fires) only when the result
/// differs from what the entity held.
pub fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    const METHOD: &str = "tags.set";
    let entity = existing_entity(world, METHOD, params)?;
    let mode = match params.get("mode") {
        None | Some(Value::Null) => "replace",
        Some(Value::String(mode)) => mode.as_str(),
        Some(_) => return Err(bad(METHOD, "bad_mode", "`mode` must be a string")),
    };
    if !matches!(mode, "replace" | "add" | "remove" | "remove_branch") {
        return Err(bad(
            METHOD,
            "bad_mode",
            "`mode` is one of replace, add, remove, remove_branch",
        ));
    }
    let flag = |key: &str, code: &str| match params.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(bad(METHOD, code, &format!("`{key}` must be a boolean"))),
    };
    let allow_undeclared = flag("allow_undeclared", "bad_allow_undeclared")?;
    let dry_run = flag("dry_run", "bad_dry_run")?;
    let Some(Value::Array(items)) = params.get("tags") else {
        return Err(bad(
            METHOD,
            "bad_tags",
            "`tags` (an array of tag names) is required",
        ));
    };
    if items.len() > SET_MAX_TAGS {
        return Err(bad(
            METHOD,
            "bad_tags",
            &format!("`tags` holds at most {SET_MAX_TAGS} names"),
        ));
    }
    let registry = registry(world, METHOD)?;
    let mut requested = Vec::with_capacity(items.len());
    for item in items {
        let Some(name) = item.as_str() else {
            return Err(bad(METHOD, "bad_tags", "`tags` must hold strings"));
        };
        // Resolving against the registry first keeps undeclared names from being interned.
        let tag = if allow_undeclared {
            Tag::new(name).map_err(|e| bad(METHOD, "bad_tag_name", &e.to_string()))?
        } else {
            registry.resolve(name).map_err(|e| match e {
                regia_framework::tags::TagError::Undeclared { .. } => {
                    bad(METHOD, "undeclared_tag", &e.to_string())
                }
                other => bad(METHOD, "bad_tag_name", &other.to_string()),
            })?
        };
        requested.push(tag);
    }

    let before = world.get::<TagContainer>(entity).cloned().unwrap_or_default();
    let mut after = before.clone();
    match mode {
        "replace" => after = requested.iter().copied().collect(),
        "add" => after.extend(requested.iter().copied()),
        "remove" => {
            for tag in &requested {
                after.remove(*tag);
            }
        }
        _ => {
            for tag in &requested {
                after.remove_branch(*tag);
            }
        }
    }
    let changed = after != before;
    if changed && !dry_run {
        // Writing only on a real difference keeps change detection (and `TagsChanged`) honest.
        match world.get_mut::<TagContainer>(entity) {
            Some(mut held) => *held = after.clone(),
            None => {
                world.entity_mut(entity).insert(after.clone());
            }
        }
    }
    let added: Vec<Tag> = after.iter().filter(|tag| !before.has_exact(*tag)).collect();
    let removed: Vec<Tag> = before.iter().filter(|tag| !after.has_exact(*tag)).collect();
    Ok(json!({
        "entity": entity.to_bits(),
        "applied": changed && !dry_run,
        "changed": changed,
        "tags": after.names(),
        "added": names(added),
        "removed": names(removed),
    }))
}

/// `tags.query { entity, query }`: runs a [`TagQuery`] (its JSON form) against an entity's tags.
pub fn query(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    const METHOD: &str = "tags.query";
    let registry = registry(world, METHOD)?;
    let entity = existing_entity(world, METHOD, params)?;
    let Some(raw) = params.get("query").filter(|value| value.is_object()) else {
        return Err(bad(
            METHOD,
            "bad_query",
            "`query` (an object such as {\"Has\": \"Combat.Damage\"}) is required",
        ));
    };
    let parsed: TagQuery =
        serde_json::from_value(raw.clone()).map_err(|e| bad(METHOD, "bad_query", &e.to_string()))?;
    parsed
        .validate()
        .map_err(|e| bad(METHOD, "bad_query", &e.to_string()))?;
    let empty = TagContainer::default();
    let container = world.get::<TagContainer>(entity).unwrap_or(&empty);
    let undeclared: Vec<String> = registry
        .check_query("query", &parsed)
        .into_iter()
        .map(|diagnostic| diagnostic.name)
        .collect();
    Ok(json!({
        "entity": entity.to_bits(),
        "matches": container.matches(&parsed),
        "tags": container.names(),
        "undeclared": undeclared,
    }))
}

/// `tags.validate {}`: every tag held by a [`TagContainer`] in the world that the registry does not
/// declare.
pub fn validate(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    const METHOD: &str = "tags.validate";
    registry(world, METHOD)?;
    let mut errors = Vec::new();
    let mut checked = 0usize;
    // `try_query` is `None` until a container has ever been spawned, so this cannot panic on an
    // app that registered the plugin but uses no tags.
    if let Some(mut containers) = world.try_query::<(Entity, &TagContainer)>() {
        let registry = registry(world, METHOD)?;
        let mut rows: Vec<(Entity, &TagContainer)> = containers.iter(world).collect();
        rows.sort_by_key(|(entity, _)| entity.to_bits());
        for (entity, container) in rows {
            checked += 1;
            for diagnostic in registry.check_container(&entity.to_bits().to_string(), container) {
                errors.push(json!({
                    "entity": entity.to_bits(),
                    "tag": diagnostic.name,
                    "message": diagnostic.error.to_string(),
                }));
            }
        }
    }
    Ok(json!({ "ok": errors.is_empty(), "checked": checked, "errors": errors }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::tags::TagPlugin;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, TagPlugin));
        let mut registry = app.world_mut().resource_mut::<TagRegistry>();
        registry
            .declare("Ag.Combat.Damage.Fire", "Fire damage")
            .expect("declare");
        registry.declare("Ag.Combat.Damage.Ice", "").expect("declare");
        registry.declare("Ag.Status.Burning", "").expect("declare");
        app
    }

    fn spawn(app: &mut App, names: &[&str]) -> u64 {
        app.world_mut()
            .spawn(TagContainer::from_names(names.iter().copied()).expect("names"))
            .id()
            .to_bits()
    }

    #[test]
    fn list_filters_by_prefix_and_implicitness() {
        let mut app = app();
        let all = list(app.world_mut(), &json!({})).expect("list");
        assert_eq!(all["count"], 7);
        assert_eq!(all["tags"][0]["name"], "Ag");
        assert_eq!(all["tags"][0]["implicit"], true);
        let fire = all["tags"]
            .as_array()
            .and_then(|tags| tags.iter().find(|t| t["name"] == "Ag.Combat.Damage.Fire"))
            .expect("fire listed");
        assert_eq!(fire["description"], "Fire damage");
        assert_eq!(fire["depth"], 4);
        assert_eq!(fire["id"].as_str().map(str::len), Some(16));

        let combat = list(
            app.world_mut(),
            &json!({ "prefix": "Ag.Combat", "implicit": false }),
        )
        .expect("list");
        assert_eq!(combat["count"], 2);
        let bad_prefix = list(app.world_mut(), &json!({ "prefix": 3 })).expect_err("bad");
        assert!(bad_prefix.message.contains("[bad_prefix]"));
    }

    #[test]
    fn set_get_round_trip_and_modes() {
        let mut app = app();
        let entity = spawn(&mut app, &[]);
        let set_result = set(
            app.world_mut(),
            &json!({ "entity": entity, "tags": ["Ag.Status.Burning", "Ag.Combat.Damage.Fire"] }),
        )
        .expect("set");
        assert_eq!(set_result["applied"], true);
        assert_eq!(
            set_result["tags"],
            json!(["Ag.Combat.Damage.Fire", "Ag.Status.Burning"])
        );

        let got = get(app.world_mut(), &json!({ "entity": entity })).expect("get");
        assert_eq!(got["tags"], set_result["tags"]);
        assert_eq!(got["undeclared"], json!([]));
        assert!(
            got["expanded"]
                .as_array()
                .is_some_and(|e| e.iter().any(|n| n == "Ag.Combat"))
        );

        let added = set(
            app.world_mut(),
            &json!({ "entity": entity, "tags": ["Ag.Combat.Damage.Ice"], "mode": "add" }),
        )
        .expect("add");
        assert_eq!(added["added"], json!(["Ag.Combat.Damage.Ice"]));
        let removed = set(
            app.world_mut(),
            &json!({ "entity": entity, "tags": ["Ag.Combat.Damage"], "mode": "remove_branch" }),
        )
        .expect("remove_branch");
        assert_eq!(removed["tags"], json!(["Ag.Status.Burning"]));
        assert_eq!(removed["removed"].as_array().map(Vec::len), Some(2));
        let removed = set(
            app.world_mut(),
            &json!({ "entity": entity, "tags": ["Ag.Status.Burning"], "mode": "remove" }),
        )
        .expect("remove");
        assert_eq!(removed["tags"], json!([]));
    }

    #[test]
    fn set_inserts_a_container_and_an_unchanged_set_is_not_applied() {
        let mut app = app();
        let entity = app.world_mut().spawn_empty().id().to_bits();
        let first = set(
            app.world_mut(),
            &json!({ "entity": entity, "tags": ["Ag.Status.Burning"] }),
        )
        .expect("set");
        assert_eq!(first["applied"], true);
        let again = set(
            app.world_mut(),
            &json!({ "entity": entity, "tags": ["Ag.Status.Burning"] }),
        )
        .expect("set");
        assert_eq!(again["applied"], false);
        assert_eq!(again["changed"], false);
    }

    #[test]
    fn dry_run_changes_nothing() {
        let mut app = app();
        let entity = spawn(&mut app, &["Ag.Status.Burning"]);
        let result = set(
            app.world_mut(),
            &json!({ "entity": entity, "tags": [], "dry_run": true }),
        )
        .expect("dry run");
        assert_eq!(result["applied"], false);
        assert_eq!(result["changed"], true);
        assert_eq!(result["removed"], json!(["Ag.Status.Burning"]));
        let got = get(app.world_mut(), &json!({ "entity": entity })).expect("get");
        assert_eq!(got["tags"], json!(["Ag.Status.Burning"]));
    }

    #[test]
    fn set_rejects_bad_input_with_documented_codes() {
        let mut app = app();
        let entity = spawn(&mut app, &[]);
        let mut code = |params: Value| set(app.world_mut(), &params).expect_err("must fail").message;
        assert!(code(json!({ "entity": entity, "tags": ["Ag.Typo"] })).contains("[undeclared_tag]"));
        assert!(code(json!({ "entity": entity, "tags": ["Bad Name"] })).contains("[bad_tag_name]"));
        assert!(code(json!({ "entity": entity, "tags": [3] })).contains("[bad_tags]"));
        assert!(code(json!({ "entity": entity })).contains("[bad_tags]"));
        assert!(code(json!({ "entity": entity, "tags": [], "mode": "swap" })).contains("[bad_mode]"));
        assert!(code(json!({ "entity": entity, "tags": [], "dry_run": "yes" })).contains("[bad_dry_run]"));
        assert!(
            code(json!({ "entity": entity, "tags": vec!["Ag.Status.Burning"; SET_MAX_TAGS + 1] }))
                .contains("[bad_tags]")
        );
        // An undeclared but valid name is accepted on request.
        let allowed = set(
            app.world_mut(),
            &json!({ "entity": entity, "tags": ["Ag.Typo"], "allow_undeclared": true }),
        )
        .expect("allowed");
        assert_eq!(allowed["tags"], json!(["Ag.Typo"]));
        let report = validate(app.world_mut(), &json!({})).expect("validate");
        assert_eq!(report["ok"], false);
        assert_eq!(report["errors"][0]["tag"], "Ag.Typo");
    }

    #[test]
    fn missing_entities_and_plugin_are_not_found() {
        let mut app = app();
        let gone = app.world_mut().spawn_empty().id();
        app.world_mut().entity_mut(gone).despawn();
        let err = get(app.world_mut(), &json!({ "entity": gone.to_bits() })).expect_err("gone");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str),
            Some("F0003")
        );

        let mut bare = App::new();
        bare.add_plugins(MinimalPlugins);
        let err = list(bare.world_mut(), &json!({})).expect_err("no plugin");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str),
            Some("F0003")
        );
    }

    #[test]
    fn query_runs_against_an_entity() {
        let mut app = app();
        let entity = spawn(&mut app, &["Ag.Combat.Damage.Fire"]);
        let run =
            |app: &mut App, query: Value| query_result(app, json!({ "entity": entity, "query": query }));
        let hit = run(&mut app, json!({ "Has": "Ag.Combat.Damage" }));
        assert_eq!(hit["matches"], true);
        let exact = run(&mut app, json!({ "HasExact": "Ag.Combat.Damage" }));
        assert_eq!(exact["matches"], false);
        let nested = run(
            &mut app,
            json!({ "AllOf": [{ "Has": "Ag.Combat" }, { "NoneOf": [{ "Has": "Ag.Status.Burning" }] }] }),
        );
        assert_eq!(nested["matches"], true);
        let typo = run(&mut app, json!({ "AnyOf": [{ "Has": "Ag.Combat.Damage.Acid" }] }));
        assert_eq!(typo["matches"], false);
        assert_eq!(typo["undeclared"], json!(["Ag.Combat.Damage.Acid"]));
    }

    fn query_result(app: &mut App, params: Value) -> Value {
        query(app.world_mut(), &params).expect("query")
    }

    #[test]
    fn query_rejects_a_malformed_expression() {
        let mut app = app();
        let entity = spawn(&mut app, &[]);
        for bad_query in [
            json!({ "Wat": [] }),
            json!({ "Has": "Bad Name" }),
            json!("Has"),
            json!(null),
        ] {
            let err =
                query(app.world_mut(), &json!({ "entity": entity, "query": bad_query })).expect_err("bad");
            assert!(err.message.contains("[bad_query]"), "{}", err.message);
        }
        assert!(query(app.world_mut(), &json!({ "entity": entity })).is_err());
    }

    #[test]
    fn validate_reports_undeclared_tags_per_entity() {
        let mut app = app();
        let empty = validate(app.world_mut(), &json!({})).expect("validate");
        assert_eq!(empty["ok"], true);
        assert_eq!(empty["checked"], 0);
        spawn(&mut app, &["Ag.Status.Burning"]);
        let bad = spawn(&mut app, &["Ag.Status.Burning", "Ag.Status.Frozen"]);
        let report = validate(app.world_mut(), &json!({})).expect("validate");
        assert_eq!(report["ok"], false);
        assert_eq!(report["checked"], 2);
        assert_eq!(report["errors"].as_array().map(Vec::len), Some(1));
        assert_eq!(report["errors"][0]["entity"], bad);
        assert_eq!(report["errors"][0]["tag"], "Ag.Status.Frozen");
    }
}
