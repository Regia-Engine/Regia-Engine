// SPDX-License-Identifier: MPL-2.0
//! The mount API of ADR-7881: what [`ModFiles::mount`] holds for each way of reading a mod,
//! [`ModFiles::with_mount`], which claims are refused with `lua_id_taken`, a reload keeping the
//! mod's mount, and the plugin's startup order between two mounts claiming one id.

mod common;

use std::collections::BTreeMap;

use common::{TestWorld, host, mod_files};
use regia_app::prelude::*;
use regia_asset::mods_vfs::{ModSource, ModsVfs};
use regia_script_lua::{LuaErrorCode, LuaMods, LuaModsPlugin, MAX_MOUNT_BYTES, ModFiles, NoWorld};

fn vfs_files(mount_id: &str, manifest_id: &str) -> ModFiles {
    let mut files = BTreeMap::new();
    files.insert(
        "luamod.ron".to_owned(),
        common::manifest(manifest_id, "1.0.0").into_bytes(),
    );
    files.insert("main.lua".to_owned(), b"x = 1".to_vec());
    let mut vfs = ModsVfs::new();
    vfs.mount(mount_id, ModSource::Memory(files)).expect("mount");
    ModFiles::from_mods_vfs(&vfs, mount_id, 1 << 20).expect("files")
}

#[test]
fn each_source_names_its_mount() {
    assert_eq!(
        vfs_files("workshop.31337", "example.a").mount(),
        Some("mods:workshop.31337")
    );
    assert_eq!(mod_files("example.a", "1", "").mount(), None);
    let dir = regia_test_utils::unique_temp_dir("lua-mod-mount").join("lights");
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("luamod.ron"), common::manifest("example.lights", "1")).expect("write");
    std::fs::write(dir.join("main.lua"), "x = 1").expect("write");
    let files = ModFiles::from_dir(&dir, 1 << 20).expect("reads");
    let mount = files.mount().expect("local mount");
    assert!(mount.starts_with("dir:"), "{mount}");
    assert_eq!(mount.len(), 4 + 64, "the full canonical-path BLAKE3 hash");
    assert!(mount.len() <= MAX_MOUNT_BYTES);
    let _ = std::fs::remove_dir_all(dir.parent().unwrap_or(&dir));

    let bound = mod_files("example.a", "1", "")
        .with_mount("pack:a")
        .expect("valid");
    assert_eq!(bound.mount(), Some("pack:a"));
    for bad in [
        "",
        "no-colon",
        "nul:\0",
        &format!("x:{}", "y".repeat(MAX_MOUNT_BYTES)),
    ] {
        let err = mod_files("example.a", "1", "")
            .with_mount(bad)
            .expect_err("refused");
        assert_eq!(err.code, LuaErrorCode::Manifest, "{bad:?}");
    }
}

#[test]
fn claims_are_refused_with_lua_id_taken_and_logged_codes_are_stable() {
    let mut mods = host();
    mods.load(vfs_files("local.a", "example.a"), &mut NoWorld)
        .expect("loads");
    // The very same mod again is already loaded (unchanged).
    let again = mods
        .load(vfs_files("local.a", "example.a"), &mut NoWorld)
        .expect_err("refused");
    assert_eq!(again.code, LuaErrorCode::Manifest);
    // Another mount claiming the id, or files with no mount claiming it.
    for other in [
        vfs_files("workshop.7", "example.a"),
        mod_files("example.a", "1", ""),
    ] {
        let err = mods.load(other, &mut NoWorld).expect_err("refused");
        assert_eq!(err.code, LuaErrorCode::IdTaken, "{err:?}");
        assert_eq!(err.code.code(), "lua_id_taken");
    }
    // The same mount under another manifest id.
    let err = mods
        .load(vfs_files("local.a", "example.b"), &mut NoWorld)
        .expect_err("refused");
    assert_eq!(err.code, LuaErrorCode::IdTaken);
    assert_eq!(mods.contested_ids().collect::<Vec<_>>(), ["example.a"]);
    assert!(mods.get("example.b").is_none());
    let loaded = mods.get("example.a").expect("the first claim stays");
    assert_eq!(loaded.mount(), Some("mods:local.a"));
    assert_eq!(loaded.save_key(), "example.a");
    assert!(LuaErrorCode::ALL.contains(&LuaErrorCode::IdTaken));
}

