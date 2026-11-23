// SPDX-License-Identifier: MPL-2.0
//! Agent surface of stable entity references in saves (ADR-3239, spec 91 § 10,
//! `book/agents/save-entity-refs.md`): `save.inspect` lists every saved entity of a slot with its
//! save id, its components and the references they hold, plus the notes of the last save or load.

use regia_app::prelude::*;
use regia_framework::save_entity_refs::{self, SaveEntityRefReport};
use regia_framework::save_flow;
use regia_framework::save_game::{SAVE_FORMAT_VERSION, SaveDirectory};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// The `save_entity_*` code a save error message starts with, if any.
fn reason_of(message: &str) -> Option<&str> {
    let start = message.find("save_entity_")?;
    let rest = &message[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_lowercase() || c == '_'))
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// The notes of the last save or load, or `null`.
fn last_report(world: &World) -> Value {
    world
        .get_resource::<SaveEntityRefReport>()
        .map_or(Value::Null, |report| {
            json!({
                "op": report.op.as_str(),
                "notes": report.notes.iter().map(|n| json!({
                    "code": n.code,
                    "entity": n.entity,
                    "component": n.component,
                    "field": n.field,
                })).collect::<Vec<_>>(),
            })
        })
}

/// `save.inspect`: a slot's saved entities with their save ids and references.
///
/// Parameters: `slot` (slot name). Reads through the app's save store (files in the
/// `SaveDirectory` when none is installed); the world is not changed.
pub fn inspect(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !world.contains_resource::<SaveDirectory>() {
        return Err(RpcError::not_found(
            "no SaveDirectory in this app",
            "add regia_framework::save_game::SavePlugin",
        ));
    }
    let slot_name = params
        .get("slot")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`slot` (save slot name) is required"))?;
    let slot = save_flow::sanitize_slot_name(slot_name);
    let bytes = save_flow::active_storage(world)
        .read(&slot)
        .map_err(|e| RpcError::internal(format!("cannot read save slot {slot:?}: {e}")))?
        .ok_or_else(|| {
            RpcError::not_found(
                format!("save slot {slot_name:?} does not exist"),
                "use save.list_slots to see available slots",
            )
        })?;
    let raw = save_entity_refs::parse_save_bytes(world, &bytes).map_err(|e| {
        let message = format!("cannot read save slot {slot:?}: {e}");
        let mut err = RpcError::new(
            protocol::INTERNAL_ERROR,
            "F0001",
            message.clone(),
            "see logs.read for details",
        );
        if let (Some(reason), Some(Value::Object(data))) = (reason_of(&message), err.data.as_mut()) {
            data.insert("reason".into(), Value::String(reason.into()));
        }
        err
    })?;
    let registry = world.resource::<AppTypeRegistry>().clone();
    let rows = save_entity_refs::inspect_refs(&raw, &registry.read());
    let key_of = |id: u32| {
        raw.entity_ids
            .get(id as usize)
            .map_or(Value::Null, |key| Value::String(key.key()))
    };
    let entities: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.id,
                "key": row.key.map_or(Value::Null, |key| Value::String(key.key())),
                "components": row.components,
                "refs": row.refs.iter().map(|r| json!({
                    "component": r.component,
                    "field": r.field,
                    "target": r.target,
                    "target_key": r.target.map_or(Value::Null, key_of),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let externals: Vec<Value> = raw
        .entity_ids
        .iter()
        .enumerate()
        .skip(raw.entities.len())
        .map(|(id, key)| json!({ "id": id, "key": key.key() }))
        .collect();
    Ok(json!({
        "slot": slot,
        "version": raw.version,
        "current_version": SAVE_FORMAT_VERSION,
        "legacy_refs": raw.version < 2,
        "entities": entities,
        "externals": externals,
        "last_report": last_report(world),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ecs::ecs::entity::{EntityMapper, MapEntities};
    use regia_ecs::reflect::ReflectMapEntities;
    use regia_framework::save_game::{SaveGame, SavePlugin, SaveSpec, Saveable};

    #[derive(Component, Reflect, Clone, Debug, PartialEq)]
    #[reflect(Component, MapEntities)]
    struct Target(Entity);

    impl MapEntities for Target {
        fn map_entities<M: EntityMapper>(&mut self, mapper: &mut M) {
            self.0 = mapper.get_mapped(self.0);
        }
    }

    #[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
    #[reflect(Component, Default)]
    struct Gem(u32);

    fn app(dir: &std::path::Path) -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SavePlugin::default()))
            .register_type::<Target>()
            .register_type::<Gem>()
            .insert_resource(SaveDirectory(dir.to_path_buf()));
        app
    }

    #[test]
    fn inspect_lists_save_ids_references_and_the_last_report() {
        let dir = regia_test_utils::unique_temp_dir("agent-save-inspect");
        let mut app = app(&dir);
        let gem = app.world_mut().spawn((Saveable, Gem(3))).id();
        app.world_mut().spawn((Saveable, Target(gem)));
        let unsaved = app.world_mut().spawn(Gem(9)).id();
        app.world_mut().spawn((Saveable, Gem(1), Target(unsaved)));
        let slot = SaveGame::slot(&SaveDirectory(dir.clone()), "Refs");
        SaveGame::save(&slot, app.world_mut(), &SaveSpec::default()).expect("save");

        let out = inspect(app.world_mut(), &json!({ "slot": "Refs" })).expect("save.inspect");
        assert_eq!(out["version"], json!(SAVE_FORMAT_VERSION));
        let entities = out["entities"].as_array().expect("entities");
        assert_eq!(entities.len(), 3);
        let with_ref = entities
            .iter()
            .find(|e| !e["refs"].as_array().is_none_or(Vec::is_empty))
            .expect("one entity holds a reference");
        let target = with_ref["refs"][0]["target"].as_u64().expect("target id");
        assert_eq!(with_ref["refs"][0]["component"], json!("Target"));
        assert_eq!(with_ref["refs"][0]["field"], json!("0"));
        assert_eq!(entities[target as usize]["components"], json!(["Gem"]));
        assert_eq!(
            with_ref["refs"][0]["target_key"],
            entities[target as usize]["key"]
        );
        let notes = out["last_report"]["notes"].as_array().expect("notes");
        assert_eq!(out["last_report"]["op"], json!("save"));
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(notes[0]["code"], json!("save_entity_ref_unsaved"));

        let missing = inspect(app.world_mut(), &json!({ "slot": "Nope" })).expect_err("missing slot");
        assert_eq!(missing.code, protocol::NOT_FOUND);
        let bad = inspect(app.world_mut(), &json!({})).expect_err("no slot");
        assert_eq!(bad.code, protocol::INVALID_PARAMS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inspect_needs_the_save_plugin() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let err = inspect(app.world_mut(), &json!({ "slot": "A" })).expect_err("no plugin");
        assert_eq!(err.code, protocol::NOT_FOUND);
    }

    #[test]
    fn reasons_are_read_from_the_message() {
        assert_eq!(
            reason_of("save file corrupted: save_entity_ref_out_of_range: saved entity 1"),
            Some("save_entity_ref_out_of_range")
        );
        assert_eq!(reason_of("CRC mismatch"), None);
    }
}
