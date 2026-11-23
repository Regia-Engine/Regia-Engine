// SPDX-License-Identifier: MPL-2.0
//! Mod ids bound to their mount (ADR-7881): a Workshop item whose `luamod.ron` claims another mod's
//! id never receives that mod's saved state or entities, and saves written before the change
//! (keyed by the manifest id) still load for the legitimate mod. ADR-7951 amends the expected
//! v1 keys: manifest ids are preserved, never rewritten as mounts. The regression tests use
//! the API present on the reviewed ADR-7881 head.
//! No GPU, no goldens, no clocks.

mod common;

use std::collections::BTreeMap;

use common::{TestWorld, host, mod_files};
use regia_app::prelude::*;
use regia_asset::mods_vfs::{ModSource, ModsVfs};
use regia_core::math::Vec3;
use regia_framework::save_game::{SaveDirectory, SaveGame, SaveSpec};
use regia_script_lua::{LuaMods, LuaModsPlugin, ModFiles, ScriptValue, decode_luastate};

/// Counts ticks in `regia.state.secret` and moves the mod's `vault` entity to x = secret, so a
/// restored state and restored entities are both visible from outside.
const VAULT: &str = r#"
local vault
function on_start()
  regia.state.secret = 0
  vault = regia.spawn("vault", regia.vec3(0, 2, 3))
end
function on_load(info) vault = regia.find("vault") end
function on_tick()
  regia.state.secret = regia.state.secret + 1
  vault:set_position(regia.vec3(regia.state.secret, 2, 3))
end
"#;

const ID: &str = "example.vault";

/// The Lua files of a mod with manifest id `manifest_id`, mounted at `mods://` as `mount_id`.
fn mounted(mount_id: &str, manifest_id: &str) -> ModFiles {
    let mut files = BTreeMap::new();
    files.insert(
        "luamod.ron".to_owned(),
        common::manifest(manifest_id, "1.0.0").into_bytes(),
    );
    files.insert("main.lua".to_owned(), VAULT.as_bytes().to_vec());
    let mut vfs = ModsVfs::new();
    vfs.mount(mount_id, ModSource::Memory(files)).expect("mount");
    ModFiles::from_mods_vfs(&vfs, mount_id, 1 << 20).expect("files")
}

/// The legitimate mod: a local `mods:` folder or `.fpak` (both `from_mods_vfs`, the only
/// loader on Android). Not a Workshop item, whose mount id is `workshop.<item id>`.
fn legit() -> ModFiles {
    mounted("local.vault", ID)
}

/// A Workshop item that claims the legitimate mod's id.
fn impostor() -> ModFiles {
    mounted("workshop.999", ID)
}

fn secret(mods: &mut LuaMods, world: &mut TestWorld) -> f64 {
    let outcome = mods
        .run_chunk(world, Some(ID), "return regia.state.secret")
        .expect("the mod exists");
    match outcome.results.first() {
        Some(ScriptValue::Number(n)) => *n,
        other => panic!("secret is {other:?}"),
    }
}

/// A save after five ticks of the mod in `files`.
fn save_after_five_ticks(files: ModFiles) -> Vec<u8> {
    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(files, &mut world).expect("loads");
    for _ in 0..5 {
        mods.call_tick(&mut world, 0.1);
    }
    assert_eq!(world.position_of("vault"), Some(Vec3::new(5.0, 2.0, 3.0)));
    mods.save_state(&mut world).0
}

fn saved_keys(bytes: &[u8]) -> Vec<String> {
    decode_luastate(bytes, &host().limits())
        .expect("decodes")
        .mods
        .into_iter()
        .map(|m| m.id)
        .collect()
}

