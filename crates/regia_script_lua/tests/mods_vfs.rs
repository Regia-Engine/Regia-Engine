// SPDX-License-Identifier: MPL-2.0
//! Lua mods read from `mods://` (ADR-2418): `ModFiles::from_mods_vfs` takes the mod's own Lua files
//! through the confined mount, skips its other files, and loads like any other file set.

use std::collections::BTreeMap;

use regia_asset::mods_vfs::{ModSource, ModsVfs};
use regia_script_lua::{LuaLimits, LuaMods, ModFiles, NoWorld, ScriptValue};

#[test]
fn a_lua_mod_loads_from_its_mods_mount() {
    let mut files = BTreeMap::new();
    files.insert("luamod.ron".to_owned(), br#"(format_version: 1, id: "example.units", name: "Units", version: "1", entry: "main.lua", api_version: 1)"#.to_vec());
    files.insert(
        "main.lua".to_owned(),
        b"local u = require('lib/units') value = u.hp".to_vec(),
    );
    files.insert("lib/units.lua".to_owned(), b"return { hp = 250 }".to_vec());
    files.insert("textures/big.bin".to_owned(), vec![0xFF; 4096]);
    let mut vfs = ModsVfs::new();
    vfs.mount("example.units", ModSource::Memory(files))
        .expect("mount");
    // A later mod overriding main.lua at mods:// does not change this mod's own files.
    let mut over = BTreeMap::new();
    over.insert("main.lua".to_owned(), b"error('not mine')".to_vec());
    vfs.mount("other", ModSource::Memory(over)).expect("mount");

    let lua_files = ModFiles::from_mods_vfs(&vfs, "example.units", 1 << 20).expect("files");
    assert!(
        lua_files.get("textures/big.bin").is_none(),
        "only Lua files and the manifest"
    );
    assert!(lua_files.get("lib/units.lua").is_some());
    let mut mods = LuaMods::new(1, LuaLimits::default());
    mods.load(lua_files, &mut NoWorld).expect("loads");
    let outcome = mods
        .run_chunk(&mut NoWorld, Some("example.units"), "return value")
        .expect("runs");
    assert_eq!(outcome.results.first(), Some(&ScriptValue::Number(250.0)));

    assert!(ModFiles::from_mods_vfs(&vfs, "missing", 1 << 20).is_err());
    assert!(
        ModFiles::from_mods_vfs(&vfs, "example.units", 8).is_err(),
        "the size cap holds"
    );
}
