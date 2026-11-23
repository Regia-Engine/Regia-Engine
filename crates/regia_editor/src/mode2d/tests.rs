// SPDX-License-Identifier: MPL-2.0
use std::collections::BTreeMap;

use super::*;
use regia_2d::tilemap::{ObjectLayer, Orientation, TileLayer, TilesetDef};

const RULES: &str = r#"(
    version: 1,
    tileset: "t",
    border: Empty,
    terrains: [
        (name: "wall", layout: Edge16([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15])),
        (name: "water", layout: Edge16([16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31])),
    ],
)"#;

/// A 6×4 map (16 px tiles, tileset "t" with 32 tiles) with a tile layer and an object layer.
pub(super) fn map() -> TileMapAsset {
    TileMapAsset {
        version: 1,
        orientation: Orientation::Orthogonal,
        width: 6,
        height: 4,
        tile_width: 16,
        tile_height: 16,
        background: None,
        tilesets: vec![TilesetDef {
            name: "t".into(),
            first_gid: 1,
            image: "t.png".into(),
            image_width: 128,
            image_height: 64,
            tile_width: 16,
            tile_height: 16,
            columns: 8,
            tile_count: 32,
            spacing: 0,
            margin: 0,
            tiles: Vec::new(),
        }],
        layers: vec![
            MapLayer::Objects(ObjectLayer {
                name: "spawns".into(),
                visible: true,
                objects: Vec::new(),
                properties: BTreeMap::new(),
            }),
            MapLayer::Tiles(TileLayer {
                name: "ground".into(),
                visible: true,
                opacity: 1.0,
                offset: [0.0, 0.0],
                data: vec![0; 24],
                properties: BTreeMap::new(),
            }),
        ],
        properties: BTreeMap::new(),
    }
}

/// A temp project with `level.tilemap.ron` and `walls.autotile.ron`.
pub(super) fn project() -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir("mode2d");
    let write = |name: &str, text: &str| {
        std::fs::write(dir.join(name), text).expect("write fixture");
    };
    write("level.tilemap.ron", &map().to_ron().expect("serializes"));
    write("walls.autotile.ron", RULES);
    dir
}

pub(super) fn ground(mode: &Mode2d) -> Vec<u32> {
    mode.doc
        .as_ref()
        .and_then(|d| d.map.tile_layer(1).map(|t| t.data.clone()))
        .unwrap_or_default()
}

fn world_with_project(dir: &Path) -> World {
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.to_path_buf()),
    });
    world
}

fn code(error: &RpcError) -> String {
    error
        .data
        .as_ref()
        .and_then(|d| d["mode2d_error"].as_str())
        .unwrap_or_default()
        .to_string()
}

#[test]
fn line_cells_walk_every_cell_between_samples() {
    assert_eq!(line_cells((0, 0), (0, 0)), vec![(0, 0)]);
    assert_eq!(line_cells((0, 0), (3, 0)), vec![(0, 0), (1, 0), (2, 0), (3, 0)]);
    assert_eq!(line_cells((2, 3), (0, 1)), vec![(2, 3), (1, 2), (0, 1)]);
    let steep = line_cells((0, 0), (1, 4));
    assert_eq!(steep.len(), 5);
    assert_eq!(steep.first(), Some(&(0, 0)));
    assert_eq!(steep.last(), Some(&(1, 4)));
    // 8-connected: consecutive cells touch.
    assert!(
        steep
            .windows(2)
            .all(|w| w[0].0.abs_diff(w[1].0) <= 1 && w[0].1.abs_diff(w[1].1) <= 1)
    );
}