#[test]
fn a_second_mount_claiming_an_id_gets_neither_the_state_nor_the_entities() {
    let bytes = save_after_five_ticks(legit());

    // The player subscribes to a Workshop item claiming the same id; it loads first.
    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(impostor(), &mut world).expect("the first claim loads");
    let refused = mods.load(legit(), &mut world);
    mods.restore_state(Some(&bytes), &mut world);
    assert_eq!(
        secret(&mut mods, &mut world),
        0.0,
        "the Workshop item must not receive the legitimate mod's saved state"
    );
    assert_ne!(
        world.position_of("vault"),
        Some(Vec3::new(5.0, 2.0, 3.0)),
        "nor its saved entities"
    );
    let refused = refused.expect_err("the second claim of the id is refused");
    assert_eq!(refused.code.code(), "lua_id_taken", "{refused:?}");

    // The legitimate mod's entry is not lost: it rides along in the next save...
    let (resaved, _) = mods.save_state(&mut world);
    // ...and the legitimate mod, alone again, gets it back.
    let mut world = TestWorld::default();
    let mut alone = host();
    alone.load(legit(), &mut world).expect("loads");
    alone.restore_state(Some(&resaved), &mut world);
    assert_eq!(secret(&mut alone, &mut world), 5.0);
    assert_eq!(world.position_of("vault"), Some(Vec3::new(5.0, 2.0, 3.0)));
}

#[test]
fn saved_state_keeps_its_v1_manifest_id_and_a_legacy_save_still_loads() {
    // A save from before mount-bound ids: every entry is keyed by the manifest id. A mod read with
    // no mount (`from_map`) still saves that way.
    let legacy = save_after_five_ticks(mod_files(ID, "1.0.0", VAULT));
    assert_eq!(saved_keys(&legacy), [ID]);

    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(legit(), &mut world).expect("loads");
    assert_eq!(
        mods.get(ID).and_then(|module| module.mount()),
        Some("mods:local.vault"),
        "the fixture is a local mods: mount, not a Workshop item"
    );
    mods.restore_state(Some(&legacy), &mut world);
    assert_eq!(secret(&mut mods, &mut world), 5.0, "the legacy entry loads");
    assert_eq!(world.position_of("vault"), Some(Vec3::new(5.0, 2.0, 3.0)));
    mods.call_tick(&mut world, 0.1);
    let (bytes, _) = mods.save_state(&mut world);
    assert_eq!(
        saved_keys(&bytes),
        [ID],
        "v1 keeps manifest ids even for mounted mods"
    );
    let mut world = TestWorld::default();
    let mut next = host();
    next.load(legit(), &mut world).expect("loads");
    next.restore_state(Some(&bytes), &mut world);
    assert_eq!(secret(&mut next, &mut world), 6.0);
}

#[test]
fn a_legacy_entry_of_a_contested_id_goes_to_nobody_and_is_kept() {
    let legacy = save_after_five_ticks(mod_files(ID, "1.0.0", VAULT));

    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(impostor(), &mut world).expect("the first claim loads");
    assert!(
        mods.load(legit(), &mut world).is_err(),
        "the second claim is refused"
    );
    mods.restore_state(Some(&legacy), &mut world);
    assert_eq!(
        secret(&mut mods, &mut world),
        0.0,
        "while two mounts claim the id, the legacy entry is nobody's"
    );
    let (bytes, _) = mods.save_state(&mut world);
    assert_eq!(
        saved_keys(&bytes),
        [ID],
        "the original manifest entry wins over the impostor's current state"
    );

    // The player removes the Workshop item: the legitimate mod gets its state back.
    let mut world = TestWorld::default();
    let mut alone = host();
    alone.load(legit(), &mut world).expect("loads");
    alone.restore_state(Some(&bytes), &mut world);
    assert_eq!(secret(&mut alone, &mut world), 5.0);
}

