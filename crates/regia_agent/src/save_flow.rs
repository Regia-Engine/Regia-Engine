// SPDX-License-Identifier: MPL-2.0
//! Agent surface of the save flow and character move-to (spec 91 § 4, `book/agents/save-flow.md`):
//! `save.quick_save`, `save.quick_load`, `save.delete`, `save.status`, `character.move_to`.
//!
//! Every save method goes through the app's save store (`regia_framework::save_flow::SaveStore`,
//! files in the `SaveDirectory` when none is installed), so an agent exercises the same storage
//! the game uses.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::save_flow::{self, QuickSaveConfig, SaveBlobs, SaveInfo};
use regia_framework::save_game::{LastSaveLoad, SaveDirectory, SaveResult};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// `F0003` when the app has no save plugin (no `SaveDirectory`).
fn require_save_plugin(world: &World) -> Result<(), RpcError> {
    if world.contains_resource::<SaveDirectory>() {
        Ok(())
    } else {
        Err(RpcError::not_found(
            "no SaveDirectory in this app",
            "add regia_framework::save_game::SavePlugin and regia_framework::save_flow::SaveFlowPlugin",
        ))
    }
}

/// The quick slot name (the default when `SaveFlowPlugin` is not installed).
fn quick_slot(world: &World) -> String {
    world
        .get_resource::<QuickSaveConfig>()
        .map_or_else(|| QuickSaveConfig::default().slot, |config| config.slot.clone())
}

/// The `kind` of the storage in effect.
fn store_kind(world: &World) -> &'static str {
    save_flow::active_storage(world).kind()
}

/// The common answer of a save or load.
fn result_json(world: &World, slot: &str, result: &SaveResult) -> Value {
    json!({
        "slot": save_flow::sanitize_slot_name(slot),
        "entity_count": result.entity_count,
        "resource_count": result.resource_count,
        "bytes": result.bytes_written,
        "store": store_kind(world),
        "copy": result.copy.as_str(),
    })
}

/// `save.quick_save`: saves the world to the quick slot.
pub fn quick_save(world: &mut World) -> Result<Value, RpcError> {
    require_save_plugin(world)?;
    let slot = quick_slot(world);
    let result = save_flow::save_to_store(world, &slot)
        .map_err(|e| RpcError::internal(format!("quick save failed: {e}")))?;
    Ok(result_json(world, &slot, &result))
}

/// `save.quick_load`: loads the quick slot into the world.
pub fn quick_load(world: &mut World) -> Result<Value, RpcError> {
    require_save_plugin(world)?;
    let slot = quick_slot(world);
    let storage = save_flow::active_storage(world);
    let sanitized = save_flow::sanitize_slot_name(&slot);
    // A current copy that is too large (or otherwise corrupt) still names a slot.
    // `load_from_store` falls back to the previous generation; do not fail before that.
    let exists = match storage.read(&sanitized) {
        Ok(Some(_)) => true,
        Ok(None) => storage.has_previous(&sanitized),
        Err(regia_framework::save_game::SaveError::Corrupt(_)) => true,
        Err(e) => return Err(RpcError::internal(format!("quick load failed: {e}"))),
    };
    if !exists {
        return Err(RpcError::not_found(
            format!("no quick save in slot {slot:?} yet"),
            "call save.quick_save first",
        ));
    }
    let result = save_flow::load_from_store(world, &slot)
        .map_err(|e| RpcError::internal(format!("quick load failed: {e}")))?;
    Ok(result_json(world, &slot, &result))
}