#[test]
fn loading_picks_the_first_tile_layer_and_refuses_other_sources() {
    let dir = project();
    let mut mode = Mode2d::default();
    mode.load_map(&dir.join("level.tilemap.ron"), false)
        .expect("loads");
    assert_eq!(mode.layer, 1, "the object layer is skipped");
    assert_eq!(
        mode.load_map(&dir.join("level.tmx"), false),
        Err(Mode2dError::NotTilemapRon(
            dir.join("level.tmx").display().to_string()
        ))
    );
    assert!(matches!(
        mode.load_map(&dir.join("gone.tilemap.ron"), false),
        Err(Mode2dError::Load(_))
    ));
    assert_eq!(
        mode.set_layer(0),
        Err(Mode2dError::BadLayer("layer 0 is not a tile layer".into()))
    );
    mode.cycle_layer(1);
    assert_eq!(mode.layer, 1, "one tile layer: cycling stays");
    std::fs::write(dir.join("bad.autotile.ron"), "(version: 1, tileset: \"nope\", terrains: [(name: \"a\", layout: Edge16([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]))])")
        .expect("write");
    assert!(
        matches!(mode.load_rules(&dir.join("bad.autotile.ron")), Err(Mode2dError::Rules(m)) if m.contains("nope"))
    );
    mode.load_rules(&dir.join("walls.autotile.ron")).expect("rules");
    mode.brush = Brush2d::Terrain("wall".into());
    mode.cycle_terrain(1);
    assert_eq!(mode.brush, Brush2d::Terrain("water".into()));
    mode.cycle_terrain(1);
    assert_eq!(mode.brush, Brush2d::Terrain("wall".into()));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_stroke_is_one_undo_entry_and_undo_redo_restore_exactly() {
    let dir = project();
    let mut world = World::new();
    let mut mode = Mode2d::default();
    mode.load_map(&dir.join("level.tilemap.ron"), false)
        .expect("loads");
    mode.load_rules(&dir.join("walls.autotile.ron")).expect("rules");
    mode.brush = Brush2d::Terrain("wall".into());
    // A drag from (0, 1) to (4, 1) with one pointer sample in between: the line fills the gap.
    mode.begin_stroke(false);
    mode.stroke_to((0, 1)).expect("paints");
    mode.stroke_to((3, 1)).expect("paints");
    mode.stroke_to((3, 1)).expect("same cell: nothing");
    mode.stroke_to((4, 1)).expect("paints");
    let command = mode.end_stroke().expect("changed");
    let painted = ground(&mode);
    // Edge16 with Empty border: caps E (2) / W (8), middle E | W (10); GID = local + 1.
    assert_eq!(&painted[6..12], &[3, 11, 11, 11, 9, 0]);
    assert_eq!(command.changes.len(), 5);
    assert!(mode.doc.as_ref().is_some_and(|d| d.dirty));
    world.insert_resource(mode);
    push_stroke(&mut world, command);
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);

    assert!(crate::command::undo_world(&mut world));
    assert!(ground(world.resource::<Mode2d>()).iter().all(|&g| g == 0));
    assert!(crate::command::redo_world(&mut world));
    assert_eq!(ground(world.resource::<Mode2d>()), painted);

    // An erase stroke (Shift) splits the wall; one more entry.
    let mut mode = world.remove_resource::<Mode2d>().expect("mode");
    mode.begin_stroke(true);
    mode.stroke_to((2, 1)).expect("erases");
    let command = mode.end_stroke().expect("changed");
    assert_eq!(&ground(&mode)[6..12], &[3, 9, 0, 3, 9, 0]);
    world.insert_resource(mode);
    push_stroke(&mut world, command);
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 2);
    assert!(crate::command::undo_world(&mut world));
    assert_eq!(ground(world.resource::<Mode2d>()), painted);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn raw_tiles_erase_without_rules_and_errors_write_nothing() {
    let dir = project();
    let mut mode = Mode2d::default();
    assert_eq!(mode.paint_now(&[(0, 0)], false), Err(Mode2dError::NoMap));
    mode.load_map(&dir.join("level.tilemap.ron"), false)
        .expect("loads");
    mode.brush = Brush2d::Tile(5 | FLIP_X_BIT);
    let changes = mode.paint_now(&[(1, 0), (1, 0), (2, 0)], false).expect("paints");
    assert_eq!(changes.len(), 2, "each cell once");
    assert_eq!(&ground(&mode)[..3], &[0, 5 | FLIP_X_BIT, 5 | FLIP_X_BIT]);
    assert_eq!(
        mode.paint_now(&[(0, 0), (6, 0)], false),
        Err(Mode2dError::OutsideMap(6, 0))
    );
    assert_eq!(ground(&mode)[0], 0, "checked before writing");
    mode.brush = Brush2d::Tile(99);
    assert!(matches!(
        mode.paint_now(&[(0, 0)], false),
        Err(Mode2dError::BadGid(_))
    ));
    mode.brush = Brush2d::Terrain("wall".into());
    assert_eq!(mode.paint_now(&[(0, 0)], false), Err(Mode2dError::NoRules));
    let erased = mode.paint_now(&[(1, 0)], true).expect("erases");
    assert_eq!(
        erased,
        vec![TileChange {
            x: 1,
            y: 0,
            before: 5 | FLIP_X_BIT,
            after: 0
        }]
    );
    // Nothing changed: no command.
    mode.begin_stroke(false);
    assert!(mode.end_stroke().is_none());
    assert_eq!(Mode2dError::OutsideMap(0, 0).code(), "mode2d_outside_map");
    assert_eq!(Mode2dError::Unsaved.code(), "mode2d_unsaved");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn save_is_atomic_round_trips_and_guards_unsaved_edits() {
    let dir = project();
    let path = dir.join("level.tilemap.ron");
    let mut mode = Mode2d::default();
    assert_eq!(save_tilemap(&mut mode, None), Err(Mode2dError::NoMap));
    mode.load_map(&path, false).expect("loads");
    mode.brush = Brush2d::Tile(7);
    mode.paint_now(&[(3, 2)], false).expect("paints");
    assert_eq!(mode.load_map(&path, false), Err(Mode2dError::Unsaved));
    let (written, bytes) = save_tilemap(&mut mode, None).expect("saves");
    assert_eq!(written, path);
    assert!(bytes > 0);
    assert!(mode.doc.as_ref().is_some_and(|d| !d.dirty));
    let text = std::fs::read_to_string(&path).expect("read back");
    let back = TileMapAsset::from_ron(&text).expect("parses");
    assert_eq!(Some(&back), mode.doc.as_ref().map(|d| &d.map));
    // No temporary file is left next to it.
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .expect("dir")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(matches!(
        save_tilemap(&mut mode, Some(&dir.join("copy.tmx"))),
        Err(Mode2dError::NotTilemapRon(_))
    ));
    let copy = dir.join("copy.tilemap.ron");
    save_tilemap(&mut mode, Some(&copy)).expect("save as");
    assert!(copy.is_file());
    mode.paint_now(&[(0, 0)], false).expect("paints");
    mode.load_map(&path, true).expect("discard reloads");
    assert_eq!(ground(&mode)[2 * 6 + 3], 7, "the saved edit is on disk");
    assert_eq!(ground(&mode)[0], 0, "the discarded edit is gone");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_level_save_writes_a_dirty_map_and_skips_a_clean_one() {
    let dir = project();
    let path = dir.join("level.tilemap.ron");
    let mut world = world_with_project(&dir);
    assert_eq!(save_dirty_tilemap(&mut world), Ok(None), "no 2D mode");
    let mut mode = Mode2d::default();
    mode.load_map(&path, false).expect("loads");
    world.insert_resource(mode);
    assert_eq!(save_dirty_tilemap(&mut world), Ok(None), "nothing edited");
    let mut mode = world.remove_resource::<Mode2d>().expect("mode");
    mode.brush = Brush2d::Tile(5);
    mode.paint_now(&[(1, 1)], false).expect("paints");
    world.insert_resource(mode);
    let (written, bytes) = save_dirty_tilemap(&mut world)
        .expect("saves")
        .expect("dirty map written");
    assert_eq!(written, path);
    assert!(bytes > 0);
    assert!(world.resource::<Mode2d>().doc.as_ref().is_some_and(|d| !d.dirty));
    let back = TileMapAsset::from_ron(&std::fs::read_to_string(&path).expect("read back")).expect("parses");
    assert_eq!(back.tile_layer(1).map(|t| t.data[6 + 1]), Some(5));
    assert_eq!(save_dirty_tilemap(&mut world), Ok(None), "saved once");

    // editor.save_level writes the dirty map next to the level and reports it.
    let mut mode = world.remove_resource::<Mode2d>().expect("mode");
    mode.paint_now(&[(2, 1)], false).expect("paints");
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let world = app.world_mut();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.clone()),
    });
    world.insert_resource(mode);
    let open = crate::level_io::new_level(world);
    world.insert_resource(open);
    let out = crate::agent_ext::test_support::save_level(world, &json!({ "path": "a.level.ron" }))
        .expect("saves the level");
    assert_eq!(out["tilemap_saved"], path.display().to_string());
    let back = TileMapAsset::from_ron(&std::fs::read_to_string(&path).expect("read back")).expect("parses");
    assert_eq!(back.tile_layer(1).map(|t| t.data[6 + 2]), Some(5));
    let again = crate::agent_ext::test_support::save_level(world, &json!({})).expect("saves again");
    assert_eq!(
        again["tilemap_saved"],
        Value::Null,
        "a clean map is not rewritten"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_front_view_maps_pixels_to_cells_and_draws_the_overlay() {
    let mut mode = Mode2d {
        active: true,
        origin: Vec3::new(100.0, 50.0, 0.0),
        ..Mode2d::default()
    };
    mode.doc = Some(TileDocument {
        path: PathBuf::from("level.tilemap.ron"),
        map: map(),
        dirty: false,
        images: Vec::new(),
    });
    let mut camera = EditorCamera::default();
    mode.frame(&mut camera);
    assert_eq!(camera.target, Vec3::new(100.0 + 48.0, 50.0 + 32.0, 0.0));
    let view = ViewCamera::of(ViewKind::Front, &camera);
    let size = Vec2::new(800.0, 600.0);
    // The centre of cell (2, 1) (row 0 = top): world x 100 + 40, y 50 + (4 − 1 − 1) · 16 + 8.
    let pixel = view.project(Vec3::new(140.0, 90.0, 0.0), size).expect("in front");
    assert_eq!(mode.cell_at_pixel(&view, pixel, size), Some((2, 1)));
    let outside = view.project(Vec3::new(90.0, 90.0, 0.0), size).expect("in front");
    assert_eq!(mode.cell_at_pixel(&view, outside, size), None);

    let colors = crate::theme::ThemeTokens::default().overlay;
    mode.hover = Some((2, 1));
    let shapes = overlay_shapes(&mode, &view, size, &colors);
    // 5 + 3 interior grid lines, 4 bound edges, 4 hover edges.
    assert_eq!(shapes.len(), 16);
    mode.grid = false;
    assert_eq!(overlay_shapes(&mode, &view, size, &colors).len(), 8);
    mode.active = false;
    assert!(overlay_shapes(&mode, &view, size, &colors).is_empty());
    assert!(mode.status().contains("layer"));
}

/// Orthogonal grids are axis lines, and only when both sides are at most 1024.
/// Other orientations outline each cell, up to 16 384 cells.
/// Spec 86 § 6: the tile grid draws only while a tile is at least 4 px on screen (and the grid
/// switch is on); the 6 × 4 fixture has 5 + 3 inner lines.
#[test]
fn the_grid_draws_only_from_four_pixels_a_tile() {
    let colors = crate::theme::ThemeTokens::default().overlay;
    let size = Vec2::new(800.0, 600.0);
    let lines = |distance: f32, grid: bool| {
        let camera = EditorCamera {
            distance,
            ..EditorCamera::default()
        };
        let view = ViewCamera::of(ViewKind::Front, &camera);
        let tile_px = 16.0 / view.units_per_pixel(size.y);
        let mode = Mode2d {
            active: true,
            grid,
            doc: Some(TileDocument {
                path: PathBuf::from("m.tilemap.ron"),
                map: map(),
                dirty: false,
                images: Vec::new(),
            }),
            ..Mode2d::default()
        };
        let count = overlay_shapes(&mode, &view, size, &colors)
            .into_iter()
            .filter(|shape| matches!(shape, OverlayShape::Line { color, .. } if *color == colors.grid_minor))
            .count();
        (tile_px, count)
    };
    // Walk the zoom out until a tile drops under 4 px; the last step at or over 4 px draws.
    let (mut above, mut below) = (None, None);
    let mut distance = 8.0f32;
    for _ in 0..400 {
        let (px, count) = lines(distance, true);
        if px >= 4.0 {
            above = Some((px, count));
        } else {
            below = Some((px, count));
            break;
        }
        distance *= 1.02;
    }
    let (px_above, drawn) = above.expect("a zoom with tiles of at least 4 px");
    let (px_below, hidden) = below.expect("a zoom with tiles under 4 px");
    assert!(
        px_above < 4.0 * 1.05,
        "the step is close to the threshold: {px_above}"
    );
    assert_eq!(drawn, 8, "{px_above} px a tile: 5 + 3 grid lines");
    assert_eq!(hidden, 0, "{px_below} px a tile: no grid");
    assert_eq!(lines(8.0, false).1, 0, "the grid switch off draws none");
}

#[test]
fn orthogonal_grid_stops_at_1024_and_other_orientations_outline_cells() {
    let colors = crate::theme::ThemeTokens::default().overlay;
    let camera = EditorCamera {
        distance: 8.0,
        ..EditorCamera::default()
    };
    let view = ViewCamera::of(ViewKind::Front, &camera);
    let size = Vec2::new(800.0, 600.0);
    let tile_px = 16.0 / view.units_per_pixel(size.y);
    assert!(
        tile_px >= 4.0,
        "the fixture tile is {tile_px} px, so the grid is allowed"
    );

    let grid_lines = |map: TileMapAsset| {
        let mode = Mode2d {
            active: true,
            grid: true,
            doc: Some(TileDocument {
                path: PathBuf::from("m.tilemap.ron"),
                map,
                dirty: false,
                images: Vec::new(),
            }),
            ..Mode2d::default()
        };
        overlay_shapes(&mode, &view, size, &colors)
            .into_iter()
            .filter(|shape| {
                matches!(
                    shape,
                    OverlayShape::Line { color, .. } if *color == colors.grid_minor
                )
            })
            .count()
    };

    let mut wide = map();
    wide.width = 1500;
    wide.height = 10;
    assert_eq!(
        grid_lines(wide),
        0,
        "an orthogonal map past 1024 on a side draws no grid lines"
    );

    let mut square = map();
    square.width = 100;
    square.height = 100;
    assert_eq!(
        grid_lines(square),
        99 + 99,
        "an orthogonal map at most 1024 on a side draws axis lines, one per interior row and column"
    );

    let outlined = grid_lines(oriented(Orientation::Isometric));
    assert!(outlined > 5, "a small isometric map outlines cells: {outlined}");
}

/// A bad `clock` is pure parameter validation: it must fail before a `discard` load drops the
/// unsaved map (it used to be checked after the load).
#[test]
fn a_bad_clock_param_changes_nothing() {
    let dir = project();
    let mut world = world_with_project(&dir);
    mode2d_set(
        &mut world,
        &json!({ "active": true, "map": "level.tilemap.ron", "brush": { "tile": 1 } }),
    )
    .expect("sets");
    tile_stroke(&mut world, &json!({ "cells": [[0, 0]] })).expect("paints");
    let dirty_before = mode2d_get(&mut world, &json!({})).expect("gets")["map"]["dirty"].clone();
    assert_eq!(dirty_before, true, "an unsaved edit to lose");
    let ground_before = ground(world.resource::<Mode2d>()).to_vec();
    let err = mode2d_set(
        &mut world,
        &json!({ "map": "level.tilemap.ron", "discard": true, "clock": { "bogus": 1 } }),
    )
    .expect_err("bad clock");
    assert_eq!(code(&err), "mode2d_bad_params");
    let after = mode2d_get(&mut world, &json!({})).expect("gets");
    assert_eq!(after["map"]["dirty"], dirty_before);
    assert_eq!(ground(world.resource::<Mode2d>()).to_vec(), ground_before);
    // A good clock still applies with the rest.
    mode2d_set(&mut world, &json!({ "clock": { "play": true } })).expect("plays");
    assert!(world.resource::<Mode2d>().clock.playing);
}

#[test]
fn agent_methods_load_paint_undo_and_save() {
    let dir = project();
    let mut world = world_with_project(&dir);
    let state = mode2d_set(
        &mut world,
        &json!({ "active": true, "map": "level.tilemap.ron", "rules": "walls.autotile.ron", "brush": { "terrain": "wall" }, "origin": [0.0, 0.0, 0.0] }),
    )
    .expect("sets");
    assert_eq!(state["active"], true);
    assert_eq!(state["layer"]["name"], "ground");
    assert_eq!(state["rules"]["terrains"], json!(["wall", "water"]));
    assert_eq!(state["map"]["size"], json!([6, 4]));
    assert!(world.resource::<Mode2d>().pending_frame);

    let out = tile_stroke(&mut world, &json!({ "cells": [[0, 3], [1, 3], [2, 3]] })).expect("strokes");
    assert_eq!(out["changed"], 3);
    assert_eq!(out["undo_depth"], 1);
    assert_eq!(&ground(world.resource::<Mode2d>())[18..21], &[3, 11, 9]);
    let get = mode2d_get(&mut world, &json!({})).expect("gets");
    assert_eq!(get["map"]["dirty"], true);
    assert_eq!(get["brush"], json!({ "terrain": "wall" }));
    assert!(crate::command::undo_world(&mut world));
    assert!(ground(world.resource::<Mode2d>()).iter().all(|&g| g == 0));
    assert!(crate::command::redo_world(&mut world));

    let saved = tilemap_save(&mut world, &json!({})).expect("saves");
    assert_eq!(saved["path"], dir.join("level.tilemap.ron").display().to_string());
    let back = TileMapAsset::from_ron(&std::fs::read_to_string(dir.join("level.tilemap.ron")).expect("read"))
        .expect("parses");
    assert_eq!(back.tile_layer(1).map(|t| t.data[19]), Some(11));

    for (params, expected) in [
        (json!({ "map": "level.tmx" }), "mode2d_not_tilemap_ron"),
        (json!({ "map": "missing.tilemap.ron" }), "mode2d_load_failed"),
        (json!({ "layer": "spawns" }), "mode2d_bad_layer"),
        (json!({ "layer": "nope" }), "mode2d_bad_layer"),
        (
            json!({ "brush": { "terrain": "lava" } }),
            "mode2d_unknown_terrain",
        ),
        (
            json!({ "brush": { "tile": 2_147_483_649u64 } }),
            "mode2d_bad_params",
        ),
        (json!({ "brush": {} }), "mode2d_bad_params"),
        (json!({ "origin": [1.0] }), "mode2d_bad_params"),
        (json!({ "rules": 3 }), "mode2d_bad_params"),
    ] {
        let err = mode2d_set(&mut world, &params).expect_err("refused");
        assert_eq!(code(&err), expected, "{params}");
    }
    for (params, expected) in [
        (json!({}), "mode2d_bad_params"),
        (json!({ "cells": [[0]] }), "mode2d_bad_params"),
        (json!({ "cells": [[9, 9]] }), "mode2d_outside_map"),
    ] {
        let err = tile_stroke(&mut world, &params).expect_err("refused");
        assert_eq!(code(&err), expected, "{params}");
    }
    // Clearing the rules turns a terrain stroke into mode2d_no_rules.
    mode2d_set(&mut world, &json!({ "rules": null })).expect("clears");
    let err = tile_stroke(&mut world, &json!({ "cells": [[0, 0]] })).expect_err("no rules");
    assert_eq!(code(&err), "mode2d_no_rules");
    let err = tilemap_save(&mut world, &json!({ "path": "out.png" })).expect_err("not ron");
    assert_eq!(code(&err), "mode2d_not_tilemap_ron");

    let mut empty = World::new();
    assert_eq!(mode2d_get(&mut empty, &json!({})).expect("gets")["active"], false);
    assert_eq!(
        code(&tilemap_save(&mut empty, &json!({})).expect_err("no map")),
        "mode2d_no_map"
    );
    assert_eq!(
        code(&mode2d_set(&mut empty, &json!({ "layer": 0 })).expect_err("no map")),
        "mode2d_no_map"
    );
    let names: Vec<&str> = methods().iter().map(|(s, _)| s.name).collect();
    assert_eq!(
        names,
        [
            "editor.mode2d_get",
            "editor.mode2d_set",
            "editor.tile_stroke",
            "editor.tile_stamp",
            "editor.tilemap_save",
            // New maps and the tileset binding (ADR-7742).
            "editor.tilemap_new",
            "editor.tilemap_tilesets",
            "editor.tileset_candidates",
            // The rule editor (ADR-7742).
            "editor.autotile_get",
            "editor.autotile_open",
            "editor.autotile_new",
            "editor.autotile_edit",
            "editor.autotile_save",
            "editor.autotile_preview",
            "editor.autotile_select",
            // The layer tree, rulers and guides (ADR-7853).
            "editor.tile_layers",
            "editor.mode2d_guides",
            // Saved patterns (ADR-7853).
            "editor.tile_pattern_list",
            "editor.tile_pattern_save",
            "editor.tile_pattern_use",
            "editor.tile_pattern_delete"
        ]
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_tilemap_entity_supplies_the_origin() {
    let dir = project();
    let mut world = world_with_project(&dir);
    // The editor propagates transforms every frame; a bare world holds the propagated value by hand.
    let transform = regia_scene::Transform::from_translation(Vec3::new(-64.0, 32.0, 2.0));
    world.spawn((
        regia_2d::TileMap::new("level.tilemap.ron"),
        transform,
        regia_scene::GlobalTransform(transform.compute_affine()),
    ));
    let state = mode2d_set(&mut world, &json!({ "map": "level.tilemap.ron" })).expect("sets");
    assert_eq!(state["origin"], json!([-64.0, 32.0, 2.0]));
    let _ = std::fs::remove_dir_all(dir);
}

// ---- Tile images (`tiles`) ---------------------------------------------------------------

use tiles::{MAX_VIEW_QUADS, corner_uvs, load_images, view_quads};

pub(super) const PANE: Vec2 = Vec2::new(800.0, 600.0);

/// A mode holding `map`, framed in a front pane.
pub(super) fn framed(map: TileMapAsset) -> (Mode2d, ViewCamera) {
    let mut mode = Mode2d {
        active: true,
        ..Mode2d::default()
    };
    mode.doc = Some(TileDocument {
        path: PathBuf::from("m.tilemap.ron"),
        map,
        dirty: false,
        images: Vec::new(),
    });
    let mut camera = EditorCamera::default();
    mode.frame(&mut camera);
    (mode, ViewCamera::of(ViewKind::Front, &camera))
}

fn set_ground(map: &mut TileMapAsset, x: u32, y: u32, raw: u32) {
    assert!(map.set_tile(1, x, y, raw), "cell in the map");
}

#[test]
fn corner_uvs_map_the_tile_rect_and_apply_flips_as_the_sprite_pass_does() {
    let map = map();
    let set = &map.tilesets[0];
    // Local 1 of an 8-column 128×64 image: x 16..32, y 0..16.
    let (l, r, t, b) = (0.125, 0.25, 0.0, 0.25);
    let plain = corner_uvs(set, 1, 0).expect("rect");
    assert_eq!(plain, [[l, t], [r, t], [r, b], [l, b]]);
    assert_eq!(
        corner_uvs(set, 1, FLIP_X_BIT).expect("rect"),
        [[r, t], [l, t], [l, b], [r, b]],
        "mirrored left to right"
    );
    assert_eq!(
        corner_uvs(set, 1, FLIP_Y_BIT).expect("rect"),
        [[l, b], [r, b], [r, t], [l, t]],
        "mirrored top to bottom"
    );
    assert_eq!(
        corner_uvs(set, 1, FLIP_DIAGONAL_BIT).expect("rect"),
        [[l, t], [l, b], [r, b], [r, t]],
        "transposed"
    );
    // Diagonal + X is a clockwise quarter turn: the source's bottom-left lands top-left.
    assert_eq!(
        corner_uvs(set, 1, FLIP_DIAGONAL_BIT | FLIP_X_BIT).expect("rect"),
        [[l, b], [l, t], [r, t], [r, b]],
    );
    assert_eq!(corner_uvs(set, 999, 0), None, "past the tile count");
}

/// Spec 86 § 8 item 10: the remaining three of the eight flip sets (Tiled's flags: X|Y is a half
/// turn, diagonal + Y a counter-clockwise quarter turn, all three the anti-transpose).
#[test]
fn corner_uvs_cover_the_other_three_flip_sets() {
    let map = map();
    let set = &map.tilesets[0];
    let (l, r, t, b) = (0.125, 0.25, 0.0, 0.25);
    assert_eq!(
        corner_uvs(set, 1, FLIP_X_BIT | FLIP_Y_BIT).expect("rect"),
        [[r, b], [l, b], [l, t], [r, t]],
        "a half turn: the source's bottom-right lands top-left"
    );
    assert_eq!(
        corner_uvs(set, 1, FLIP_DIAGONAL_BIT | FLIP_Y_BIT).expect("rect"),
        [[r, t], [r, b], [l, b], [l, t]],
        "a counter-clockwise quarter turn: the source's top-right lands top-left"
    );
    assert_eq!(
        corner_uvs(set, 1, FLIP_DIAGONAL_BIT | FLIP_X_BIT | FLIP_Y_BIT).expect("rect"),
        [[r, b], [r, t], [l, t], [l, b]],
        "the anti-transpose: the source's bottom-right lands top-left"
    );
    // All eight flag sets give eight different corner orders (the dihedral group of the square).
    let all: std::collections::BTreeSet<Vec<[u32; 2]>> = (0..8u32)
        .map(|i| {
            let raw = (if i & 1 != 0 { FLIP_X_BIT } else { 0 })
                | (if i & 2 != 0 { FLIP_Y_BIT } else { 0 })
                | (if i & 4 != 0 { FLIP_DIAGONAL_BIT } else { 0 });
            corner_uvs(set, 1, raw)
                .expect("rect")
                .iter()
                .map(|uv| uv.map(f32::to_bits))
                .collect()
        })
        .collect();
    assert_eq!(all.len(), 8);
}

#[test]
fn view_quads_land_on_the_cells_the_grid_draws() {
    let mut map = map();
    set_ground(&mut map, 0, 0, 1);
    set_ground(&mut map, 5, 3, 2);
    let (mode, view) = framed(map);
    let drawn = view_quads(&mode, &view, PANE);
    assert_eq!(drawn.stride, 1);
    assert_eq!(drawn.quads.len(), 2, "empty cells draw nothing");
    let doc = mode.doc.as_ref().expect("map");
    for (quad, (x, y), local) in [(&drawn.quads[0], (0, 0), 0), (&drawn.quads[1], (5, 3), 1)] {
        assert_eq!((quad.layer, quad.tileset, quad.local), (1, 0, local));
        let (min, max) = doc.map.tile_world_rect(x, y);
        let [min, max] = [min, max].map(|p| p.map(|v| v as f32));
        let top_left = view
            .project(Vec3::new(min[0], max[1], 0.0), PANE)
            .expect("in front");
        let bottom_right = view
            .project(Vec3::new(max[0], min[1], 0.0), PANE)
            .expect("in front");
        assert!((quad.min - top_left).length() < 0.01, "{quad:?} vs {top_left}");
        assert!(
            (quad.max - bottom_right).length() < 0.01,
            "{quad:?} vs {bottom_right}"
        );
        // A cell hovered under the quad's centre is that cell: picking and drawing agree.
        let centre = (quad.min + quad.max) * 0.5;
        assert_eq!(mode.cell_at_pixel(&view, centre, PANE), Some((x, y)));
    }
}

#[test]
fn view_quads_cull_cells_outside_the_pane() {
    let mut map = map();
    for y in 0..4 {
        for x in 0..6 {
            set_ground(&mut map, x, y, 1);
        }
    }
    let (mode, mut view) = framed(map);
    assert_eq!(view_quads(&mode, &view, PANE).quads.len(), 24);
    // Far to the right of the map: nothing is visible, margin or not.
    view.eye.x += 10_000.0;
    view.target.x += 10_000.0;
    assert!(view_quads(&mode, &view, PANE).quads.is_empty());
}

#[test]
fn view_quads_follow_visibility_opacity_offset_and_oversized_tiles() {
    let mut map = map();
    set_ground(&mut map, 2, 2, 1);
    let tile_layer = |map: &mut TileMapAsset, f: &dyn Fn(&mut regia_2d::tilemap::TileLayer)| {
        if let MapLayer::Tiles(layer) = &mut map.layers[1] {
            f(layer);
        }
    };
    tile_layer(&mut map, &|l| l.opacity = 0.25);
    let (mode, view) = framed(map.clone());
    let base = view_quads(&mode, &view, PANE).quads[0];
    assert_eq!(base.opacity, 0.25);
    let px_per_unit = (base.max.x - base.min.x) / 16.0;

    // A layer offset moves right and down in map pixels, as in the sprite pass.
    tile_layer(&mut map, &|l| l.offset = [8.0, 4.0]);
    let (mode, view) = framed(map.clone());
    let moved = view_quads(&mode, &view, PANE).quads[0];
    assert!((moved.min.x - base.min.x - 8.0 * px_per_unit).abs() < 0.01);
    assert!((moved.min.y - base.min.y - 4.0 * px_per_unit).abs() < 0.01);
    tile_layer(&mut map, &|l| l.offset = [0.0, 0.0]);

    // A tile taller than the grid keeps its bottom-left corner on the cell.
    map.tilesets[0].tile_height = 32;
    let (mode, view) = framed(map.clone());
    let tall = view_quads(&mode, &view, PANE).quads[0];
    assert!((tall.max.y - base.max.y).abs() < 0.01, "same bottom edge");
    assert!(
        (tall.max.y - tall.min.y - 32.0 * px_per_unit).abs() < 0.01,
        "twice as tall"
    );
    map.tilesets[0].tile_height = 16;

    tile_layer(&mut map, &|l| l.visible = false);
    let (mode, view) = framed(map.clone());
    assert!(view_quads(&mode, &view, PANE).quads.is_empty(), "hidden layer");
    tile_layer(&mut map, &|l| {
        l.visible = true;
        l.opacity = 0.0;
    });
    let (mode, view) = framed(map);
    assert!(
        view_quads(&mode, &view, PANE).quads.is_empty(),
        "fully transparent layer"
    );
}

#[test]
fn a_huge_view_samples_cells_and_stays_inside_the_budget() {
    let mut map = map();
    map.width = 1024;
    map.height = 1024;
    if let MapLayer::Tiles(layer) = &mut map.layers[1] {
        layer.data = vec![1; 1024 * 1024];
    }
    let (mode, view) = framed(map);
    let drawn = view_quads(&mode, &view, PANE);
    // 1 048 576 / 9 = 116 508: the first square stride under the budget.
    assert_eq!(drawn.stride, 3);
    assert!(
        drawn.quads.len() as u64 <= MAX_VIEW_QUADS,
        "{}",
        drawn.quads.len()
    );
    assert!(
        drawn.quads.len() > 100_000,
        "the whole map is on screen: {}",
        drawn.quads.len()
    );
    let quad = drawn.quads[0];
    let next = drawn.quads[1];
    let width = quad.max.x - quad.min.x;
    assert!((next.min.x - quad.max.x).abs() < 0.01, "sampled quads touch");
    assert!(width > 0.0);
}

#[test]
fn load_images_records_missing_and_broken_files_without_failing() {
    let dir = regia_test_utils::unique_temp_dir("mode2d-images");
    let map = map();
    let path = dir.join("m.tilemap.ron");
    // Missing.
    let images = load_images(&map, &path);
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].path, dir.join("t.png"));
    assert!(images[0].pixels.is_none());
    assert!(images[0].error.is_some());
    // Not an image.
    std::fs::write(dir.join("t.png"), b"not a png").expect("write");
    assert!(load_images(&map, &path)[0].error.is_some());
    // Loaded.
    image::RgbaImage::from_pixel(128, 64, image::Rgba([10, 20, 30, 255]))
        .save(dir.join("t.png"))
        .expect("png");
    let images = load_images(&map, &path);
    assert_eq!(images[0].size(), Some([128, 64]));
    assert_eq!(images[0].error, None);
}

#[test]
fn the_state_lists_tilesets_and_reload_images_picks_up_a_new_file() {
    let dir = project();
    let mut world = world_with_project(&dir);
    let state = mode2d_set(&mut world, &json!({ "map": "level.tilemap.ron" })).expect("loads");
    let set = &state["map"]["tilesets"][0];
    assert_eq!(set["name"], "t");
    assert_eq!(set["first_gid"], 1);
    assert_eq!(set["tile_count"], 32);
    assert_eq!(set["loaded"], false, "{state}");
    assert!(set["error"].is_string());
    assert_eq!(state["view"], json!({ "tiles": 0, "stride": 1 }));
    let generation = world.resource::<Mode2d>().image_generation;

    image::RgbaImage::from_pixel(128, 64, image::Rgba([10, 20, 30, 255]))
        .save(dir.join("t.png"))
        .expect("png");
    let state = mode2d_set(&mut world, &json!({ "reload_images": true })).expect("reloads");
    assert_eq!(state["map"]["tilesets"][0]["loaded"], true);
    assert_eq!(state["map"]["tilesets"][0]["error"], Value::Null);
    assert_ne!(
        world.resource::<Mode2d>().image_generation,
        generation,
        "textures follow"
    );
}

/// One egui frame whose GPU reports `max_side` as its texture limit, syncing the pane textures.
fn sync_with_limit(world: &mut World, max_side: usize) {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        max_texture_side: Some(max_side),
        ..egui::RawInput::default()
    };
    let mut output = ctx.run_ui(raw, |ui| tiles::sync_textures(ui.ctx(), world));
    // No renderer applies the uploads here; egui asserts that they were handled.
    output.textures_delta.clear();
}