#[test]
fn workshop_alone_cannot_take_or_rekey_a_v1_manifest_entry() {
    let legacy = save_after_five_ticks(mod_files(ID, "1.0.0", VAULT));
    let original = decode_luastate(&legacy, &host().limits())
        .expect("decode")
        .mods
        .remove(0);
    for restore_first in [false, true] {
        let mut world = TestWorld::default();
        let mut mods = host();
        if restore_first {
            mods.restore_state(Some(&legacy), &mut world);
            mods.load(impostor(), &mut world).expect("loads");
        } else {
            mods.load(impostor(), &mut world).expect("loads");
            mods.restore_state(Some(&legacy), &mut world);
        }
        assert_eq!(
            secret(&mut mods, &mut world),
            0.0,
            "a Workshop mount alone must not receive a v1 manifest entry (restore_first={restore_first})"
        );
        assert_eq!(world.position_of("vault"), Some(Vec3::new(0.0, 2.0, 3.0)));
        let (bytes, report) = mods.save_state(&mut world);
        assert!(report.mods.iter().all(|line| line.error.is_none()), "{report:?}");
        let saved = decode_luastate(&bytes, &mods.limits()).expect("decode");
        assert_eq!(
            saved.mods.as_slice(),
            std::slice::from_ref(&original),
            "v1 preserves the original manifest entry exactly"
        );
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().expect("version")), 1);
        let mut alone = host();
        alone.load(legit(), &mut world).expect("loads");
        assert_eq!(
            alone.get(ID).and_then(|module| module.mount()),
            Some("mods:local.vault")
        );
        alone.restore_state(Some(&bytes), &mut world);
        assert_eq!(secret(&mut alone, &mut world), 5.0);
        assert_eq!(world.position_of("vault"), Some(Vec3::new(5.0, 2.0, 3.0)));
    }
}

#[test]
fn held_entry_beside_a_live_mod_does_not_fail_the_save_or_drop_the_held_entry() {
    let mut world = TestWorld::default();
    let mut source = host();
    source
        .load(mod_files(ID, "1.0.0", VAULT), &mut world)
        .expect("loads");
    source
        .run_chunk(&mut world, Some(ID), "regia.state.padding = string.rep('x', 600)")
        .expect("chunk");
    let legacy = source.save_state(&mut world).0;
    let mut mods = LuaMods::new(
        1234,
        regia_script_lua::LuaLimits {
            max_state_total_bytes: 1024,
            ..Default::default()
        },
    );
    mods.load(impostor(), &mut world).expect("loads");
    assert!(mods.load(legit(), &mut world).is_err());
    let restored = mods.restore_state(Some(&legacy), &mut world);
    assert!(restored.error.is_none(), "{restored:?}");
    assert_eq!(mods.held_state().collect::<Vec<_>>(), [ID]);
    mods.load(
        mod_files(
            "example.other",
            "1.0.0",
            "regia.state.padding = string.rep('y', 600)",
        ),
        &mut world,
    )
    .expect("loads");
    let (bytes, report) = mods.save_state(&mut world);
    assert!(
        !bytes.is_empty(),
        "a live mod that does not fit beside a held entry is dropped; the save must still succeed"
    );
    assert!(report.bytes > 0, "{report:?}");
    assert!(
        report.mods.iter().any(|line| {
            line.id == "example.other"
                && line
                    .error
                    .as_ref()
                    .is_some_and(|error| error.code == regia_script_lua::LuaErrorCode::TooLarge)
        }),
        "the live mod that does not fit uses the per-mod lua_too_large path: {report:?}"
    );
    let saved = decode_luastate(&bytes, &mods.limits()).expect("save decodes");
    let original = decode_luastate(&legacy, &host().limits()).expect("decode");
    assert!(
        saved.mods.contains(&original.mods[0]),
        "the save keeps the held entry"
    );
    assert!(
        saved.mods.iter().all(|entry| entry.id != "example.other"),
        "the live mod that does not fit is left out"
    );
    assert_eq!(mods.held_state().collect::<Vec<_>>(), [ID]);
    let (again, again_report) = mods.save_state(&mut world);
    assert!(!again.is_empty(), "the next save in the session also succeeds");
    assert!(again_report.bytes > 0, "{again_report:?}");
    mods.run_chunk(&mut world, Some("example.other"), "regia.state.padding = nil")
        .expect("chunk");
    let (retry, _) = mods.save_state(&mut world);
    let saved = decode_luastate(&retry, &mods.limits()).expect("retry decodes");
    assert!(
        saved.mods.contains(&original.mods[0]),
        "a later save still preserves the held entry"
    );
    assert!(
        saved.mods.iter().any(|entry| entry.id == "example.other"),
        "once the live mod fits, it is saved again"
    );
}