/// `save.delete`: deletes a slot. Parameters: `slot`.
pub fn delete(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_save_plugin(world)?;
    let slot = crate::params::opt_str(params, "slot")?
        .filter(|slot| !slot.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`slot` (save slot name) is required"))?;
    let deleted = save_flow::delete_from_store(world, slot)
        .map_err(|e| RpcError::internal(format!("delete failed: {e}")))?;
    Ok(json!({ "slot": save_flow::sanitize_slot_name(slot), "deleted": deleted }))
}

/// `save.status`: the store in effect, the quick slot, the slot names, the slots that keep a
/// previous generation, the last load (slot and copy), the live `SaveInfo` and the `SaveBlobs`
/// keys.
pub fn status(world: &mut World) -> Result<Value, RpcError> {
    require_save_plugin(world)?;
    let listed = save_flow::list_store(world)
        .map_err(|e| RpcError::internal(format!("cannot list save slots: {e}")))?;
    let slots_with_previous: Vec<&str> = listed
        .iter()
        .filter(|slot| slot.has_previous)
        .map(|slot| slot.name.as_str())
        .collect();
    let last_load = world
        .get_resource::<LastSaveLoad>()
        .map(|last| json!({ "slot": last.slot, "copy": last.copy.as_str() }));
    let slots: Vec<&str> = listed.iter().map(|slot| slot.name.as_str()).collect();
    let info = world.get_resource::<SaveInfo>().map(|info| {
        json!({
            "display_name": info.display_name,
            "level": info.level,
            "play_time_secs": info.play_time_secs,
            "save_count": info.save_count,
        })
    });
    let blob_keys: Vec<&str> = world
        .get_resource::<SaveBlobs>()
        .map(|blobs| blobs.keys().collect())
        .unwrap_or_default();
    Ok(json!({
        "store": store_kind(world),
        "quick_slot": quick_slot(world),
        "slots": slots,
        "slots_with_previous": slots_with_previous,
        "last_load": last_load,
        "info": info,
        "blob_keys": blob_keys,
    }))
}

/// `character.move_to`: gives a character a `MoveTo` toward `x`, `y`, `z`. Parameters: `entity`,
/// `x`, `y`, `z`, optional `accept_radius` (metres, default 0.25) and `sprint`.
pub fn character_move_to(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_framework::character::{Character, CharacterInput};
    use regia_framework::character_control::MoveTo;

    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entity` (character entity id) is required"))?;
    let entity = Entity::try_from_bits(bits)
        .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))?;
    let coordinate = |name: &str| {
        crate::params::opt_f64(params, name)?
            .map(|v| v as f32)
            .filter(|v| v.is_finite())
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` (finite number) is required")))
    };
    let target = Vec3::new(coordinate("x")?, coordinate("y")?, coordinate("z")?);
    let accept_radius = match params.get("accept_radius") {
        None | Some(Value::Null) => 0.25,
        Some(value) => value
            .as_f64()
            .map(|v| v as f32)
            .filter(|v| v.is_finite() && *v >= 0.0)
            .ok_or_else(|| RpcError::invalid_params("`accept_radius` must be a number >= 0"))?,
    };
    let sprint = crate::params::opt_bool(params, "sprint")?.unwrap_or(false);
    let is_character = world
        .get_entity(entity)
        .is_ok_and(|e| e.contains::<Character>() && e.contains::<CharacterInput>());
    if !is_character {
        return Err(RpcError::not_found(
            format!("entity {bits} is not a character with CharacterInput"),
            "players.list or scene queries name the character entities",
        ));
    }
    if !world.contains_resource::<Messages<regia_framework::character_control::MoveToArrived>>() {
        return Err(RpcError::not_found(
            "no CharacterControlPlugin in this app",
            "add regia_framework::character_control::CharacterControlPlugin",
        ));
    }
    world.entity_mut(entity).insert(MoveTo {
        accept_radius,
        sprint,
        ..MoveTo::new(target)
    });
    Ok(json!({ "entity": bits, "target": [target.x, target.y, target.z], "accept_radius": accept_radius }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::save_flow::{SaveFlowPlugin, SaveStorage};
    use regia_framework::save_game::{SavePlugin, Saveable};

    #[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
    #[reflect(Component, Default)]
    struct Gem(u32);

    fn app(dir: &std::path::Path) -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SavePlugin::default(), SaveFlowPlugin))
            .register_type::<Gem>()
            .insert_resource(SaveDirectory(dir.to_path_buf()));
        app
    }

    #[test]
    fn quick_save_status_quick_load_delete_round_trip() {
        let dir = regia_test_utils::unique_temp_dir("agent-save-flow");
        let mut app = app(&dir);
        app.world_mut().spawn((Saveable, Gem(4)));
        app.world_mut().resource_mut::<SaveInfo>().display_name = "Dock".into();
        app.world_mut().resource_mut::<SaveBlobs>().set("quest", vec![9]);

        let missing = quick_load(app.world_mut()).expect_err("no quick save yet");
        assert_eq!(missing.code, crate::protocol::NOT_FOUND);

        let saved = quick_save(app.world_mut()).expect("quick save");
        assert_eq!(saved["slot"], json!("Quick"));
        assert_eq!(saved["store"], json!("file"));
        assert_eq!(saved["entity_count"], json!(1));

        let state = status(app.world_mut()).expect("status");
        assert_eq!(state["slots"], json!(["Quick"]));
        assert_eq!(state["quick_slot"], json!("Quick"));
        assert_eq!(state["info"]["display_name"], json!("Dock"));
        assert_eq!(state["info"]["save_count"], json!(1));
        assert_eq!(state["blob_keys"], json!(["quest"]));

        // Change the world; the quick load brings the saved gem back.
        let gems: Vec<Entity> = app
            .world_mut()
            .query_filtered::<Entity, With<Gem>>()
            .iter(app.world())
            .collect();
        for gem in gems {
            app.world_mut().despawn(gem);
        }
        let loaded = quick_load(app.world_mut()).expect("quick load");
        assert_eq!(loaded["entity_count"], json!(1));
        let world = app.world_mut();
        assert_eq!(
            world.query::<&Gem>().iter(world).map(|g| g.0).collect::<Vec<_>>(),
            [4]
        );

        let deleted = delete(app.world_mut(), &json!({ "slot": "Quick" })).expect("delete");
        assert_eq!(deleted, json!({ "slot": "Quick", "deleted": true }));
        let again = delete(app.world_mut(), &json!({ "slot": "Quick" })).expect("delete twice");
        assert_eq!(again["deleted"], json!(false));
        let bad = delete(app.world_mut(), &json!({})).expect_err("slot required");
        assert_eq!(bad.code, crate::protocol::INVALID_PARAMS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A store keeping slots in memory: proves `save.save` / `save.load` / `save.list_slots` follow
    /// an installed store instead of the save directory.
    #[derive(Default, Clone)]
    struct Memory(std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>>);

    impl regia_framework::save_flow::SaveStorage for Memory {
        fn kind(&self) -> &'static str {
            "memory"
        }
        fn write(&self, slot: &str, bytes: &[u8]) -> Result<(), regia_framework::save_game::SaveError> {
            if let Ok(mut slots) = self.0.lock() {
                slots.insert(slot.to_owned(), bytes.to_vec());
            }
            Ok(())
        }
        fn read(&self, slot: &str) -> Result<Option<Vec<u8>>, regia_framework::save_game::SaveError> {
            Ok(self.0.lock().ok().and_then(|slots| slots.get(slot).cloned()))
        }
        fn delete(&self, slot: &str) -> Result<bool, regia_framework::save_game::SaveError> {
            Ok(self.0.lock().is_ok_and(|mut slots| slots.remove(slot).is_some()))
        }
        fn list(&self) -> Result<Vec<String>, regia_framework::save_game::SaveError> {
            Ok(self
                .0
                .lock()
                .map(|slots| slots.keys().cloned().collect())
                .unwrap_or_default())
        }
    }

    #[test]
    fn save_save_load_and_list_follow_an_installed_store() {
        let dir = regia_test_utils::unique_temp_dir("agent-save-store");
        let mut app = app(&dir);
        let memory = Memory::default();
        app.insert_resource(regia_framework::save_flow::SaveStore::new(memory.clone()));
        app.world_mut().spawn((Saveable, Gem(2)));

        let saved =
            crate::framework::save_save(app.world_mut(), &json!({ "slot": "Cloud 1" })).expect("save.save");
        assert_eq!(saved["slot"], json!("Cloud_1"));
        assert!(
            memory.read("Cloud_1").ok().flatten().is_some(),
            "the store got the slot"
        );
        assert!(!dir.join("Cloud_1.save").exists(), "nothing on disk");

        let listed = crate::framework::save_list_slots(app.world_mut()).expect("save.list_slots");
        assert_eq!(listed["count"], json!(1));
        assert_eq!(listed["slots"][0]["name"], json!("Cloud_1"));
        assert!(listed["slots"][0]["timestamp"].is_number(), "{listed}");

        let loaded =
            crate::framework::save_load(app.world_mut(), &json!({ "slot": "Cloud 1" })).expect("save.load");
        assert_eq!(loaded["entity_count"], json!(1));
        let missing = crate::framework::save_load(app.world_mut(), &json!({ "slot": "Nope" }))
            .expect_err("missing slot");
        assert_eq!(missing.code, crate::protocol::NOT_FOUND);
        assert_eq!(status(app.world_mut()).expect("status")["store"], json!("memory"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-3102: `save.list_slots` / `save.status` report the previous generation, and a load of a
    /// damaged slot reports `copy: "previous"` with the save before's data.
    #[test]
    fn a_damaged_slot_loads_its_previous_copy_and_status_says_so() {
        let dir = regia_test_utils::unique_temp_dir("agent-save-previous");
        let mut app = app(&dir);
        let gem = app.world_mut().spawn((Saveable, Gem(4))).id();
        quick_save(app.world_mut()).expect("first quick save");
        app.world_mut().entity_mut(gem).insert(Gem(5));
        let saved = quick_save(app.world_mut()).expect("second quick save");
        assert_eq!(saved["copy"], json!("current"));

        let state = status(app.world_mut()).expect("status");
        assert_eq!(state["slots_with_previous"], json!(["Quick"]));
        assert_eq!(state["last_load"], Value::Null, "no load yet");
        let listed = crate::framework::save_list_slots(app.world_mut()).expect("save.list_slots");
        assert_eq!(listed["slots"][0]["has_previous"], json!(true));

        // Tear the current file: the quick load falls back to the first save.
        let current = dir.join("Quick.save");
        let bytes = std::fs::read(&current).expect("current");
        std::fs::write(&current, &bytes[..bytes.len() / 2]).expect("tear");
        let loaded = quick_load(app.world_mut()).expect("quick load of a torn slot");
        assert_eq!(loaded["copy"], json!("previous"));
        let world = app.world_mut();
        assert_eq!(
            world.query::<&Gem>().iter(world).map(|g| g.0).collect::<Vec<_>>(),
            [4]
        );
        let state = status(app.world_mut()).expect("status");
        assert_eq!(state["last_load"], json!({ "slot": "Quick", "copy": "previous" }));

        // `save.load` (the file path, no store installed) does the same.
        let loaded = crate::framework::save_load(app.world_mut(), &json!({ "slot": "Quick" }))
            .expect("save.load of a torn slot");
        assert_eq!(loaded["copy"], json!("previous"));

        // A deliberate delete removes both copies.
        let deleted = delete(app.world_mut(), &json!({ "slot": "Quick" })).expect("delete");
        assert_eq!(deleted["deleted"], json!(true));
        assert!(!dir.join("Quick.prev").exists() && !current.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quick_load_of_an_oversized_current_copy_uses_the_previous_generation() {
        let dir = regia_test_utils::unique_temp_dir("agent-save-oversize");
        let mut app = app(&dir);
        let gem = app.world_mut().spawn((Saveable, Gem(4))).id();
        quick_save(app.world_mut()).expect("first");
        app.world_mut().entity_mut(gem).insert(Gem(5));
        quick_save(app.world_mut()).expect("second");
        let current = dir.join("Quick.save");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&current)
            .expect("open");
        // Sparse: the size check refuses this before reading the bytes.
        file.set_len(regia_framework::save_game::MAX_SAVE_FILE_BYTES + 1)
            .expect("sparse length");
        drop(file);
        let loaded = quick_load(app.world_mut()).expect("fallback to the previous generation");
        assert_eq!(loaded["copy"], json!("previous"));
        let world = app.world_mut();
        assert_eq!(
            world.query::<&Gem>().iter(world).map(|g| g.0).collect::<Vec<_>>(),
            [4]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_methods_need_the_save_plugin() {
        let mut bare = App::new();
        bare.add_plugins(MinimalPlugins);
        for result in [
            quick_save(bare.world_mut()),
            quick_load(bare.world_mut()),
            status(bare.world_mut()),
            delete(bare.world_mut(), &json!({ "slot": "A" })),
        ] {
            let err = result.expect_err("no save plugin");
            assert_eq!(err.code, crate::protocol::NOT_FOUND);
        }
    }

    #[test]
    fn move_to_validates_and_inserts_the_component() {
        use regia_framework::character::{Character, CharacterInput, CharacterPlugin};
        use regia_framework::character_control::{CharacterControlPlugin, MoveTo};

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CharacterPlugin, CharacterControlPlugin));
        let character = app.world_mut().spawn((Character, CharacterInput::default())).id();
        let prop = app.world_mut().spawn_empty().id();
        let bits = character.to_bits();

        let ok = character_move_to(
            app.world_mut(),
            &json!({ "entity": bits, "x": 2.0, "y": 0.0, "z": -1.0, "accept_radius": 0.5 }),
        )
        .expect("move_to");
        assert_eq!(ok["target"], json!([2.0, 0.0, -1.0]));
        let move_to = app.world().get::<MoveTo>(character).cloned();
        assert_eq!(
            move_to.map(|m| (m.target, m.accept_radius)),
            Some((Vec3::new(2.0, 0.0, -1.0), 0.5))
        );

        let not_character = character_move_to(
            app.world_mut(),
            &json!({ "entity": prop.to_bits(), "x": 0, "y": 0, "z": 0 }),
        )
        .expect_err("not a character");
        assert_eq!(not_character.code, crate::protocol::NOT_FOUND);
        for bad in [
            json!({ "x": 0, "y": 0, "z": 0 }),
            json!({ "entity": bits, "x": 0, "z": 0 }),
            json!({ "entity": bits, "x": 0, "y": 0, "z": 0, "accept_radius": -1 }),
        ] {
            let err = character_move_to(app.world_mut(), &bad).expect_err("bad params");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{bad}");
        }
    }
}