#[test]
fn a_reload_keeps_the_mods_mount() {
    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(vfs_files("workshop.7", "example.a"), &mut world)
        .expect("loads");
    // New code handed over as a map (`lua.reload` with `files`): still the same mounted mod.
    let outcome = mods
        .reload(
            "example.a",
            Some(mod_files("example.a", "1.0.1", "x = 2")),
            &mut world,
        )
        .expect("the mod exists");
    assert!(outcome.ok, "{outcome:?}");
    let reloaded = mods.get("example.a").expect("loaded");
    assert_eq!(reloaded.manifest().version, "1.0.1");
    assert_eq!(reloaded.mount(), Some("mods:workshop.7"));
    assert_eq!(world.position_of("vault"), None, "the mod spawns nothing");
    let (bytes, _) = mods.save_state(&mut world);
    let file = regia_script_lua::decode_luastate(&bytes, &mods.limits()).expect("decodes");
    assert_eq!(file.mods[0].id, "example.a");
}

#[test]
fn at_startup_a_local_mod_keeps_its_id_against_a_workshop_item() {
    // The game lists the Workshop item first; the local mod still wins the id.
    let local = mod_files("example.a", "1", "who = 'local'")
        .with_mount("dir:a")
        .expect("valid");
    let mut workshop = BTreeMap::new();
    workshop.insert(
        "luamod.ron".to_owned(),
        common::manifest("example.a", "1").into_bytes(),
    );
    workshop.insert("main.lua".to_owned(), b"who = 'workshop'".to_vec());
    let mut vfs = ModsVfs::new();
    vfs.mount("workshop.1", ModSource::Memory(workshop))
        .expect("mount");
    let workshop = ModFiles::from_mods_vfs(&vfs, "workshop.1", 1 << 20).expect("files");
    let mut app = App::new();
    app.add_plugins(LuaModsPlugin::new(vec![workshop, local]));
    app.world_mut().run_schedule(Startup);
    let mods = app.world().resource::<LuaMods>();
    assert_eq!(mods.iter().count(), 1);
    assert_eq!(mods.get("example.a").and_then(|m| m.mount()), Some("dir:a"));
    // The agent sees the binding and the refused claim.
    let extension = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .clone();
    let (_, handler) = extension.method("lua.list").expect("registered");
    let list = handler(app.world_mut(), &serde_json::json!({})).expect("answers");
    assert_eq!(list["mods"][0]["mount"], "dir:a", "{list}");
    assert_eq!(list["mods"][0]["save_key"], "example.a");
    assert_eq!(list["contested_ids"], serde_json::json!(["example.a"]));
    assert_eq!(list["held_state"], serde_json::json!([]));
}

#[test]
fn same_named_directories_in_different_parents_keep_their_own_state() {
    let root = regia_test_utils::unique_temp_dir("lua-same-name-mounts");
    let mut files = Vec::new();
    for (parent, id) in [("one", "example.one"), ("two", "example.two")] {
        let dir = root.join(parent).join("lights");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("luamod.ron"), common::manifest(id, "1")).expect("manifest");
        std::fs::write(dir.join("main.lua"), "regia.state.value = 0").expect("script");
        files.push(ModFiles::from_dir(&dir, 1 << 20).expect("reads"));
    }
    assert_ne!(
        files[0].mount(),
        files[1].mount(),
        "same-named folders have different canonical paths"
    );
    let alias = ModFiles::from_dir(&root.join("one/lights/../lights"), 1 << 20).expect("alias");
    assert_eq!(
        files[0].mount(),
        alias.mount(),
        "aliases of the same canonical directory agree"
    );
    let mut mods = host();
    for file in &files {
        mods.load(file.clone(), &mut NoWorld).expect("both load");
    }
    for (id, value) in [("example.one", 11), ("example.two", 22)] {
        mods.run_chunk(&mut NoWorld, Some(id), &format!("regia.state.value = {value}"))
            .expect("chunk");
    }
    let (saved, _) = mods.save_state(&mut NoWorld);
    let mut restored = host();
    for file in files.into_iter().rev() {
        restored.load(file, &mut NoWorld).expect("loads in reverse order");
    }
    restored.restore_state(Some(&saved), &mut NoWorld);
    for (id, value) in [("example.one", 11.0), ("example.two", 22.0)] {
        let result = restored
            .run_chunk(&mut NoWorld, Some(id), "return regia.state.value")
            .expect("chunk");
        assert_eq!(result.results, [regia_script_lua::ScriptValue::Number(value)]);
    }
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn held_entry_beside_a_live_mod_does_not_fail_every_game_save() {
    use regia_framework::save_game::{SaveDirectory, SaveGame, SaveSpec};

    let mut source = host();
    source
        .load(
            mod_files("example.a", "1", "regia.state.padding = string.rep('x', 600)"),
            &mut NoWorld,
        )
        .expect("loads");
    let legacy = source.save_state(&mut NoWorld).0;
    let mut mods = LuaMods::new(
        1234,
        regia_script_lua::LuaLimits {
            max_state_total_bytes: 1024,
            ..Default::default()
        },
    );
    mods.load(vfs_files("workshop.7", "example.a"), &mut NoWorld)
        .expect("loads");
    assert!(
        mods.load(
            mod_files("example.a", "1", "")
                .with_mount("dir:legitimate")
                .expect("mount"),
            &mut NoWorld
        )
        .is_err()
    );
    assert!(mods.restore_state(Some(&legacy), &mut NoWorld).error.is_none());
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, LuaModsPlugin::new(Vec::new())));
    app.world_mut().run_schedule(Startup);
    app.insert_resource(mods);
    let dir = regia_test_utils::unique_temp_dir("lua-held-save-overflow");
    let slot = SaveGame::slot(&SaveDirectory(dir.clone()), "Test");
    SaveGame::save(&slot, app.world_mut(), &SaveSpec::default()).expect("initial save fits");
    app.world_mut()
        .resource_mut::<LuaMods>()
        .load(
            mod_files("example.other", "1", "regia.state.padding = string.rep('y', 600)"),
            &mut NoWorld,
        )
        .expect("loads");
    SaveGame::save(&slot, app.world_mut(), &SaveSpec::default())
        .expect("a live mod that does not fit beside held entries must not fail the game save");
    SaveGame::encode(app.world_mut(), &SaveSpec::default())
        .expect("cloud saves also succeed when only the live mod does not fit");
    let extension = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .clone();
    let (_, handler) = extension.method("lua.list").expect("registered");
    let list = handler(app.world_mut(), &serde_json::json!({})).expect("answers");
    assert!(
        list["last_save"]["bytes"].as_u64().unwrap_or(0) > 0,
        "the game save kept the Lua section: {list}"
    );
    assert_eq!(list["held_state"], serde_json::json!(["example.a"]));
    SaveGame::save(&slot, app.world_mut(), &SaveSpec::default())
        .expect("the next save in the session also succeeds");
    app.world_mut()
        .resource_mut::<LuaMods>()
        .run_chunk(&mut NoWorld, Some("example.other"), "regia.state.padding = nil")
        .expect("shrink");
    SaveGame::save(&slot, app.world_mut(), &SaveSpec::default()).expect("retry fits");
    std::fs::remove_dir_all(dir).expect("cleanup");
}