/// Held bytes alone over the cap still refuse that save. The entry stays held.
#[test]
fn held_entries_alone_over_the_cap_fail_the_save_without_dropping_them() {
    let mut world = TestWorld::default();
    let mut source = host();
    source
        .load(mod_files(ID, "1.0.0", VAULT), &mut world)
        .expect("loads");
    source
        .run_chunk(
            &mut world,
            Some(ID),
            "regia.state.padding = string.rep('x', 2000)",
        )
        .expect("chunk");
    let legacy = source.save_state(&mut world).0;
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
    mods.load(impostor(), &mut world).expect("loads");
    assert!(mods.restore_state(Some(&legacy), &mut world).error.is_none());
    assert_eq!(mods.held_state().collect::<Vec<_>>(), [ID]);
    let (bytes, report) = mods.save_state(&mut world);
    assert!(
        bytes.is_empty(),
        "held entries alone over the cap must not be omitted"
    );
    assert_eq!(report.bytes, 0);
    assert!(
        report.mods.iter().any(|line| line
            .error
            .as_ref()
            .is_some_and(|error| error.code == regia_script_lua::LuaErrorCode::TooLarge)),
        "{report:?}"
    );
    assert_eq!(mods.held_state().collect::<Vec<_>>(), [ID]);
}

/// One saved id, rewritten in place to the same length so the postcard framing stays valid.
fn replace_id(bytes: &[u8], from: &str, to: &str) -> Vec<u8> {
    assert_eq!(from.len(), to.len());
    let from = from.as_bytes();
    let to = to.as_bytes();
    let mut out = bytes.to_vec();
    let mut found = 0usize;
    let mut index = 0;
    while index + from.len() <= out.len() {
        if &out[index..index + from.len()] == from {
            out[index..index + from.len()].copy_from_slice(to);
            found += 1;
            index += from.len();
        } else {
            index += 1;
        }
    }
    assert_eq!(found, 1, "the manifest id should appear once in the blob");
    out
}

/// A v1 blob whose second entry id contains `:`. No v1 mod id can.
fn colon_blob() -> Vec<u8> {
    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(mod_files(ID, "1.0.0", VAULT), &mut world)
        .expect("loads");
    for _ in 0..5 {
        mods.call_tick(&mut world, 0.1);
    }
    mods.load(
        mod_files("example.other", "1.0.0", "regia.state.n = 7"),
        &mut world,
    )
    .expect("loads");
    let blob = mods.save_state(&mut world).0;
    let patched = replace_id(&blob, "example.other", "example:other");
    let decoded = decode_luastate(&patched, &host().limits()).expect("colon blob decodes");
    assert!(decoded.mods.iter().any(|entry| entry.id == "example:other"));
    assert!(decoded.mods.iter().any(|entry| entry.id == ID));
    patched
}

