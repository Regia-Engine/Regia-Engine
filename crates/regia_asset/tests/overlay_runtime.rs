// SPDX-License-Identifier: MPL-2.0
//! Overlay asset roots at runtime (ADR-8845): a source the game reads directly is found in the
//! project's `assets/` first, then in the overlay roots `[cook.overlay_roots]` names, earliest
//! first (the order the project cook fills from, ADR-8155).

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_asset::data_table::{DataTableAppExt, DataTables};
use regia_asset::{AssetServer, AssetServerSettings, SourceAssetChanged};
use serde::{Deserialize, Serialize};

#[derive(Reflect, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
struct OverlayRow {
    damage: u32,
}

const TABLE: &str = "tables/weapons.table.ron";

fn table(damage: u32) -> String {
    format!(
        "(\n    format_version: 1,\n    row_type: \"OverlayRow\",\n    rows: {{\n        \"rifle\": (damage: {damage}),\n    }},\n)\n"
    )
}

fn put(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).expect("write");
    std::fs::rename(&tmp, path).expect("rename");
}

/// A project whose `Regia.toml` lists `layers/base` then `layers/hd` (both created).
fn project(tag: &str) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir(tag);
    put(
        &root.join("Regia.toml"),
        "[project]\nname = \"overlay\"\n\n[cook.overlay_roots]\npaths = [\"layers/base\", \"layers/hd\"]\n",
    );
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    std::fs::create_dir_all(root.join("layers/base")).expect("base");
    std::fs::create_dir_all(root.join("layers/hd")).expect("hd");
    root
}

fn open_server(root: &Path) -> AssetServer {
    AssetServer::open(AssetServerSettings {
        project_root: root.to_path_buf(),
        platform: "linux".to_string(),
        content_dir: None,
    })
    .expect("server")
}

fn app_with_table(root: &Path) -> App {
    let mut app = App::new();
    app.insert_resource(open_server(root));
    app.register_data_table::<OverlayRow>(TABLE);
    app.update();
    app
}

fn damage(app: &App) -> Option<u32> {
    let tables = app.world().resource::<DataTables>();
    tables
        .get::<OverlayRow>(TABLE)
        .and_then(|typed| typed.get("rifle").map(|row| row.damage))
}

#[test]
fn a_table_only_in_an_overlay_root_loads_at_runtime() {
    let root = project("overlay_runtime_only");
    put(&root.join("layers/hd").join(TABLE), &table(41));
    let app = app_with_table(&root);
    assert_eq!(damage(&app), Some(41), "the overlay copy loads");
    let tables = app.world().resource::<DataTables>();
    let slot = tables.slot(TABLE).expect("slot");
    assert_eq!(
        slot.file.as_deref(),
        Some(root.join("layers/hd").join(TABLE).as_path())
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_project_then_the_earliest_overlay_wins_as_in_the_cook() {
    let root = project("overlay_runtime_order");
    put(&root.join("layers/base").join(TABLE), &table(1));
    put(&root.join("layers/hd").join(TABLE), &table(2));
    let app = app_with_table(&root);
    assert_eq!(damage(&app), Some(1), "the earliest overlay root wins");

    put(&root.join("assets").join(TABLE), &table(7));
    let app = app_with_table(&root);
    assert_eq!(
        damage(&app),
        Some(7),
        "a file in the project wins, as in the cook"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_overlay_table_reloads_when_its_file_changes() {
    let root = project("overlay_runtime_reload");
    let file = root.join("layers/hd").join(TABLE);
    put(&file, &table(10));
    let mut app = app_with_table(&root);
    assert_eq!(damage(&app), Some(10));
    put(&file, &table(11));
    app.world_mut()
        .write_message(SourceAssetChanged::new(file.clone()));
    app.update();
    assert_eq!(damage(&app), Some(11), "the overlay file reloads");

    // A copy in a later root is shadowed by the earlier one: its change is not this table's.
    put(&root.join("layers/base").join(TABLE), &table(20));
    let mut app = app_with_table(&root);
    assert_eq!(damage(&app), Some(20));
    put(&file, &table(12));
    app.world_mut()
        .write_message(SourceAssetChanged::new(file.clone()));
    app.update();
    assert_eq!(
        damage(&app),
        Some(20),
        "a shadowed overlay copy does not replace the rows"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn without_overlay_roots_only_assets_is_searched() {
    let root = regia_test_utils::unique_temp_dir("overlay_runtime_off");
    put(&root.join("Regia.toml"), "[project]\nname = \"plain\"\n");
    put(&root.join("layers/hd").join(TABLE), &table(3));
    let app = app_with_table(&root);
    assert_eq!(damage(&app), None);
    let _ = std::fs::remove_dir_all(&root);
}
