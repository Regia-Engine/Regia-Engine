// SPDX-License-Identifier: MPL-2.0
//! The sprint's editor features end to end, through the installed agent methods and real panel
//! draws (ADR-866): the Bake panel's states, the budget meters in the prefab banner, the Content
//! Browser index on a 2,000-asset folder, and outliner moves with undo. No GPU, no CLI, no clock:
//! bakes come from canned CLI output and costs are checked with the index's counters.

use std::path::Path;

use regia_ecs::prelude::*;
use regia_editor::bake_runner::{BakeFailure, BakeJob, BakePanel, BakeRequest, bake_runner_panel};
use regia_editor::budget::prefab_budget_ui;
use regia_editor::content::{ContentBrowserState, content_browser_panel};
use regia_editor::content_index::ContentIndex;
use regia_editor::lightmap_panel::{LightmapPanel, LightmapRequest, lightmap_panel};
use regia_render::MeshSource;
use regia_scene::prelude::{Name, Transform};
use serde_json::{Value, json};

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let (_, handler) = regia_editor::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("{method} is installed"));
    handler(world, &params)
}

/// Text drawn in one egui frame.
fn frame_text(ctx: &egui::Context, mut draw: impl FnMut(&mut egui::Ui)) -> String {
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
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1200.0, 900.0),
        )),
        ..Default::default()
    };
    let mut output = ctx.run_ui(raw, |ui| {
        egui::CentralPanel::default().show(ui, |ui| draw(ui));
    });
    output.textures_delta.clear();
    let mut out = String::new();
    for clipped in &output.shapes {
        walk(&clipped.shape, &mut out);
    }
    out
}

fn f32s(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// A GLB with one node of `triangles` non-indexed triangles and one material.
fn glb(triangles: usize) -> Vec<u8> {
    let floats: Vec<f32> = (0..triangles)
        .flat_map(|i| {
            let z = i as f32;
            [0.0, 0.0, z, 1.0, 0.0, z, 0.0, 1.0, z]
        })
        .collect();
    let mut bin = f32s(&floats);
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let doc = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "name": "Rock", "mesh": 0 }],
        "meshes": [{ "primitives": [{ "attributes": { "POSITION": 0 }, "material": 0 }] }],
        "materials": [{ "name": "Stone" }],
        "accessors": [{ "bufferView": 0, "componentType": 5126, "count": triangles * 3, "type": "VEC3",
                        "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, triangles.saturating_sub(1) as f32] }],
        "bufferViews": [{ "buffer": 0, "byteOffset": 0, "byteLength": bin.len() }],
        "buffers": [{ "byteLength": bin.len() }]
    });
    let mut text = serde_json::to_vec(&doc).expect("json");
    while !text.len().is_multiple_of(4) {
        text.push(b' ');
    }
    let total = 12 + 8 + text.len() + 8 + bin.len();
    let mut out = Vec::new();
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&text);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin);
    out
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    regia_test_utils::write_atomic(path, bytes).expect("write");
}

const DRY_RUN: &str = r#"{"ok": true, "dry_run": true, "scene": "furnace",
  "layers": {"terrain": null, "bricks": [{"index": 0}], "probes": 27, "valid": null, "invalid": null}, "seconds": 0.01}"#;
const DESC_ERROR: &str = r#"{"ok": false, "code": "bake_desc_invalid", "path": "layout.auto_bricks.spacing",
  "message": "spacing must be > 0"}"#;

fn canned(_program: &Path, request: &BakeRequest, _cwd: Option<&Path>) -> Result<BakeJob, BakeFailure> {
    let stdout = if request.scene.ends_with("bad.bake.ron") {
        DESC_ERROR
    } else {
        DRY_RUN
    };
    Ok(BakeJob::from_readers(
        request.clone(),
        std::io::Cursor::new(stdout.as_bytes().to_vec()),
        std::io::Cursor::new(b"  layout: 27/27 probes\n".to_vec()),
    ))
}