/// Spec 86 § 6: an image larger than the GPU's texture limit is refused when the textures are
/// built, the refusal lands on its entry (`editor.mode2d_get` reports it), and an image exactly
/// at the limit loads.
#[test]
fn an_image_past_the_gpu_texture_limit_is_refused_and_reported() {
    let dir = project();
    // 1 025 px wide: egui's font atlas needs a limit of at least 1 024 px, so the test limits
    // sit just above that.
    image::RgbaImage::from_pixel(1025, 8, image::Rgba([10, 20, 30, 255]))
        .save(dir.join("t.png"))
        .expect("png");
    let mut world = world_with_project(&dir);
    let state = mode2d_set(&mut world, &json!({ "map": "level.tilemap.ron" })).expect("loads");
    assert_eq!(state["map"]["tilesets"][0]["loaded"], true, "{state}");

    // The 1 025 px wide image against a 1 024 px limit.
    sync_with_limit(&mut world, 1024);
    assert!(
        world.resource::<tiles::TileTextures>().get(0).is_none(),
        "no texture for a refused image"
    );
    let state = mode2d_get(&mut world, &json!({})).expect("state");
    let set = &state["map"]["tilesets"][0];
    assert_eq!(set["loaded"], false, "{state}");
    let error = set["error"].as_str().unwrap_or_default();
    assert!(error.contains("1024 px texture limit"), "{error}");

    // Reloaded, the same image exactly at a 1 025 px limit gets its texture.
    mode2d_set(&mut world, &json!({ "reload_images": true })).expect("reloads");
    sync_with_limit(&mut world, 1025);
    assert!(world.resource::<tiles::TileTextures>().get(0).is_some());
    let state = mode2d_get(&mut world, &json!({})).expect("state");
    assert_eq!(state["map"]["tilesets"][0]["loaded"], true, "{state}");
    assert_eq!(state["map"]["tilesets"][0]["error"], Value::Null);
}

