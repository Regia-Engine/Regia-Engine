// SPDX-License-Identifier: MPL-2.0
//! Content Browser navigation, filters and collections through the installed agent methods and the
//! real panel (ADR-7759): every new method is reached through `agent_ext::all_methods`, and the
//! panel draws the breadcrumbs, the tree and a collection's members.

use std::path::Path;

use regia_ecs::prelude::*;
use regia_editor::content::{ContentBrowserState, content_browser_panel};
use serde_json::{Value, json};

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let (_, handler) = regia_editor::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("{method} is installed"));
    handler(world, &params)
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    regia_test_utils::write_atomic(path, bytes).expect("write");
}

fn project() -> std::path::PathBuf {
    let root = regia_test_utils::unique_temp_dir("content_nav_e2e");
    for rel in [
        "assets/props/rocks/boulder.glb",
        "assets/props/crate.glb",
        "assets/tex/moss.png",
        "assets/maps/arena.level.ron",
    ] {
        write(&root.join(rel), b"x");
    }
    write(
        &root.join("assets/maps/arena.level.ron"),
        b"(entities: [(mesh: \"assets/props/crate.glb\")])",
    );
    write(
        &root.join("config/asset_tags.ron"),
        b"(format_version: 1, rules: [(paths: [\"assets/props/**\"], tags: {\"biome\": \"desert\"})])",
    );
    root
}

fn world_for(root: &Path) -> World {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let mut world = std::mem::take(app.world_mut());
    let open = regia_editor::level_io::new_level(&mut world);
    world.insert_resource(open);
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(root.to_path_buf()),
    });
    world
}

/// Text drawn by one frame of the panel.
fn draw(world: &mut World, state: &mut ContentBrowserState, ctx: &egui::Context) -> String {
    fn walk(shape: &egui::Shape, out: &mut String) {
        match shape {
            egui::Shape::Text(text) => {
                out.push_str(text.galley.text());
                out.push('\n');
            }
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
            _ => {}
        }
    }
    let mut open = world
        .remove_resource::<regia_editor::level_io::OpenLevel>()
        .expect("open level");
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1200.0, 900.0),
        )),
        ..Default::default()
    };
    let mut output = ctx.run_ui(raw, |ui| {
        egui::CentralPanel::default().show(ui, |ui| content_browser_panel(ui, world, state, &mut open));
    });
    world.insert_resource(open);
    output.textures_delta.clear();
    let mut out = String::new();
    for clipped in &output.shapes {
        walk(&clipped.shape, &mut out);
    }
    out
}

#[test]
fn the_agent_drives_navigation_filters_and_collections_end_to_end() {
    let root = project();
    let mut world = world_for(&root);
    let mut state = ContentBrowserState {
        project_root: Some(root.clone()),
        ..Default::default()
    };
    let ctx = egui::Context::default();
    draw(&mut world, &mut state, &ctx);

    // Navigation: the agent picks a folder, the panel shows it with its breadcrumbs.
    let view = call(
        &mut world,
        "editor.content_view",
        json!({ "folder": "assets/props/rocks" }),
    )
    .expect("view");
    assert_eq!(
        view["breadcrumbs"],
        json!(["assets", "assets/props", "assets/props/rocks"])
    );
    assert_eq!(view["grid"], false, "the view fields are still there");
    let text = draw(&mut world, &mut state, &ctx);
    assert_eq!(state.folder, "assets/props/rocks");
    assert!(text.contains("assets/props/rocks · 1 assets"), "{text}");
    assert!(text.contains("boulder.glb") && text.contains("rocks"), "{text}");
    let back = call(&mut world, "editor.content_view", json!({ "back": true })).expect("back");
    assert_eq!(back["folder"], "assets");
    draw(&mut world, &mut state, &ctx);
    assert_eq!(state.folder, "");

    // Filters: chips combine with words; the index answers the same words.
    let filtered = call(
        &mut world,
        "editor.content_filter",
        json!({ "tags": ["biome=desert"], "kinds": ["mesh"] }),
    )
    .expect("filter");
    assert_eq!(filtered["words"], "type:mesh tag:biome=desert");
    assert_eq!(
        filtered["paths"],
        json!(["assets/props/crate.glb", "assets/props/rocks/boulder.glb"])
    );
    assert_eq!(filtered["tag_choices"], json!(["biome=desert"]));
    let text = draw(&mut world, &mut state, &ctx);
    assert!(text.contains("2 assets"), "the panel applies the chips:\n{text}");
    call(&mut world, "editor.content_filter", json!({ "clear": true })).expect("clear");
    let index = call(
        &mut world,
        "editor.content_index",
        json!({ "search": "is:unreferenced" }),
    )
    .expect("index");
    assert_eq!(
        index["paths"]
            .as_array()
            .map(|p| p.iter().map(|v| v["path"].clone()).collect::<Vec<_>>()),
        Some(vec![
            json!("assets/maps/arena.level.ron"),
            json!("assets/props/rocks/boulder.glb"),
            json!("assets/tex/moss.png")
        ]),
        "{index}"
    );
    assert_eq!(index["reference_scans"], 1);
    let err = call(
        &mut world,
        "editor.content_index",
        json!({ "search": "is:dirty" }),
    )
    .expect_err("bad state");
    assert_eq!(
        err.data.as_ref().and_then(|d| d.get("index_error")),
        Some(&json!("search_bad_state"))
    );

    // Favorites and collections.
    let favorites = call(
        &mut world,
        "editor.content_favorites",
        json!({ "add": "assets/tex" }),
    )
    .expect("favorites");
    assert_eq!(favorites["favorites"], json!(["assets/tex"]));
    call(&mut world, "editor.collection_create", json!({ "name": "Picks" })).expect("create");
    let added = call(
        &mut world,
        "editor.collection_add",
        json!({ "name": "Picks", "paths": ["assets/tex/moss.png"] }),
    )
    .expect("add");
    assert_eq!(added["added"], 1);
    call(
        &mut world,
        "editor.content_view",
        json!({ "collection": "picks" }),
    )
    .expect("show collection");
    let text = draw(&mut world, &mut state, &ctx);
    assert!(text.contains("Collection Picks · 1 assets"), "{text}");
    assert!(text.contains("moss.png"), "{text}");
    let removed = call(
        &mut world,
        "editor.collection_remove",
        json!({ "name": "Picks", "paths": ["assets/tex/moss.png"] }),
    )
    .expect("remove");
    assert_eq!(removed["removed"], 1);
    let listed = call(&mut world, "editor.collection_list", json!({})).expect("list");
    assert_eq!(listed["collections"][0]["count"], 0);
    call(&mut world, "editor.collection_delete", json!({ "name": "Picks" })).expect("delete");
    assert!(
        root.join("assets/tex/moss.png").is_file(),
        "collections never move or delete files"
    );
    let _ = std::fs::remove_dir_all(&root);
}