#[test]
fn the_bake_panel_shows_what_the_agent_started() {
    let mut world = World::new();
    let mut panel = BakePanel::default();
    panel.launcher = canned;
    world.insert_resource(panel);
    let ctx = egui::Context::default();
    let idle = frame_text(&ctx, |ui| bake_runner_panel(ui, &mut world));
    assert!(idle.contains("Bake GI"), "{idle}");
    assert!(idle.contains("Validate") && idle.contains("Cancel"), "{idle}");

    let status = call(
        &mut world,
        "editor.bake_gi",
        json!({ "scene": "furnace", "dry_run": true, "wait": true }),
    )
    .expect("editor.bake_gi");
    assert_eq!(status["state"], "succeeded");
    let done = frame_text(&ctx, |ui| bake_runner_panel(ui, &mut world));
    assert!(done.contains("Description is valid"), "{done}");
    assert!(done.contains("27"), "the probe count is shown:\n{done}");

    let failed = call(
        &mut world,
        "editor.bake_gi",
        json!({ "scene": "/p/bad.bake.ron", "dry_run": true, "wait": true }),
    )
    .expect("the call succeeds; the bake failed");
    assert_eq!(failed["error"]["code"], "bake_desc_invalid");
    let text = frame_text(&ctx, |ui| bake_runner_panel(ui, &mut world));
    assert!(text.contains("Bake failed (bake_desc_invalid)"), "{text}");
    assert!(text.contains("layout.auto_bricks.spacing"), "{text}");
    assert_eq!(
        call(&mut world, "editor.bake_status", json!({})).expect("status")["job"],
        2
    );
}

const LIGHTMAP_DRY_RUN: &str = r#"{"ok": true, "dry_run": true,
  "atlas": {"width": 64, "height": 32, "fit_scale": 0.8},
  "settings": {"density": 8.0},
  "entries": [{"name": "entity_a_0", "label": "wall (a)", "rect": [0, 0, 32, 32], "texels_per_metre": 6.4}]}"#;
const LIGHTMAP_ERROR: &str =
    r#"{"ok": false, "code": "export_no_input", "path": null, "message": "no static mesh"}"#;

fn canned_lightmap(
    _program: &Path,
    request: &LightmapRequest,
    _cwd: Option<&Path>,
) -> Result<BakeJob, BakeFailure> {
    let stdout = if request.source.ends_with("bad.level.ron") {
        LIGHTMAP_ERROR
    } else {
        LIGHTMAP_DRY_RUN
    };
    Ok(BakeJob::from_readers(
        BakeRequest::new(request.source.clone()),
        std::io::Cursor::new(stdout.as_bytes().to_vec()),
        std::io::Cursor::new(b"  lightmap: 5/5 texels\n".to_vec()),
    ))
}

/// The Lightmaps panel through the installed methods and a real draw (ADR-2024, headless): what an
/// agent starts is what the panel shows.
#[test]
fn the_lightmaps_panel_shows_what_the_agent_started() {
    let mut world = World::new();
    let mut panel = LightmapPanel::default();
    panel.launcher = canned_lightmap;
    world.insert_resource(panel);
    let ctx = egui::Context::default();
    let idle = frame_text(&ctx, |ui| lightmap_panel(ui, &mut world));
    assert!(idle.contains("Lightmaps"), "{idle}");
    assert!(
        idle.contains("Lay out") && idle.contains("Bake") && idle.contains("Cancel"),
        "{idle}"
    );
    assert!(idle.contains("the open level"), "{idle}");

    let status = call(
        &mut world,
        "editor.lightmap_bake",
        json!({ "source": "yard.level.ron", "dry_run": true, "wait": true }),
    )
    .expect("editor.lightmap_bake");
    assert_eq!(status["state"], "succeeded");
    assert_eq!(status["view"]["fit_scale"].as_f64().map(|f| f as f32), Some(0.8));
    let done = frame_text(&ctx, |ui| lightmap_panel(ui, &mut world));
    assert!(done.contains("Atlas laid out"), "{done}");
    assert!(done.contains("Atlas 64×32, 1 entries"), "{done}");
    assert!(done.contains("density lowered to 80 %"), "{done}");

    let failed = call(
        &mut world,
        "editor.lightmap_bake",
        json!({ "source": "bad.level.ron", "dry_run": true, "wait": true }),
    )
    .expect("the call succeeds; the bake failed");
    assert_eq!(failed["error"]["code"], "export_no_input");
    let text = frame_text(&ctx, |ui| lightmap_panel(ui, &mut world));
    assert!(text.contains("Bake failed (export_no_input)"), "{text}");
    assert_eq!(
        call(&mut world, "editor.lightmap_status", json!({})).expect("status")["job"],
        2
    );
}