/// Held entries alone over the cap still abort the game save and leave the previous file.
#[test]
fn held_entries_alone_over_the_cap_do_not_replace_the_previous_save() {
    use regia_framework::save_game::{SaveDirectory, SaveGame, SaveSpec};

    let mut source = host();
    source
        .load(
            mod_files("example.a", "1", "regia.state.padding = string.rep('x', 2000)"),
            &mut NoWorld,
        )
        .expect("loads");
    let legacy = source.save_state(&mut NoWorld).0;
    // `decode` accepts a blob up to cap + 16 bytes. This cap still reads the file and is
    // still under the one held entry (8-byte header plus that entry).
    let cap = (legacy.len() as u64).saturating_sub(16);
    assert!(
        cap > 64,
        "blob {} is too small to prove an over-cap hold",
        legacy.len()
    );
    let mut mods = LuaMods::new(
        1234,
        regia_script_lua::LuaLimits {
            max_state_total_bytes: cap,
            ..Default::default()
        },
    );
    mods.load(vfs_files("workshop.7", "example.a"), &mut NoWorld)
        .expect("loads");
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, LuaModsPlugin::new(Vec::new())));
    app.world_mut().run_schedule(Startup);
    app.insert_resource(mods);
    let dir = regia_test_utils::unique_temp_dir("lua-held-alone-overflow");
    let slot = SaveGame::slot(&SaveDirectory(dir.clone()), "Test");
    SaveGame::save(&slot, app.world_mut(), &SaveSpec::default()).expect("initial save fits");
    let previous = std::fs::read(&slot.path).expect("previous file");
    assert!(
        app.world_mut()
            .resource_mut::<LuaMods>()
            .restore_state(Some(&legacy), &mut NoWorld)
            .error
            .is_none()
    );
    let error = SaveGame::save(&slot, app.world_mut(), &SaveSpec::default())
        .expect_err("held entries alone over the cap fail the game save");
    assert!(error.to_string().contains("lua_too_large"), "{error}");
    assert_eq!(std::fs::read(&slot.path).expect("still present"), previous);
    assert!(
        SaveGame::encode(app.world_mut(), &SaveSpec::default()).is_err(),
        "cloud saves also fail before writing"
    );
    let extension = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .clone();
    let (_, handler) = extension.method("lua.list").expect("registered");
    let list = handler(app.world_mut(), &serde_json::json!({})).expect("answers");
    assert_eq!(list["last_save"]["bytes"], 0);
    assert_eq!(list["held_state"], serde_json::json!(["example.a"]));
    std::fs::remove_dir_all(dir).expect("cleanup");
}
