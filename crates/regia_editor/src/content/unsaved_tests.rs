// SPDX-License-Identifier: MPL-2.0
//! P-147: an asset whose open document has unsaved edits shows the unsaved dot on its Content
//! Browser row and tile, and says so in its accessible name.

use std::path::Path;

use regia_ecs::prelude::*;

use crate::canvas_keys::test_input::frame;

fn names(update: &egui::accesskit::TreeUpdate) -> Vec<String> {
    update
        .nodes
        .iter()
        .filter_map(|(_, node)| node.label().map(str::to_string))
        .collect()
}

#[test]
fn a_dirty_asset_shows_its_marker() {
    let root = regia_test_utils::unique_temp_dir("content_unsaved_marker");
    std::fs::create_dir_all(root.join("assets/levels")).expect("levels");
    let level_file = root.join("assets/levels/arena.level.ron");
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let mut world = std::mem::take(app.world_mut());
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    // The open level has a file in the project and unsaved edits.
    let mut open = crate::level_io::new_level(&mut world);
    open.path = Some(level_file.clone());
    world.insert_resource(open.clone());
    let saved = crate::agent_ext::editor_save_level(&mut world, &serde_json::json!({}));
    assert!(saved.is_ok(), "{saved:?}");
    assert!(level_file.is_file());
    std::fs::write(
        root.join("assets/levels/clean.png"),
        crate::os_drop::tests::tiny_png(),
    )
    .expect("png");
    world
        .get_resource_or_insert_with(crate::command::UndoStack::new)
        .mark_dirty();
    let mut state = crate::content::ContentBrowserState {
        project_root: Some(root.clone()),
        folder: "assets/levels".to_string(),
        ..Default::default()
    };
    let ctx = egui::Context::default();
    let mut run = |world: &mut World| {
        frame(&ctx, [1200.0, 900.0], Vec::new(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                crate::content::content_browser_panel(ui, world, &mut state, &mut open);
            });
        })
    };
    let marked = "arena.level.ron, unsaved changes";
    for grid in [false, true] {
        world.insert_resource(crate::content::ContentView {
            grid,
            ..Default::default()
        });
        run(&mut world);
        let update = run(&mut world);
        let names = names(&update);
        assert!(
            names.iter().any(|n| n == marked),
            "grid {grid}: no row or tile named {marked:?} in {names:?}"
        );
        assert!(
            names
                .iter()
                .any(|n| n.contains("clean.png") && !n.contains("unsaved")),
            "grid {grid}: the clean asset has no marker: {names:?}"
        );
    }
    // Saved: the marker goes.
    world.resource_mut::<crate::command::UndoStack>().mark_saved();
    run(&mut world);
    let update = run(&mut world);
    let names = names(&update);
    assert!(
        !names.iter().any(|n| n.contains("unsaved changes")),
        "a saved asset has no marker: {names:?}"
    );
    let _ = std::fs::remove_dir_all(Path::new(&root));
}