#[test]
fn the_prefab_banner_shows_the_scene_budget() {
    let dir = regia_test_utils::unique_temp_dir("editor_features_budget");
    let rock = dir.join("rock.glb");
    write(&rock, &glb(10));
    let mut world = World::new();
    for _ in 0..2 {
        world.spawn((
            Transform::default(),
            MeshSource::new(rock.display().to_string(), ""),
        ));
    }
    let ctx = egui::Context::default();
    let text = frame_text(&ctx, |ui| {
        ui.horizontal(|ui| prefab_budget_ui(ui, &mut world));
    });
    assert!(text.contains("Budget prop · 20/20000 tris"), "{text}");
    let reply = call(
        &mut world,
        "editor.budget",
        json!({ "limits": { "max_materials": 1 } }),
    )
    .expect("budget");
    assert_eq!(reply["budget"]["instances"], 2);
    let materials = reply["budget"]["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["key"] == "materials"))
        .cloned()
        .expect("materials row");
    assert_eq!(
        materials["value"], 1,
        "two instances of one model share its material"
    );
    assert_eq!(materials["status"], "near", "1 of 1 is at the limit, not over");
    let _ = std::fs::remove_dir_all(&dir);
}

fn browser_world(root: &Path) -> (World, ContentBrowserState) {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let mut world = std::mem::take(app.world_mut());
    let open = regia_editor::level_io::new_level(&mut world);
    world.insert_resource(open);
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(root.to_path_buf()),
    });
    let state = ContentBrowserState {
        project_root: Some(root.to_path_buf()),
        folder: "assets/props".to_string(), // the index stores `/` paths on every platform
        ..Default::default()
    };
    (world, state)
}

#[test]
fn the_content_browser_scans_once_and_draws_only_visible_rows() {
    let root = regia_test_utils::unique_temp_dir("editor_features_content");
    for i in 0..2_000 {
        write(&root.join(format!("assets/props/prop_{i:04}.glb")), b"x");
    }
    write(&root.join("assets/levels/main.level.ron"), b"()");
    let (mut world, mut state) = browser_world(&root);
    let ctx = egui::Context::default();
    let mut last = String::new();
    for _ in 0..20 {
        let mut open = world
            .remove_resource::<regia_editor::level_io::OpenLevel>()
            .expect("open level");
        last = frame_text(&ctx, |ui| {
            content_browser_panel(ui, &mut world, &mut state, &mut open)
        });
        world.insert_resource(open);
    }
    let index = world.resource::<ContentIndex>();
    assert_eq!(index.scans, 1, "twenty frames, one walk of the tree");
    assert_eq!(index.views_built, 1, "an unchanged folder reuses its view");
    assert!(
        last.contains("2000 assets"),
        "the header counts the whole folder:\n{last}"
    );
    let rows = last.lines().filter(|line| line.contains("prop_")).count();
    assert!(
        rows > 0 && rows < 200,
        "only the rows in view are drawn, not all 2000: {rows}"
    );

    let reply = call(&mut world, "editor.content_index", json!({ "search": "prop_19" })).expect("index");
    assert_eq!(reply["visible"], 100, "prop_1900..prop_1999");
    assert_eq!(reply["scans"], 1, "the agent reads the panel's index");
    assert_eq!(reply["assets"], 2_001);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn outliner_moves_go_through_the_agent_and_undo_in_one_step() {
    let mut world = World::new();
    let holder = world
        .spawn((
            Name::new("Holder"),
            Transform::from_translation(regia_core::math::Vec3::new(5.0, 0.0, 0.0)),
        ))
        .id();
    let a = world.spawn((Name::new("A"), Transform::default())).id();
    let b = world.spawn((Name::new("B"), Transform::default())).id();
    let reply = call(
        &mut world,
        "editor.reparent",
        json!({ "entities": [a.to_bits(), b.to_bits()], "parent": holder.to_bits() }),
    )
    .expect("editor.reparent");
    assert_eq!(reply["moved"], json!([a.to_bits(), b.to_bits()]), "both moved");
    assert_eq!(reply["undoable"], 1, "two entities, one undo entry");
    for row in reply["entities"].as_array().expect("rows") {
        assert_eq!(row["translation"][0], -5.0, "local is relative to the holder");
        assert_eq!(row["world_translation"][0], 0.0, "world pose kept");
    }
    let undo = call(&mut world, "editor.undo", json!({})).expect("editor.undo");
    assert_eq!(undo["undone"], true);
    assert!(world.get::<ChildOf>(a).is_none() && world.get::<ChildOf>(b).is_none());
    assert_eq!(
        world.get::<Transform>(a).map(|t| t.translation.x),
        Some(0.0),
        "undo restores the local transform"
    );
    let cycle = call(
        &mut world,
        "editor.reparent",
        json!({ "entities": [holder.to_bits()], "parent": holder.to_bits() }),
    )
    .expect_err("self-parenting");
    assert_eq!(
        cycle.data.and_then(|data| data.get("reparent_error").cloned()),
        Some(json!("reparent_cycle"))
    );
}