#[test]
fn a_layer_change_during_a_drag_does_not_move_the_stroke() {
    let mut map = map();
    map.layers.push(MapLayer::Tiles(TileLayer {
        name: "top".into(),
        visible: true,
        opacity: 1.0,
        offset: [0.0, 0.0],
        data: vec![0; 24],
        properties: BTreeMap::new(),
    }));
    let (mut mode, _) = framed(map);
    mode.layer = 1;
    mode.begin_stroke(false);
    mode.stroke_to((0, 0)).expect("paints");
    mode.cycle_layer(1);
    assert_eq!(mode.layer, 2);
    mode.stroke_to((1, 0)).expect("paints");
    let command = mode.end_stroke().expect("a command");
    assert_eq!(command.layer, 1);
    let doc = mode.doc.as_ref().expect("map");
    assert_eq!(doc.map.tile_layer(1).expect("ground").data[..2], [1, 1]);
    assert_eq!(
        doc.map.tile_layer(2).expect("top").data[..2],
        [0, 0],
        "nothing leaked"
    );
}

#[test]
fn the_level_s_map_opens_by_itself_and_a_level_without_one_asks_for_the_tab() {
    let dir = project();
    let assets = dir.join("assets/maps");
    std::fs::create_dir_all(&assets).expect("dir");
    std::fs::copy(dir.join("level.tilemap.ron"), assets.join("yard.tilemap.ron")).expect("copy");
    let mut world = world_with_project(&dir);

    open_level_map(&mut world);
    assert!(world.resource::<Mode2d>().doc.is_none());
    assert!(world.resource::<Mode2d>().focus_panel, "nothing to open: the tab");

    let shown = |map: &str| regia_2d::TileMap {
        map: map.into(),
        ..Default::default()
    };
    world.spawn(shown("missing.tilemap.ron"));
    world.spawn(shown("level.tmx"));
    world.spawn(shown("maps/yard.tilemap.ron"));
    world.spawn(shown("level.tilemap.ron"));
    assert_eq!(
        level_maps(&mut world),
        vec![assets.join("yard.tilemap.ron"), dir.join("level.tilemap.ron")],
        "resolved in the project or its assets folder, sorted; missing and Tiled sources skipped"
    );
    world.resource_mut::<Mode2d>().focus_panel = false;
    open_level_map(&mut world);
    let mode = world.resource::<Mode2d>();
    assert_eq!(
        mode.doc.as_ref().map(|d| d.path.clone()),
        Some(assets.join("yard.tilemap.ron"))
    );
    assert!(!mode.focus_panel);
    assert!(mode.pending_frame, "the map gets framed");
}

