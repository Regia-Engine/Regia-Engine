// SPDX-License-Identifier: MPL-2.0
//! The lightmapped sample project `samples/render_lab/lightmap_room` (ADR-2635) bakes with
//! `regia bake-lightmaps --scene` through the real binary and opens with its lightmap installed.
//! No baked file is checked in: this test bakes a temporary copy (CPU, quick settings), checks the
//! file stays small and keys the static entities only, and opens the level with the framework's
//! `LightmapLevelPlugin`, as `regia run --level` and the editor do.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use regia_app::prelude::*;
use regia_asset::lightmap::{Lightmap, LightmapEntryKey};
use regia_core::id::Guid;
use regia_framework::lightmap_level::{LevelLightmapState, LightmapLevelPlugin};
use regia_render::lightmap::InstalledLightmap;
use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};
use regia_scene::prelude::ScenePlugin;
use serde_json::Value;

const LEVEL: &str = "assets/levels/room.level.ron";
const OUT: &str = "assets/maps/room.flmap";
const ROOM: &str = "00000000-0000-0000-0000-00000000a001";
const PLINTH: &str = "00000000-0000-0000-0000-00000000a002";
const VISITOR: &str = "00000000-0000-0000-0000-00000000a003";
/// The checked-in size rule for samples (no binary over 2 MiB); the bake is not checked in, but a
/// sample that needs a bigger one than this is not small.
const MAX_BYTES: u64 = 2 * 1024 * 1024;
/// Quick on the CPU in a debug build.
const QUICK: [&str; 8] = [
    "--density",
    "2",
    "--samples",
    "8",
    "--bounces",
    "2",
    "--threads",
    "2",
];

/// Removes a directory tree when dropped (even on assertion failure).
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("dirs");
    for entry in std::fs::read_dir(from).expect("read sample") {
        let entry = entry.expect("entry");
        let dest = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).expect("copy");
        }
    }
}

fn sample() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../samples/render_lab/lightmap_room")
}

fn regia(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run regia")
}

fn guid(s: &str) -> Guid {
    s.parse().expect("guid")
}

#[test]
fn the_lightmap_sample_bakes_and_opens_with_its_lightmap() {
    // Baking in the sample itself (its README's steps) must not fail this test or reach the repo.
    let ignore = std::fs::read_to_string(sample().join(".gitignore")).expect("the sample's .gitignore");
    assert!(
        ignore.lines().any(|l| l.trim() == "assets/maps/"),
        "the sample's lightmap is baked, never checked in: {ignore}"
    );
    let tmp = Cleanup(regia_test_utils::unique_temp_dir("lightmap_sample"));
    let dir = &tmp.0;
    copy_tree(&sample(), dir);
    let _ = std::fs::remove_file(dir.join(OUT));

    let mut args = vec!["bake-lightmaps", "--scene", LEVEL];
    args.extend(QUICK);
    let out = regia(dir, &args);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert!(out.status.success(), "{v}");
    assert_eq!(v["ok"], true, "{v}");
    let file = dir.join(OUT);
    let bytes = std::fs::metadata(&file)
        .expect("the bake wrote the level's lightmap")
        .len();
    assert!(bytes <= MAX_BYTES, "{bytes} bytes");

    let map = Lightmap::read(&file).expect("the file reads back");
    for static_one in [ROOM, PLINTH] {
        assert!(
            map.entry_for_key(LightmapEntryKey {
                entity: guid(static_one),
                part: 0
            })
            .is_some(),
            "{static_one} is a keyed receiver"
        );
    }
    assert!(
        map.entries
            .iter()
            .all(|e| e.key.is_some_and(|k| k.entity != guid(VISITOR))),
        "the dynamic visitor keeps the probe light"
    );

    // Open the level as a game does: the plugin installs the file the level names.
    let mut app = App::new();
    app.add_plugins((ScenePlugin, LightmapLevelPlugin));
    regia_render::register_render_types(&mut app);
    app.insert_resource(
        regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: dir.clone(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("asset server"),
    );
    app.insert_resource(regia_render::RuntimeRenderer::default());
    let id = LevelId(Guid::from_u128(0x1a_0001));
    let text = std::fs::read_to_string(dir.join(LEVEL)).expect("level");
    let level = Level::from_ron_str(id, &text).expect("the sample level parses");
    app.world_mut().resource_mut::<LevelLibrary>().insert(level);
    open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
    app.update();
    let state = app.world().resource::<LevelLightmapState>();
    assert!(state.installed, "{:?} {:?}", state.error_code, state.error);
    assert_eq!(state.keyed_entries, map.entries.len());
    let installed = app
        .world()
        .resource::<InstalledLightmap>()
        .lightmap
        .clone()
        .expect("installed");
    assert_eq!(installed.entries, map.entries);
}
