// SPDX-License-Identifier: MPL-2.0
//! P-145: the selection bar's bulk actions are reachable and operable from the keyboard (Tab to
//! the button, Space presses it): Edit selection in grid opens the Bulk Edit grid on the selection.

use regia_app::prelude::{Reflect, ReflectDefault};
use regia_ecs::prelude::*;
use regia_ecs::reflect::AppTypeRegistry;

use crate::canvas_keys::test_input::{frame, key, tab_to};

/// A data asset type for the grid.
#[derive(Reflect, Default, Clone, PartialEq, Debug)]
#[reflect(Default)]
struct Crate {
    weight: u32,
}
impl regia_asset::data_asset::Asset for Crate {}

#[test]
fn the_selection_bar_works_from_the_keyboard() {
    let root = regia_test_utils::unique_temp_dir("content_selection_bar_keys");
    let folder = root.join("assets/crates");
    std::fs::create_dir_all(&folder).expect("dir");
    let kind = std::any::type_name::<Crate>();
    for (name, weight) in [("a", 1), ("b", 2), ("c", 3)] {
        let text = format!("DataAsset(format_version: 1, type: \"{kind}\", value: (weight: {weight}))\n");
        std::fs::write(folder.join(format!("{name}.data.ron")), text).expect("write");
    }
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let mut world = std::mem::take(app.world_mut());
    {
        let registry = world.get_resource_or_insert_with(AppTypeRegistry::default);
        let mut registry = registry.write();
        registry.register::<Crate>();
        registry.register::<u32>();
        registry.register_type_data::<Crate, regia_asset::data_asset::ReflectAsset>();
    }
    world.get_resource_or_insert_with(crate::command::UndoStack::new);
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    world
        .get_resource_or_insert_with(crate::content_actions::ContentSelection::default)
        .set([
            "assets/crates/a.data.ron".to_string(),
            "assets/crates/c.data.ron".to_string(),
        ]);
    let mut open = crate::level_io::new_level(&mut world);
    let mut state = crate::content::ContentBrowserState {
        project_root: Some(root.clone()),
        folder: "assets/crates".to_string(),
        ..Default::default()
    };
    let ctx = egui::Context::default();
    let size = [1400.0, 900.0];
    let mut draw = |ui: &mut egui::Ui| {
        egui::CentralPanel::default().show(ui, |ui| {
            crate::content::content_browser_panel(ui, &mut world, &mut state, &mut open);
        });
    };
    frame(&ctx, size, Vec::new(), &mut draw);
    frame(&ctx, size, Vec::new(), &mut draw);
    tab_to(&ctx, size, "Edit selection in grid", false, 200, &mut draw);
    frame(
        &ctx,
        size,
        vec![key(egui::Key::Space, egui::Modifiers::NONE)],
        &mut draw,
    );
    frame(&ctx, size, Vec::new(), &mut draw);
    drop(draw);
    let rows = world
        .get_resource::<crate::bulk_edit_panel::BulkEdit>()
        .and_then(|panel| panel.grid.as_ref())
        .map(|grid| grid.rows.clone())
        .unwrap_or_default();
    assert_eq!(
        rows,
        [
            crate::bulk_edit_panel::RowKey::Asset("assets/crates/a.data.ron".into()),
            crate::bulk_edit_panel::RowKey::Asset("assets/crates/c.data.ron".into()),
        ],
        "Space on the focused button opened the grid on the selection"
    );
    let _ = std::fs::remove_dir_all(&root);
}