#[test]
fn a_stamp_copies_rotates_and_pastes_as_one_undo_entry() {
    let dir = project();
    let mut world = World::new();
    let mut mode = Mode2d::default();
    mode.load_map(&dir.join("level.tilemap.ron"), false)
        .expect("loads");
    // Three tiles in a row at (0..3, 0).
    for (x, gid) in [(0, 3), (1, 4), (2, 5)] {
        mode.brush = Brush2d::Tile(gid);
        mode.paint_now(&[(x, 0)], false).expect("paints");
    }
    assert_eq!(mode.copy_stamp((2, 0), (0, 0)), Ok(()));
    assert_eq!(mode.brush, Brush2d::Stamp);
    assert_eq!(mode.stamp.as_ref().map(|s| (s.width, s.height)), Some((3, 1)));
    mode.transform_stamp(StampOp::RotateCw).expect("rotates");
    assert_eq!(mode.stamp.as_ref().map(|s| (s.width, s.height)), Some((1, 3)));
    let before = ground(&mode);
    // The stamp is centered on the painted cell: a 1x3 stamp on (4, 2) covers rows 1..=3.
    mode.begin_stroke(false);
    mode.stroke_to((4, 2)).expect("pastes");
    let command = mode.end_stroke().expect("changed");
    let after = ground(&mode);
    assert_eq!(command.changes.len(), 3);
    for (row, gid) in [(1usize, 3u32), (2, 4), (3, 5)] {
        assert_eq!(after[row * 6 + 4] & GID_MASK, gid, "row {row}");
        assert_ne!(
            after[row * 6 + 4] & !GID_MASK,
            0,
            "the flips turned with the stamp"
        );
    }
    world.insert_resource(mode);
    push_stroke(&mut world, command);
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
    assert!(crate::command::undo_world(&mut world));
    assert_eq!(ground(world.resource::<Mode2d>()), before);
    assert!(crate::command::redo_world(&mut world));
    assert_eq!(ground(world.resource::<Mode2d>()), after);

    let mut mode = world.remove_resource::<Mode2d>().expect("mode");
    mode.clear_stamp();
    assert_eq!(mode.brush, Brush2d::Tile(1));
    assert_eq!(mode.transform_stamp(StampOp::FlipX), Err(Mode2dError::NoStamp));
    mode.brush = Brush2d::Stamp;
    assert_eq!(mode.paint_now(&[(0, 0)], false), Err(Mode2dError::NoStamp));
    assert_eq!(
        mode.copy_stamp((0, 0), (9, 9)).map_err(|e| e.code()),
        Err("mode2d_bad_stamp")
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_stamp_agent_method_copies_transforms_and_reports_errors() {
    let dir = project();
    let mut world = world_with_project(&dir);
    mode2d_set(&mut world, &json!({ "active": true, "map": "level.tilemap.ron" })).expect("sets");
    tile_stroke(&mut world, &json!({ "cells": [[0, 0], [1, 0]] })).expect("paints tile 1");
    let out = tile_stamp(&mut world, &json!({ "action": "copy", "rect": [0, 0, 1, 0] })).expect("copies");
    assert_eq!(out["stamp"]["width"], 2);
    assert_eq!(out["brush"], json!({ "stamp": true }));
    let out = tile_stamp(&mut world, &json!({ "action": "transform", "op": "rotate_ccw" })).expect("rotates");
    assert_eq!(
        (out["stamp"]["width"].as_u64(), out["stamp"]["height"].as_u64()),
        (Some(1), Some(2))
    );
    let get = mode2d_get(&mut world, &json!({})).expect("gets");
    assert_eq!(get["stamp"], json!({ "width": 1, "height": 2 }));
    // Painting with the stamp brush through the stroke method pastes it.
    let out = tile_stroke(&mut world, &json!({ "cells": [[4, 2]] })).expect("pastes");
    assert!(out["changed"].as_u64().unwrap_or(0) >= 1);
    let cleared = tile_stamp(&mut world, &json!({ "action": "clear" })).expect("clears");
    assert!(cleared["stamp"].is_null());
    for (params, expected) in [
        (json!({}), "mode2d_bad_params"),
        (json!({ "action": "spin" }), "mode2d_bad_params"),
        (json!({ "action": "copy" }), "mode2d_bad_params"),
        (
            json!({ "action": "copy", "rect": [0, 0, 99, 99] }),
            "mode2d_bad_stamp",
        ),
        (
            json!({ "action": "transform", "op": "nope" }),
            "mode2d_bad_params",
        ),
        (
            json!({ "action": "transform", "op": "flip_x" }),
            "mode2d_no_stamp",
        ),
    ] {
        let error = tile_stamp(&mut world, &params).expect_err("refused");
        assert_eq!(code(&error), expected, "{params}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// A 4x3 map of 32x16 tiles in `orientation`, tile 1 at (1, 1).
fn oriented(orientation: Orientation) -> TileMapAsset {
    let mut m = map();
    m.orientation = orientation;
    m.width = 4;
    m.height = 3;
    m.tile_width = 32;
    m.tile_height = 16;
    m.tilesets[0].tile_width = 32;
    m.tilesets[0].tile_height = 16;
    m.tilesets[0].image_width = 256;
    m.tilesets[0].image_height = 64;
    if let Some(MapLayer::Tiles(t)) = m.layers.get_mut(1) {
        t.data = vec![0; 12];
    }
    m
}

fn orientations() -> Vec<Orientation> {
    use regia_2d::tilemap::{StaggerAxis, StaggerIndex};
    vec![
        Orientation::Isometric,
        Orientation::Staggered {
            axis: StaggerAxis::Y,
            index: StaggerIndex::Odd,
        },
        Orientation::Hexagonal {
            axis: StaggerAxis::Y,
            index: StaggerIndex::Even,
            side_length: 8,
        },
        Orientation::Hexagonal {
            axis: StaggerAxis::X,
            index: StaggerIndex::Odd,
            side_length: 12,
        },
    ]
}

#[test]
fn picking_and_drawing_follow_every_orientation() {
    for orientation in orientations() {
        let mut map = oriented(orientation);
        for (x, y) in [(0, 0), (1, 1), (3, 2), (2, 0)] {
            set_ground(&mut map, x, y, 3);
        }
        let (mut mode, view) = framed(map);
        for y in 0..3 {
            for x in 0..4 {
                let doc = mode.doc.as_ref().expect("map");
                let [cx, cy] = doc.map.tile_world_center(x, y);
                let pixel = view
                    .project(Vec3::new(cx + mode.origin.x, cy + mode.origin.y, 0.0), PANE)
                    .expect("in front");
                assert_eq!(
                    mode.cell_at_pixel(&view, pixel, PANE),
                    Some((x, y)),
                    "{orientation:?}"
                );
            }
        }
        // Four painted cells draw four quads; each quad is at the cell box's bottom-left.
        let drawn = view_quads(&mode, &view, PANE);
        assert_eq!(drawn.quads.len(), 4, "{orientation:?}");
        let doc = mode.doc.as_ref().expect("map");
        for quad in &drawn.quads {
            let centre = (quad.min + quad.max) * 0.5;
            let picked = mode.cell_at_pixel(&view, centre, PANE);
            let cell = picked.expect("quad centre is over a cell or its overhang");
            let (min, _) = doc.map.tile_world_rect(cell.0, cell.1);
            let min = min.map(|v| v as f32);
            let corner = view
                .project(Vec3::new(min[0], min[1], 0.0), PANE)
                .expect("in front");
            assert!(
                (corner.x - quad.min.x).abs() < 0.01,
                "{orientation:?} quad {quad:?}"
            );
        }
        // The overlay outlines the hover cell with its own shape (4 or 6 edges) plus the bounds.
        let colors = crate::theme::ThemeTokens::default().overlay;
        mode.grid = false;
        mode.hover = Some((1, 1));
        let edges = doc_edges(&mode);
        assert_eq!(
            overlay_shapes(&mode, &view, PANE, &colors).len(),
            4 + edges,
            "{orientation:?}"
        );
    }
}

fn doc_edges(mode: &Mode2d) -> usize {
    mode.doc
        .as_ref()
        .map_or(0, |d| d.map.tile_world_outline(1, 1).len())
}

#[test]
fn a_non_orthogonal_grid_is_outlined_cell_by_cell() {
    let (mut mode, view) = framed(oriented(Orientation::Isometric));
    let colors = crate::theme::ThemeTokens::default().overlay;
    mode.hover = None;
    mode.grid = true;
    let with_grid = overlay_shapes(&mode, &view, PANE, &colors).len();
    // 12 diamonds of 4 edges each plus the 4 bound edges.
    assert_eq!(with_grid, 12 * 4 + 4);
    mode.grid = false;
    assert_eq!(overlay_shapes(&mode, &view, PANE, &colors).len(), 4);
}

#[test]
fn the_state_reports_the_orientation_and_a_stamp_footprint_is_outlined() {
    let dir = project();
    let mut world = world_with_project(&dir);
    let state = mode2d_set(&mut world, &json!({ "active": true, "map": "level.tilemap.ron" })).expect("sets");
    assert_eq!(state["map"]["orientation"], "orthogonal");
    tile_stamp(&mut world, &json!({ "action": "copy", "rect": [0, 0, 1, 1] })).expect("copies");
    let mut mode = world.remove_resource::<Mode2d>().expect("mode");
    let mut camera = EditorCamera::default();
    mode.frame(&mut camera);
    let view = ViewCamera::of(ViewKind::Front, &camera);
    let colors = crate::theme::ThemeTokens::default().overlay;
    mode.grid = false;
    mode.hover = Some((3, 2));
    let plain = {
        mode.brush = Brush2d::Tile(1);
        overlay_shapes(&mode, &view, PANE, &colors).len()
    };
    mode.brush = Brush2d::Stamp;
    // A 2x2 stamp adds the outline of its four cells (4 edges each) to the hover cell's.
    assert_eq!(overlay_shapes(&mode, &view, PANE, &colors).len(), plain + 16);
    let _ = std::fs::remove_dir_all(dir);
}

/// A mode with the fixture map; tile 0 of the tileset (gid 1) animates through local tiles 0, 5
/// and 9 (100, 150 and 250 ms), and every ground cell shows it.
fn animated_mode() -> Mode2d {
    let mut m = map();
    m.tilesets[0].tiles = vec![regia_2d::tilemap::TileDef {
        id: 0,
        animation: [(0, 100), (5, 150), (9, 250)]
            .map(|(tile, duration_ms)| regia_2d::tilemap::TileFrame { tile, duration_ms })
            .to_vec(),
        ..regia_2d::tilemap::TileDef::default()
    }];
    for y in 0..4 {
        for x in 0..6 {
            set_ground(&mut m, x, y, 1);
        }
    }
    let mut mode = Mode2d {
        active: true,
        ..Mode2d::default()
    };
    mode.doc = Some(TileDocument {
        path: PathBuf::from("level.tilemap.ron"),
        map: m,
        dirty: false,
        images: Vec::new(),
    });
    mode
}

fn pane_frames(mode: &Mode2d) -> Vec<u32> {
    let mut camera = EditorCamera::default();
    let framing = Mode2d {
        doc: mode.doc.clone(),
        ..Mode2d::default()
    };
    framing.frame(&mut camera);
    let view = ViewCamera::of(ViewKind::Front, &camera);
    tiles::view_quads(mode, &view, Vec2::new(800.0, 600.0))
        .quads
        .iter()
        .filter(|q| q.layer == 1)
        .map(|q| q.local)
        .collect()
}

#[test]
fn after_n_fixed_steps_the_pane_tile_frame_equals_the_runtime_frame() {
    let mut mode = animated_mode();
    mode.clock.playing = true;
    let set = mode.doc.as_ref().expect("doc").map.tilesets[0].clone();
    // The runtime reads `Time::elapsed().as_millis()` (regia_2d plugin `extract_frame`).
    let mut time = regia_app::Time::default();
    let dt = std::time::Duration::from_micros(16_667);
    let mut seen = BTreeSet::new();
    for step in 0..120 {
        let expected = regia_2d::batch::animated_tile(&set, 0, time.elapsed().as_millis() as u64);
        let frames = pane_frames(&mode);
        assert_eq!(frames.len(), 24, "step {step}: every cell is drawn");
        assert!(
            frames.iter().all(|&f| f == expected),
            "step {step}: pane {frames:?} vs runtime {expected}"
        );
        seen.insert(expected);
        time.advance(dt);
        assert!(mode.clock.tick(dt));
    }
    assert_eq!(
        seen,
        BTreeSet::from([0, 5, 9]),
        "the run covers every frame of the animation"
    );
}

#[test]
fn a_paused_pane_clock_does_not_move_and_steps_are_exact() {
    let mut mode = animated_mode();
    assert!(!mode.clock.tick(std::time::Duration::from_millis(500)), "paused");
    assert_eq!(mode.clock.time_ms(), 0);
    assert_eq!(pane_frames(&mode)[0], 0, "the first frame at time zero");
    mode.clock.step(std::time::Duration::from_millis(100));
    assert_eq!(pane_frames(&mode)[0], 5);
    mode.clock.step(std::time::Duration::from_millis(150));
    assert_eq!(pane_frames(&mode)[0], 9);
    mode.clock.step(std::time::Duration::from_millis(250));
    assert_eq!(pane_frames(&mode)[0], 0, "loops");
    mode.clock.reset();
    assert_eq!(mode.clock.time_ms(), 0);
}

#[test]
fn the_clock_parameter_plays_steps_and_resets() {
    let mut mode = animated_mode();
    apply_clock(&mut mode.clock, &json!({ "step_ms": 120, "play": true })).expect("applies");
    assert!(mode.clock.playing);
    assert_eq!(mode.clock.time_ms(), 120);
    apply_clock(&mut mode.clock, &json!({ "reset": true, "play": false })).expect("applies");
    assert_eq!((mode.clock.playing, mode.clock.time_ms()), (false, 0));
    for bad in [
        json!(3),
        json!({ "step_ms": -1 }),
        json!({ "step_ms": 9_999_999_999u64 }),
        json!({ "play": 1 }),
        json!({ "speed": 2 }),
    ] {
        assert!(
            matches!(apply_clock(&mut mode.clock, &bad), Err(Mode2dError::BadParams(_))),
            "{bad}"
        );
    }
}

/// Spec 86 § 13: `clock` applies reset, then step, then play, so `{ reset, step_ms }` lands on
/// `step_ms` (a step before the reset would land on 0), and a step moves a paused clock.
#[test]
fn the_clock_parameter_resets_before_it_steps() {
    let mut mode = animated_mode();
    apply_clock(&mut mode.clock, &json!({ "step_ms": 500 })).expect("applies");
    assert_eq!(mode.clock.time_ms(), 500);
    assert!(!mode.clock.playing, "a step does not start the clock");
    apply_clock(&mut mode.clock, &json!({ "reset": true, "step_ms": 40 })).expect("applies");
    assert_eq!(mode.clock.time_ms(), 40, "reset first, then the step");
    apply_clock(
        &mut mode.clock,
        &json!({ "play": true, "reset": true, "step_ms": 7 }),
    )
    .expect("applies");
    assert_eq!((mode.clock.playing, mode.clock.time_ms()), (true, 7));
    // A playing clock steps by exactly `step_ms` too; `tick` is the pane's frame time.
    apply_clock(&mut mode.clock, &json!({ "step_ms": 3 })).expect("applies");
    assert_eq!(mode.clock.time_ms(), 10);
}

/// Spec 86 § 5 / § 13 through the agent handlers: `editor.mode2d_set { clock }` answers the new
/// `clock`, `editor.mode2d_get` reports it; a brush gid no tileset holds is `mode2d_bad_gid` on
/// the stroke; a save into a missing directory is `mode2d_save_failed` and keeps the edits.
#[test]
fn agent_clock_bad_gid_and_save_failed() {
    let dir = project();
    let mut world = world_with_project(&dir);
    let state = mode2d_set(
        &mut world,
        &json!({ "active": true, "map": "level.tilemap.ron", "clock": { "step_ms": 250, "play": true } }),
    )
    .expect("sets");
    assert_eq!(state["clock"], json!({ "playing": true, "time_ms": 250 }));
    let state = mode2d_set(
        &mut world,
        &json!({ "clock": { "reset": true, "step_ms": 5, "play": false } }),
    )
    .expect("sets");
    assert_eq!(
        state["clock"],
        json!({ "playing": false, "time_ms": 5 }),
        "reset, then step, then play"
    );
    let get = mode2d_get(&mut world, &json!({})).expect("gets");
    assert_eq!(get["clock"], json!({ "playing": false, "time_ms": 5 }));
    assert_eq!(
        code(&mode2d_set(&mut world, &json!({ "clock": { "step_ms": -1 } })).expect_err("bad clock")),
        "mode2d_bad_params"
    );

    // Tileset "t" holds gids 1..=32: 99 is accepted as a brush and refused when it paints.
    mode2d_set(&mut world, &json!({ "brush": { "tile": 99 } })).expect("sets");
    let err = tile_stroke(&mut world, &json!({ "cells": [[0, 0]] })).expect_err("bad gid");
    assert_eq!(code(&err), "mode2d_bad_gid");
    assert!(
        ground(world.resource::<Mode2d>()).iter().all(|&g| g == 0),
        "nothing written"
    );

    mode2d_set(&mut world, &json!({ "brush": { "tile": 3 } })).expect("sets");
    tile_stroke(&mut world, &json!({ "cells": [[1, 1]] })).expect("strokes");
    let err = tilemap_save(&mut world, &json!({ "path": "no/such/dir/out.tilemap.ron" }))
        .expect_err("missing directory");
    assert_eq!(code(&err), "mode2d_save_failed");
    let doc = world.resource::<Mode2d>().doc.as_ref().expect("map");
    assert!(doc.dirty, "a failed save keeps the edits unsaved");
    let _ = std::fs::remove_dir_all(dir);
}

/// Audit G finding 8: `editor.tilemap_save` to a differently-spelled path that denotes the same
/// file (here via a `sub/../` spelling; on Windows also case and `\` spellings) must clear
/// `dirty`, or the status line keeps the `*` marker and `mode2d_set` refuses with
/// `mode2d_unsaved` forever.
#[test]
fn save_tilemap_with_respelled_same_file_clears_dirty() {
    let dir = project();
    std::fs::create_dir_all(dir.join("sub")).expect("dir");
    let path = dir.join("level.tilemap.ron");
    let mut mode = Mode2d::default();
    mode.load_map(&path, false).expect("load");
    let respelled = dir.join("sub/../level.tilemap.ron");
    assert_ne!(respelled, path, "the test needs a differently-spelled same file");
    mode.doc.as_mut().expect("doc").dirty = true;
    save_tilemap(&mut mode, Some(&respelled)).expect("save");
    assert!(
        !mode.doc.as_ref().expect("doc").dirty,
        "saving the same file under another spelling clears dirty"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Audit G finding 4: `map_origin` matched the shown path with a plain string `ends_with`, so
/// `.../oldmaps/x.tilemap.ron` falsely matched an entity showing `maps/x.tilemap.ron` on every
/// OS, and a case-variant spelling matched nowhere on Windows. The match must sit on path
/// components; the case-fold half is the Windows behaviour (proven in `path_norm`'s tests).
#[test]
fn map_origin_requires_a_segment_boundary() {
    let mut world = World::new();
    world.spawn((
        regia_scene::prelude::Transform::from_translation(regia_core::math::Vec3::new(3.0, 0.0, 4.0)),
        // `Transform` requires a `GlobalTransform`, and `world_matrix` reads that first.
        regia_scene::prelude::GlobalTransform(regia_core::math::Affine3A::from_translation(
            regia_core::math::Vec3::new(3.0, 0.0, 4.0),
        )),
        regia_2d::TileMap {
            map: "maps/x.tilemap.ron".into(),
            ..Default::default()
        },
    ));
    let inside = map_origin(&mut world, Path::new("/p/assets/maps/x.tilemap.ron"));
    assert_eq!(
        inside,
        Some(regia_core::math::Vec3::new(3.0, 0.0, 4.0)),
        "a real suffix matches"
    );
    let no_boundary = map_origin(&mut world, Path::new("/p/assets/oldmaps/x.tilemap.ron"));
    assert_eq!(no_boundary, None, "oldmaps/ is not maps/ at a component boundary");
    let other = map_origin(&mut world, Path::new("/p/assets/maps/y.tilemap.ron"));
    assert_eq!(other, None);
}

/// rc1 editor-undo finding 4 (ADR-7412): the windowed Save level marked the undo stack saved
/// before writing the tile map, so a failed tile-map write left the strokes on a clean stack and
/// the close prompt never asked. The stack must stay dirty until the map is on disk.
#[test]
fn a_failed_tile_map_write_keeps_the_level_dirty() {
    let dir = project();
    let path = dir.join("level.tilemap.ron");
    let mut world = world_with_project(&dir);
    let mut mode = Mode2d::default();
    mode.load_map(&path, false).expect("loads");
    mode.brush = Brush2d::Tile(5);
    mode.paint_now(&[(1, 1)], false).expect("paints");
    // The map's folder is gone: the write fails.
    if let Some(doc) = mode.doc.as_mut() {
        doc.path = dir.join("gone").join("level.tilemap.ron");
    }
    world.insert_resource(mode);
    let mut stack = crate::command::UndoStack::new();
    stack.mark_dirty();
    world.insert_resource(stack);

    let note = save_dirty_tilemap_and_mark_saved(&mut world);
    assert!(note.contains("tile map not saved"), "{note}");
    assert!(
        world.resource::<crate::command::UndoStack>().is_dirty(),
        "the strokes are not on disk: the level stays dirty"
    );

    // Once the map can be written, the same call writes it and marks the level saved.
    if let Some(doc) = world.resource_mut::<Mode2d>().doc.as_mut() {
        doc.path = path.clone();
    }
    let note = save_dirty_tilemap_and_mark_saved(&mut world);
    assert!(note.contains("level.tilemap.ron"), "{note}");
    assert!(!world.resource::<crate::command::UndoStack>().is_dirty());
    let _ = std::fs::remove_dir_all(dir);
}
