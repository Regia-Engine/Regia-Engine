// SPDX-License-Identifier: MPL-2.0
//! Hot reload routes a changed file to the longest registered asset path it ends with
//! (ADR-1809): `assets/anim/hit.chooser.ron` reloads `anim/hit.chooser.ron`, not a shorter
//! `hit.chooser.ron` registered first. Files live in a unique temp dir; no clocks.

use regia_app::prelude::*;
use regia_asset::SourceAssetChanged;
use regia_framework::chooser::{ChooserAppExt, Choosers};
use regia_framework::curve_table::{CurveTableAppExt, CurveTables};

const CHOOSER: &str = r#"(format_version: 1, result: Asset("montage"),
    rows: [(id: "front", result: Asset("anim/front.montage.ron"))])"#;

const CURVES: &str = r#"(format_version: 1, curves: { "falloff": (keys: [(0.0, 1.0), (10.0, 0.0)]) })"#;

fn write(dir: &std::path::Path, rel: &str, text: &str) -> std::path::PathBuf {
    let file = dir.join("assets").join(rel);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("dirs");
    regia_test_utils::write_atomic(&file, text.as_bytes()).expect("write");
    file
}

#[test]
fn a_changed_chooser_reloads_the_longest_matching_registration() {
    let dir = regia_test_utils::unique_temp_dir("chooser-reload-routing");
    let file = write(&dir, "anim/hit.chooser.ron", CHOOSER);
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.register_chooser("hit.chooser.ron");
    app.register_chooser("anim/hit.chooser.ron");
    app.update();
    app.world_mut().write_message(SourceAssetChanged::new(&file));
    app.update();
    let choosers = app.world().resource::<Choosers>();
    assert_eq!(
        choosers.generation("anim/hit.chooser.ron"),
        1,
        "the matching table loaded"
    );
    assert_eq!(
        choosers.generation("hit.chooser.ron"),
        0,
        "the shorter registration is untouched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_changed_curve_table_reloads_the_longest_matching_registration() {
    let dir = regia_test_utils::unique_temp_dir("curve-reload-routing");
    let file = write(&dir, "anim/falloff.curves.ron", CURVES);
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.register_curve_table("falloff.curves.ron");
    app.register_curve_table("anim/falloff.curves.ron");
    app.update();
    app.world_mut().write_message(SourceAssetChanged::new(&file));
    app.update();
    let tables = app.world().resource::<CurveTables>();
    assert_eq!(tables.generation("anim/falloff.curves.ron"), 1);
    assert_eq!(tables.generation("falloff.curves.ron"), 0);
    let _ = std::fs::remove_dir_all(&dir);
}
