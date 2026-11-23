// SPDX-License-Identifier: MPL-2.0
//! The 2D mode end to end through the agent methods and the real editor window: a map painted on
//! a two-color tileset shows those colors in the front pane (`docs/specs/86-2d-toolkit.md` § 8.10).
//! No golden: the test reads the pixels against the tile colors, on a headless GPU adapter.

use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_editor::mode2d::methods;
use regia_editor::selection::Selection;
use regia_editor::shell::EditorShell;
use serde_json::{Value, json};

const RED: [u8; 4] = [230, 20, 20, 255];
const BLUE: [u8; 4] = [20, 30, 230, 255];

const MAP: &str = r#"(
    version: 1,
    width: 8,
    height: 6,
    tile_width: 16,
    tile_height: 16,
    tilesets: [(
        name: "colors",
        first_gid: 1,
        image: "colors.png",
        image_width: 32,
        image_height: 16,
        tile_width: 16,
        tile_height: 16,
        columns: 2,
        tile_count: 2,
    )],
    layers: [Tiles((
        name: "ground",
        data: [
            0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0,
        ],
    ))],
)"#;

fn edit_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<Selection>()
        .init_resource::<regia_editor::viewport::EditorCamera>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>();
    let open = regia_editor::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    regia_editor::agent_ext::install(app.world_mut());
    app.update();
    app
}

fn call(app: &mut App, name: &str, params: &Value) -> Value {
    let handler = methods()
        .into_iter()
        .find(|(schema, _)| schema.name == name)
        .expect("method")
        .1;
    handler(app.world_mut(), params).unwrap_or_else(|e| panic!("{name}: {e:?}"))
}

/// Pixels of `image` close to `color`, and their mean x.
fn count(image: &egui::ColorImage, color: [u8; 4]) -> (usize, f64) {
    let (mut n, mut sum) = (0usize, 0f64);
    let width = image.width();
    for (i, p) in image.pixels.iter().enumerate() {
        let close = (i32::from(p.r()) - i32::from(color[0])).abs() < 12
            && (i32::from(p.g()) - i32::from(color[1])).abs() < 12
            && (i32::from(p.b()) - i32::from(color[2])).abs() < 12;
        if close {
            n += 1;
            sum += (i % width) as f64;
        }
    }
    (n, sum / n.max(1) as f64)
}

#[test]
fn painted_tiles_show_in_the_front_pane_of_the_editor_window() {
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let parent = regia_test_utils::unique_temp_dir("mode2d-view");
    regia_editor::project::set_recent_file_for_tests(parent.join("recents.txt"));
    let mut app = edit_app();
    regia_editor::project::create_and_open(app.world_mut(), "Tiles", &parent, "blank").expect("project");
    let root = app
        .world()
        .get_resource::<regia_editor::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .expect("project root");
    let maps = root.join("assets/maps");
    std::fs::create_dir_all(&maps).expect("maps dir");
    std::fs::write(maps.join("yard.tilemap.ron"), MAP).expect("map");
    let mut tiles = image::RgbaImage::from_pixel(32, 16, image::Rgba(RED));
    for y in 0..16 {
        for x in 16..32 {
            tiles.put_pixel(x, y, image::Rgba(BLUE));
        }
    }
    tiles.save(maps.join("colors.png")).expect("png");

    let state = call(
        &mut app,
        "editor.mode2d_set",
        &json!({ "active": true, "map": "assets/maps/yard.tilemap.ron", "origin": [0.0, 0.0, 0.0], "grid": false }),
    );
    assert_eq!(state["map"]["tilesets"][0]["loaded"], true, "{state}");
    // The left half red, the right half blue.
    call(&mut app, "editor.mode2d_set", &json!({ "brush": { "tile": 1 } }));
    let left: Vec<[u32; 2]> = (0..4).flat_map(|x| (0..6).map(move |y| [x, y])).collect();
    call(&mut app, "editor.tile_stroke", &json!({ "cells": left }));
    call(&mut app, "editor.mode2d_set", &json!({ "brush": { "tile": 2 } }));
    let right: Vec<[u32; 2]> = (4..8).flat_map(|x| (0..6).map(move |y| [x, y])).collect();
    call(&mut app, "editor.tile_stroke", &json!({ "cells": right }));
    app.world_mut()
        .insert_resource(regia_editor::views::ViewportLayout::default());
    app.update();

    let mut shell = EditorShell::new(Some(device.clone()), Some(root.clone()));
    let ctx = egui::Context::default();
    let image = regia_editor::window_capture::capture_editor_window(
        &mut shell,
        &ctx,
        app.world_mut(),
        &device,
        [1280, 800],
        1.0,
        5,
    )
    .expect("window capture");

    let shots = regia_core::brand::env_var("REGIA_EDITOR_SHOTS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| parent.clone());
    std::fs::create_dir_all(&shots).expect("shot dir");
    let bytes: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|p| [p.r(), p.g(), p.b(), p.a()])
        .collect();
    let png = image::RgbaImage::from_raw(1280, 800, bytes).expect("size");
    let path = shots.join("mode2d_tiles.png");
    png.save(&path).expect("png");
    eprintln!("2D mode shot: {}", path.display());

    let state = call(&mut app, "editor.mode2d_get", &json!({}));
    assert_eq!(state["view"]["tiles"], 48, "the pane drew every tile: {state}");
    assert_eq!(state["view"]["stride"], 1);
    let (red, red_x) = count(&image, RED);
    let (blue, blue_x) = count(&image, BLUE);
    assert!(red > 5000, "red tiles on screen: {red}");
    assert!(blue > 5000, "blue tiles on screen: {blue}");
    assert!(red_x < blue_x, "red left of blue: {red_x} vs {blue_x}");
    let ratio = red as f64 / blue as f64;
    assert!((0.8..1.25).contains(&ratio), "equal halves: {red} vs {blue}");
    let _ = std::fs::remove_dir_all(&parent);
}