#[test]
fn local_mods_mount_restores_by_manifest_id_and_workshop_mount_is_held() {
    let legacy = save_after_five_ticks(mod_files(ID, "1.0.0", VAULT));

    let mut world = TestWorld::default();
    let mut local = host();
    local.load(legit(), &mut world).expect("local mods: loads");
    assert_eq!(
        local.get(ID).and_then(|module| module.mount()),
        Some("mods:local.vault")
    );
    // A `.fpak` archive uses the same loader and a mount id that is not `workshop.<id>`.
    let pak = mounted("pak.lights", "example.pak");
    local.load(pak, &mut world).expect("fpak-style mods: id loads");
    local.restore_state(Some(&legacy), &mut world);
    assert_eq!(
        secret(&mut local, &mut world),
        5.0,
        "a local mods: folder restores the v1 manifest entry"
    );
    assert!(
        local.held_state().next().is_none(),
        "a local mods: mount does not hold its own manifest entry"
    );
    local.call_tick(&mut world, 0.1);
    let (bytes, report) = local.save_state(&mut world);
    assert!(report.mods.iter().all(|line| line.error.is_none()), "{report:?}");
    assert_eq!(saved_keys(&bytes), ["example.pak", ID]);

    for restore_first in [false, true] {
        let mut world = TestWorld::default();
        let mut mods = host();
        if restore_first {
            mods.restore_state(Some(&legacy), &mut world);
            mods.load(impostor(), &mut world).expect("loads");
        } else {
            mods.load(impostor(), &mut world).expect("loads");
            mods.restore_state(Some(&legacy), &mut world);
        }
        assert_eq!(
            mods.get(ID).and_then(|module| module.mount()),
            Some("mods:workshop.999")
        );
        assert_eq!(
            secret(&mut mods, &mut world),
            0.0,
            "a Workshop mount is still held (restore_first={restore_first})"
        );
        assert_eq!(mods.held_state().collect::<Vec<_>>(), [ID]);
        let (bytes, report) = mods.save_state(&mut world);
        assert!(!bytes.is_empty(), "{report:?}");
        assert_eq!(saved_keys(&bytes), [ID]);
    }
}

#[test]
fn a_restored_colon_id_does_not_block_later_saves() {
    let patched = colon_blob();
    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(legit(), &mut world).expect("loads");
    let restored = mods.restore_state(Some(&patched), &mut world);
    assert!(restored.error.is_none(), "{restored:?}");
    assert_eq!(
        secret(&mut mods, &mut world),
        5.0,
        "the manifest entry still loads"
    );
    let (bytes, report) = mods.save_state(&mut world);
    assert!(
        !bytes.is_empty(),
        "a restored id containing ':' must not fail the save: {report:?}"
    );
    assert!(report.bytes > 0, "{report:?}");
    assert!(
        report.mods.iter().any(|line| {
            line.id == "example:other"
                && line.bytes == 0
                && line
                    .error
                    .as_ref()
                    .is_some_and(|error| error.message.contains("':'"))
        }),
        "the colon entry is skipped with a warning: {report:?}"
    );
    let saved = decode_luastate(&bytes, &mods.limits()).expect("decodes");
    assert!(saved.mods.iter().all(|entry| !entry.id.contains(':')));
    assert!(saved.mods.iter().any(|entry| entry.id == ID));
    let (again, again_report) = mods.save_state(&mut world);
    assert!(!again.is_empty(), "the next save also succeeds: {again_report:?}");
    assert!(
        again_report.mods.iter().all(|line| !line.id.contains(':')),
        "the quarantined entry does not fail a later save: {again_report:?}"
    );
}

#[test]
fn a_restored_colon_id_does_not_block_game_saves() {
    let patched = colon_blob();
    let mut world = TestWorld::default();
    let mut mods = host();
    mods.load(legit(), &mut world).expect("loads");
    assert!(mods.restore_state(Some(&patched), &mut world).error.is_none());
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, LuaModsPlugin::new(Vec::new())));
    app.world_mut().run_schedule(Startup);
    app.insert_resource(mods);
    let dir = regia_test_utils::unique_temp_dir("lua-colon-id-save");
    let slot = SaveGame::slot(&SaveDirectory(dir.clone()), "Test");
    SaveGame::save(&slot, app.world_mut(), &SaveSpec::default())
        .expect("a restored ':' id must not fail the game save");
    SaveGame::save(&slot, app.world_mut(), &SaveSpec::default())
        .expect("the next game save in the session also succeeds");
    let report = app.world().resource::<LuaMods>().last_save().expect("a save ran");
    assert!(report.bytes > 0, "{report:?}");
    assert!(
        report.mods.iter().all(|line| !line.id.contains(':')),
        "the quarantined entry is not still blocking saves: {report:?}"
    );
    std::fs::remove_dir_all(&dir).expect("cleanup");
}
