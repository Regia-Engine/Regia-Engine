// SPDX-License-Identifier: MPL-2.0
//! Tests of the paint tools, the random brush and the terrain path mode (ADR-7853): the mode's
//! own API, `editor.tile_stroke` / `editor.mode2d_set`, the front pane's pointer path and the
//! 2D tab's keyboard path.
#![cfg(test)]

use super::tests::{PANE, framed, ground, map, project};
use super::*;

/// A mode holding the test map with the active layer on the tile layer.
fn mode() -> Mode2d {
    let (mut mode, _) = framed(map());
    mode.layer = 1;
    mode
}

/// One stroke from `from` to `to` (two pointer samples) with the current tool; the command.
fn stroke(mode: &mut Mode2d, cells: &[(u32, u32)], erase: bool) -> Option<TilePaintCommand> {
    mode.begin_stroke(erase);
    for &cell in cells {
        mode.stroke_to(cell).expect("samples");
    }
    mode.end_stroke()
}

fn row(mode: &Mode2d, y: usize) -> Vec<u32> {
    ground(mode)[y * 6..(y + 1) * 6].to_vec()
}

#[test]
fn a_line_lands_on_release_as_one_entry_and_previews_while_dragging() {
    let mut mode = mode();
    mode.tool = Tool2d::Line;
    mode.brush = Brush2d::Tile(4);
    mode.begin_stroke(false);
    mode.stroke_to((0, 0)).expect("press");
    mode.stroke_to((2, 1)).expect("drag");
    assert!(
        ground(&mode).iter().all(|&g| g == 0),
        "nothing lands before the release"
    );
    assert_eq!(mode.shape_preview(), line_cells((0, 0), (2, 1)));
    mode.stroke_to((5, 3)).expect("drag");
    let command = mode.end_stroke().expect("changed");
    let expected = line_cells((0, 0), (5, 3));
    assert_eq!(command.changes.len(), expected.len());
    for (x, y) in expected {
        assert_eq!(ground(&mode)[y as usize * 6 + x as usize], 4, "({x}, {y})");
    }
    assert!(mode.shape_preview().is_empty());
}

#[test]
fn a_rectangle_fills_or_outlines_and_undoes_as_one_entry() {
    let mut world = World::new();
    let mut m = mode();
    m.tool = Tool2d::Rect;
    m.brush = Brush2d::Tile(2);
    let command = stroke(&mut m, &[(1, 0), (4, 3)], false).expect("changed");
    assert_eq!(command.changes.len(), 16);
    world.insert_resource(m);
    push_stroke(&mut world, command);
    assert!(crate::command::undo_world(&mut world));
    assert!(ground(world.resource::<Mode2d>()).iter().all(|&g| g == 0));
    let mut m = world.remove_resource::<Mode2d>().expect("mode");
    m.rect_filled = false;
    let command = stroke(&mut m, &[(4, 3), (1, 0)], false).expect("changed");
    assert_eq!(command.changes.len(), 12, "a 4x4 border");
    assert_eq!(row(&m, 1), [0, 2, 0, 0, 2, 0]);
    assert_eq!(row(&m, 0), [0, 2, 2, 2, 2, 0]);
    // A rectangle erase (Shift) clears its cells.
    m.rect_filled = true;
    stroke(&mut m, &[(1, 0), (2, 3)], true).expect("erases");
    assert_eq!(row(&m, 0), [0, 0, 0, 2, 2, 0]);
}

#[test]
fn the_bucket_fills_the_region_or_every_matching_cell() {
    let mut m = mode();
    // A wall of tile 9 down column 2 splits the map.
    m.brush = Brush2d::Tile(9);
    stroke(&mut m, &[(2, 0), (2, 3)], false).expect("wall");
    m.tool = Tool2d::Bucket;
    m.brush = Brush2d::Tile(5);
    let command = stroke(&mut m, &[(0, 0), (1, 3), (5, 3)], false).expect("fills");
    assert_eq!(command.changes.len(), 8, "the 2x4 region left of the wall");
    assert_eq!(
        row(&m, 3),
        [5, 5, 9, 0, 0, 0],
        "later samples of the drag add nothing"
    );
    m.bucket_contiguous = false;
    m.brush = Brush2d::Tile(6);
    let command = stroke(&mut m, &[(5, 0)], false).expect("fills");
    assert_eq!(command.changes.len(), 12, "every empty cell, both sides");
    assert_eq!(row(&m, 0), [5, 5, 9, 6, 6, 6]);
    // A bucket with the stamp tiles the stamp, aligned to the map.
    m.stamp = Some(regia_2d::stamp::Stamp::new(2, 1, vec![7, 8]).expect("stamp"));
    m.brush = Brush2d::Stamp;
    m.bucket_contiguous = true;
    stroke(&mut m, &[(3, 1)], false).expect("pattern");
    assert_eq!(row(&m, 1), [5, 5, 9, 8, 7, 8]);
}

#[test]
fn pick_takes_the_cell_s_tile_and_an_empty_cell_picks_erase() {
    let mut m = mode();
    m.brush = Brush2d::Tile(3 | FLIP_X_BIT);
    m.paint_now(&[(1, 1)], false).expect("paints");
    m.brush = Brush2d::Erase;
    m.random = Some(RandomBrush {
        tiles: vec![RandomTile { raw: 2, weight: 1 }],
        nothing: 0.0,
        seed: 0,
    });
    m.pick_cell((1, 1)).expect("picks");
    assert_eq!(m.brush, Brush2d::Tile(3 | FLIP_X_BIT), "flip bits come along");
    assert!(m.random.is_none(), "a pick ends the random brush");
    m.pick_cell((0, 0)).expect("picks");
    assert_eq!(m.brush, Brush2d::Erase);
    assert_eq!(m.pick_cell((9, 0)), Err(Mode2dError::OutsideMap(9, 0)));
}

#[test]
fn the_random_brush_scatters_in_every_tool_and_repeats_for_a_seed() {
    let random = RandomBrush {
        tiles: vec![RandomTile { raw: 5, weight: 2 }, RandomTile { raw: 6, weight: 1 }],
        nothing: 0.3,
        seed: 11,
    };
    for tool in [Tool2d::Paint, Tool2d::Rect, Tool2d::Bucket] {
        let mut m = mode();
        m.tool = tool;
        m.brush = Brush2d::Tile(1);
        m.random = Some(random.clone());
        let cells: &[(u32, u32)] = match tool {
            Tool2d::Paint => &[(0, 0), (5, 0)],
            Tool2d::Rect => &[(0, 0), (5, 3)],
            _ => &[(0, 0)],
        };
        stroke(&mut m, cells, false);
        let rows = if tool == Tool2d::Paint { 1 } else { 4 };
        for y in 0..rows {
            for x in 0..6u32 {
                let expected = random.pick(x, y).unwrap_or(0);
                assert_eq!(
                    ground(&m)[y as usize * 6 + x as usize],
                    expected,
                    "{tool:?} ({x}, {y})"
                );
            }
        }
        if tool != Tool2d::Paint {
            assert!(
                ground(&m).contains(&0) && ground(&m).contains(&5),
                "{tool:?} left gaps and painted"
            );
        }
        assert_eq!(
            m.random.as_ref().map(|r| r.seed),
            Some(12),
            "the next stroke scatters anew"
        );
    }
    // A gid no tileset holds is refused before anything is written.
    let mut m = mode();
    m.random = Some(RandomBrush {
        tiles: vec![RandomTile { raw: 999, weight: 1 }],
        ..random
    });
    assert!(matches!(
        m.paint_now(&[(0, 0)], false),
        Err(Mode2dError::BadGid(_))
    ));
    assert!(ground(&m).iter().all(|&g| g == 0));
}

#[test]
fn path_mode_joins_only_the_stroke_s_cells() {
    let dir = project();
    let paint = |autotile: AutotileMode| {
        let mut m = Mode2d::default();
        m.load_map(&dir.join("level.tilemap.ron"), false).expect("loads");
        m.load_rules(&dir.join("walls.autotile.ron")).expect("rules");
        m.brush = Brush2d::Terrain("wall".into());
        stroke(&mut m, &[(0, 1), (2, 1)], false).expect("row 1 wall");
        assert_eq!(row(&m, 1), [3, 11, 9, 0, 0, 0]);
        m.autotile_mode = autotile;
        stroke(&mut m, &[(0, 2), (1, 2)], false).expect("row 2");
        m
    };
    // Connect: row 2 joins row 1 above it (N bit 1), and row 1 gains its S bit (4).
    let connect = paint(AutotileMode::Connect);
    assert_eq!(row(&connect, 2), [4, 10, 0, 0, 0, 0]);
    assert_eq!(row(&connect, 1), [7, 15, 9, 0, 0, 0]);
    // Path: row 2 joins only itself; row 1 keeps its tiles.
    let mut path = paint(AutotileMode::Path);
    assert_eq!(row(&path, 2), [3, 9, 0, 0, 0, 0]);
    assert_eq!(row(&path, 1), [3, 11, 9, 0, 0, 0]);
    // A new path stroke does not join the last one; within a stroke, later samples join earlier.
    stroke(&mut path, &[(2, 2)], false).expect("alone");
    assert_eq!(row(&path, 2), [3, 9, 1, 0, 0, 0]);
    path.begin_stroke(false);
    path.stroke_to((0, 3)).expect("first sample");
    assert_eq!(row(&path, 3)[0], 1, "alone so far");
    path.stroke_to((2, 3)).expect("second sample");
    path.end_stroke().expect("changed");
    assert_eq!(row(&path, 3), [3, 11, 9, 0, 0, 0]);
    // A path rectangle joins its own cells.
    path.tool = Tool2d::Rect;
    path.rect_filled = false;
    stroke(&mut path, &[(3, 0), (5, 1)], false).expect("box");
    // Edge16 bits N 1, E 2, S 4, W 8; gid = mask + 1. (2, 1) of the first wall is not joined.
    assert_eq!(&row(&path, 0)[3..], &[7, 15, 13]);
    assert_eq!(row(&path, 1), [3, 11, 9, 4, 12, 10]);
    let _ = std::fs::remove_dir_all(dir);
}

fn code(error: &RpcError) -> String {
    error
        .data
        .as_ref()
        .and_then(|d| d["mode2d_error"].as_str())
        .unwrap_or_default()
        .to_string()
}

fn world_with(mode: Mode2d) -> World {
    let mut world = World::new();
    world.insert_resource(mode);
    world
}

#[test]
fn tile_stroke_takes_a_tool_a_random_brush_and_a_mode_for_one_call() {
    let mut world = world_with(mode());
    world.resource_mut::<Mode2d>().brush = Brush2d::Tile(2);
    let out = tile_stroke(&mut world, &json!({ "tool": "rect", "cells": [[0, 0], [2, 1]] })).expect("rect");
    assert_eq!(out["changed"], 6);
    assert_eq!(out["tool"], "rect");
    let out = tile_stroke(
        &mut world,
        &json!({ "tool": "rect", "filled": false, "cells": [[3, 0], [5, 3]], "erase": false }),
    )
    .expect("outline");
    assert_eq!(out["changed"], 10);
    assert_eq!(
        world.resource::<Mode2d>().tool,
        Tool2d::Paint,
        "the call's tool was for that call"
    );
    let out = tile_stroke(&mut world, &json!({ "tool": "line", "cells": [[0, 3], [2, 3]] })).expect("line");
    assert_eq!(out["changed"], 3);
    let out = tile_stroke(
        &mut world,
        &json!({ "tool": "bucket", "cells": [[4, 1]], "random": { "tiles": [{ "tile": 7 }, { "tile": 8, "weight": 3 }], "nothing": 0.0, "seed": 5 } }),
    )
    .expect("bucket");
    assert_eq!(out["changed"], 2, "the 1x2 hole inside the outline");
    assert!(
        world.resource::<Mode2d>().random.is_none(),
        "the call's random brush was for that call"
    );
    let state = mode2d_get(&mut world, &json!({})).expect("gets");
    assert_eq!(state["tool"], "paint");
    assert_eq!(state["random"], Value::Null);
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        4,
        "one entry per call"
    );
    let out = tile_stroke(&mut world, &json!({ "tool": "pick", "cells": [[0, 0]] })).expect("pick");
    assert_eq!(out["changed"], 0);
    assert_eq!(out["brush"]["tile"], 2);
    let out = tile_stroke(&mut world, &json!({ "tool": "pick", "cells": [[0, 0], [1, 1]] })).expect("pick");
    assert_eq!(out["brush"], json!({ "stamp": true }));
    // A call's own random brush paints random tiles whatever the brush is (here the stamp).
    let out = tile_stroke(
        &mut world,
        &json!({ "cells": [[5, 0]], "random": { "tiles": [{ "tile": 9 }] } }),
    )
    .expect("random over the stamp brush");
    assert_eq!(out["changed"], 1);
    assert_eq!(out["brush"], json!({ "stamp": true }), "the brush comes back");
    assert_eq!(ground(world.resource::<Mode2d>())[5], 9);
    for (params, expected) in [
        (json!({ "tool": "lasso", "cells": [[0, 0]] }), "mode2d_bad_params"),
        (json!({ "tool": "line", "cells": [[0, 0]] }), "mode2d_bad_params"),
        (
            json!({ "tool": "bucket", "cells": [[0, 0], [1, 1]] }),
            "mode2d_bad_params",
        ),
        (json!({ "tool": "pick", "cells": [] }), "mode2d_bad_params"),
        (
            json!({ "mode": "zigzag", "cells": [[0, 0]] }),
            "mode2d_bad_params",
        ),
        (json!({ "filled": 1, "cells": [[0, 0]] }), "mode2d_bad_params"),
        (
            json!({ "random": { "tiles": [] }, "cells": [[0, 0]] }),
            "mode2d_bad_params",
        ),
        (
            json!({ "random": { "tiles": [{ "tile": 1 }], "nothing": 2.0 }, "cells": [[0, 0]] }),
            "mode2d_bad_params",
        ),
        (
            json!({ "random": { "tiles": [{ "tile": 999 }] }, "cells": [[0, 0]] }),
            "mode2d_bad_gid",
        ),
        (
            json!({ "tool": "rect", "cells": [[0, 0], [9, 0]] }),
            "mode2d_outside_map",
        ),
    ] {
        let err = tile_stroke(&mut world, &params).expect_err("refused");
        assert_eq!(code(&err), expected, "{params}");
    }
}

#[test]
fn mode2d_set_holds_the_tool_state_and_refuses_bad_values_before_changing_anything() {
    let mut world = world_with(mode());
    let state = mode2d_set(
        &mut world,
        &json!({ "tool": "bucket", "bucket_contiguous": false, "rect_filled": false, "autotile_mode": "path",
                 "random": { "tiles": [{ "tile": 3, "flip": { "x": true } }], "nothing": 0.5, "seed": 9 } }),
    )
    .expect("sets");
    assert_eq!(state["tool"], "bucket");
    assert_eq!(state["bucket_contiguous"], false);
    assert_eq!(state["rect_filled"], false);
    assert_eq!(state["autotile_mode"], "path");
    assert_eq!(state["random"]["tiles"][0]["tile"], 3);
    assert_eq!(state["random"]["tiles"][0]["raw"], 3 | FLIP_X_BIT);
    assert_eq!(state["random"]["nothing"], 0.5);
    for params in [
        json!({ "tool": "line", "random": { "tiles": "x" } }),
        json!({ "tool": "line", "autotile_mode": 1 }),
        json!({ "tool": "nope" }),
        json!({ "tool": "line", "bucket_contiguous": "yes" }),
    ] {
        let err = mode2d_set(&mut world, &params).expect_err("refused");
        assert_eq!(code(&err), "mode2d_bad_params", "{params}");
        assert_eq!(
            world.resource::<Mode2d>().tool,
            Tool2d::Bucket,
            "{params} changed nothing"
        );
    }
    let state = mode2d_set(&mut world, &json!({ "random": null, "tool": "paint" })).expect("clears");
    assert_eq!(state["random"], Value::Null);
}

// ---- Pointer and keyboard paths -------------------------------------------------------------

/// The front pane, as `input_tests` drives it.
struct Pane {
    ctx: egui::Context,
    world: World,
    view: ViewCamera,
    layout: ViewportLayout,
}

impl Pane {
    fn new() -> Self {
        let (mut mode, view) = framed(map());
        mode.layer = 1;
        Self {
            ctx: egui::Context::default(),
            world: world_with(mode),
            view,
            layout: ViewportLayout::default(),
        }
    }

    fn frame(&mut self, events: Vec<egui::Event>) {
        let Self {
            ctx,
            world,
            view,
            layout,
        } = self;
        let front = layout
            .panes
            .iter()
            .position(|p| p.kind == ViewKind::Front)
            .expect("front pane");
        let mut camera = layout.panes[front].camera.clone();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(PANE.x + 200.0, PANE.y + 200.0),
            )),
            events,
            ..egui::RawInput::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(PANE.x, PANE.y));
            let response = ui.interact(rect, egui::Id::new("pane"), egui::Sense::click_and_drag());
            if is_active(world) {
                paint_overlay(ui, world, view, rect);
                viewport_input(ui, world, &response, view, PANE);
            }
            viewport_frame(ui, world, layout, front, &mut camera, true, response.has_focus());
        });
        output.textures_delta.clear();
    }

    fn at(&self, cell: (u32, u32)) -> egui::Pos2 {
        let mode = self.world.resource::<Mode2d>();
        let doc = mode.doc.as_ref().expect("map");
        let (min, max) = doc.map.tile_world_rect(cell.0, cell.1);
        let [min, max] = [min, max].map(|p| p.map(|v| v as f32));
        let center = Vec3::new((min[0] + max[0]) * 0.5, (min[1] + max[1]) * 0.5, 0.0);
        let pixel = self.view.project(mode.origin + center, PANE).expect("in view");
        egui::pos2(pixel.x, pixel.y)
    }

    fn button(
        &mut self,
        pos: egui::Pos2,
        button: egui::PointerButton,
        pressed: bool,
        modifiers: egui::Modifiers,
    ) {
        self.frame(vec![egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers,
        }]);
    }

    fn click(&mut self, cell: (u32, u32), button: egui::PointerButton, modifiers: egui::Modifiers) {
        let pos = self.at(cell);
        self.frame(vec![
            egui::Event::ModifiersChanged(modifiers),
            egui::Event::PointerMoved(pos),
        ]);
        self.button(pos, button, true, modifiers);
        self.button(pos, button, false, modifiers);
        self.frame(vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE)]);
    }

    fn drag(&mut self, cells: &[(u32, u32)]) {
        let first = self.at(cells[0]);
        self.frame(vec![egui::Event::PointerMoved(first)]);
        self.button(first, egui::PointerButton::Primary, true, egui::Modifiers::NONE);
        for cell in &cells[1..] {
            let pos = self.at(*cell);
            self.frame(vec![egui::Event::PointerMoved(pos)]);
        }
        let last = self.at(cells[cells.len() - 1]);
        self.button(last, egui::PointerButton::Primary, false, egui::Modifiers::NONE);
    }

    fn mode(&self) -> &Mode2d {
        self.world.resource::<Mode2d>()
    }

    fn undo_depth(&self) -> usize {
        self.world
            .get_resource::<UndoStack>()
            .map_or(0, UndoStack::undo_depth)
    }
}

#[test]
fn a_rectangle_drag_in_the_pane_previews_then_lands_as_one_entry() {
    let mut pane = Pane::new();
    pane.world.resource_mut::<Mode2d>().tool = Tool2d::Rect;
    let first = pane.at((1, 1));
    pane.frame(vec![egui::Event::PointerMoved(first)]);
    pane.button(first, egui::PointerButton::Primary, true, egui::Modifiers::NONE);
    let corner = pane.at((3, 2));
    pane.frame(vec![egui::Event::PointerMoved(corner)]);
    assert_eq!(
        pane.mode().shape_preview().len(),
        6,
        "the 3x2 rectangle is outlined"
    );
    assert!(ground(pane.mode()).iter().all(|&g| g == 0));
    pane.button(corner, egui::PointerButton::Primary, false, egui::Modifiers::NONE);
    assert_eq!(&ground(pane.mode())[6..12], &[0, 1, 1, 1, 0, 0]);
    assert_eq!(&ground(pane.mode())[12..18], &[0, 1, 1, 1, 0, 0]);
    assert_eq!(pane.undo_depth(), 1);
}

#[test]
fn right_click_erases_in_every_tool_and_the_bucket_erases_its_region() {
    let mut pane = Pane::new();
    pane.drag(&[(0, 1), (5, 1)]);
    assert_eq!(&ground(pane.mode())[6..12], &[1; 6]);
    for (tool, cell) in [
        (Tool2d::Paint, (0, 1)),
        (Tool2d::Line, (1, 1)),
        (Tool2d::Rect, (2, 1)),
        (Tool2d::Pick, (3, 1)),
    ] {
        pane.world.resource_mut::<Mode2d>().tool = tool;
        pane.click(cell, egui::PointerButton::Secondary, egui::Modifiers::NONE);
        assert_eq!(
            ground(pane.mode())[6 + cell.0 as usize],
            0,
            "{tool:?} right-click erased"
        );
    }
    assert_eq!(pane.undo_depth(), 5, "one entry per right-click");
    pane.world.resource_mut::<Mode2d>().tool = Tool2d::Bucket;
    pane.click((5, 1), egui::PointerButton::Secondary, egui::Modifiers::NONE);
    assert!(
        ground(pane.mode()).iter().all(|&g| g == 0),
        "the bucket erased (4, 1) and (5, 1)"
    );
    assert_eq!(pane.undo_depth(), 6);
}

#[test]
fn the_pick_tool_and_ctrl_click_pick_a_tile_and_a_drag_picks_a_stamp() {
    let mut pane = Pane::new();
    pane.world.resource_mut::<Mode2d>().brush = Brush2d::Tile(6);
    pane.drag(&[(0, 0), (2, 0)]);
    pane.world.resource_mut::<Mode2d>().brush = Brush2d::Tile(2);
    pane.click((1, 0), egui::PointerButton::Primary, egui::Modifiers::COMMAND);
    assert_eq!(pane.mode().brush, Brush2d::Tile(6), "Ctrl+click picked the tile");
    pane.world.resource_mut::<Mode2d>().brush = Brush2d::Tile(2);
    pane.world.resource_mut::<Mode2d>().tool = Tool2d::Pick;
    pane.click((2, 0), egui::PointerButton::Primary, egui::Modifiers::NONE);
    assert_eq!(
        pane.mode().brush,
        Brush2d::Tile(6),
        "the pick tool picked the tile"
    );
    pane.drag(&[(0, 0), (2, 1)]);
    assert_eq!(pane.mode().brush, Brush2d::Stamp);
    assert_eq!(
        pane.mode().stamp.as_ref().map(|s| (s.width, s.height)),
        Some((3, 2))
    );
    assert_eq!(pane.undo_depth(), 1, "picking paints nothing");
}

#[test]
fn the_tool_buttons_and_options_work_from_the_keyboard() {
    use crate::canvas_keys::test_input::{frame, key, tab_to};
    let ctx = egui::Context::default();
    let mut world = world_with(mode());
    let size = [420.0, 1600.0];
    let mut draw = |ui: &mut egui::Ui| super::panel::mode2d_panel(ui, &mut world);
    frame(&ctx, size, vec![], &mut draw);
    tab_to(&ctx, size, "Rectangle", false, 80, &mut draw);
    frame(
        &ctx,
        size,
        vec![key(egui::Key::Space, egui::Modifiers::NONE)],
        &mut draw,
    );
    frame(&ctx, size, vec![], &mut draw);
    tab_to(&ctx, size, "Filled", false, 20, &mut draw);
    frame(
        &ctx,
        size,
        vec![key(egui::Key::Space, egui::Modifiers::NONE)],
        &mut draw,
    );
    frame(&ctx, size, vec![], &mut draw);
    // The Random brush section opens from the keyboard, then its checkbox turns it on.
    tab_to(&ctx, size, "Random brush", false, 40, &mut draw);
    frame(
        &ctx,
        size,
        vec![key(egui::Key::Space, egui::Modifiers::NONE)],
        &mut draw,
    );
    frame(&ctx, size, vec![], &mut draw);
    tab_to(&ctx, size, "Paint random tiles", false, 10, &mut draw);
    frame(
        &ctx,
        size,
        vec![key(egui::Key::Space, egui::Modifiers::NONE)],
        &mut draw,
    );
    frame(&ctx, size, vec![], &mut draw);
    drop(draw);
    let mode = world.resource::<Mode2d>();
    assert_eq!(mode.tool, Tool2d::Rect);
    assert!(!mode.rect_filled, "Space on Filled turned it off");
    assert!(mode.random.is_some(), "Space on Random brush turned it on");
}

// ---- Layer tree (`editor.tile_layers`) ------------------------------------------------------

#[test]
fn the_layer_tree_lists_typed_layers_selects_moves_and_edits_objects_with_undo() {
    let mut world = world_with(mode());
    let out = layers::layer_action(&mut world, &json!({})).expect("lists");
    assert_eq!(out["layers"][0]["kind"], "objects");
    assert_eq!(out["layers"][1]["kind"], "tiles");
    assert_eq!(out["layers"][1]["depth"], 1);
    assert_eq!(out["layers"][1]["active"], true);
    // Object layers: add, change, delete, each one undo entry.
    let out = layers::layer_action(
        &mut world,
        &json!({ "action": "object_add", "layer": "spawns", "object": { "name": "start", "class": "spawn", "position": [16.0, 32.0], "size": [16.0, 16.0], "properties": { "team": 2, "boss": false } } }),
    )
    .expect("adds");
    let id = out["id"].as_u64().expect("id");
    assert_eq!(out["object_layer"], 0);
    assert_eq!(out["selected_object"], id);
    assert_eq!(
        out["layers"][0]["objects"][0]["properties"],
        json!({ "team": 2, "boss": false })
    );
    layers::layer_action(
        &mut world,
        &json!({ "action": "object_set", "id": id, "object": { "position": [48.0, 32.0], "properties": { "boss": null } } }),
    )
    .expect("moves");
    let object = |world: &World| {
        world
            .resource::<Mode2d>()
            .doc
            .as_ref()
            .and_then(|d| d.map.object_layer(0))
            .map(|l| l.objects.clone())
    };
    assert_eq!(
        object(&world).map(|o| (o[0].position, o[0].properties.len())),
        Some(([48.0, 32.0], 1))
    );
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 2);
    assert!(crate::command::undo_world(&mut world));
    assert_eq!(object(&world).map(|o| o[0].position), Some([16.0, 32.0]));
    assert!(crate::command::redo_world(&mut world));
    layers::layer_action(
        &mut world,
        &json!({ "action": "object_delete", "layer": 0, "id": id }),
    )
    .expect("deletes");
    assert_eq!(object(&world).map(|o| o.len()), Some(0));
    assert_eq!(world.resource::<Mode2d>().selected_object, None);
    assert!(crate::command::undo_world(&mut world));
    assert_eq!(object(&world).map(|o| o.len()), Some(1));
    // Depth: moving the object layer above the tile layer keeps the paint layer on "ground".
    let out = layers::layer_action(
        &mut world,
        &json!({ "action": "move", "layer": "spawns", "depth": 1 }),
    )
    .expect("moves");
    assert_eq!(out["layers"][0]["name"], "ground");
    assert_eq!(out["active"], 0, "the paint layer followed its layer");
    assert_eq!(out["object_layer"], 1);
    assert!(crate::command::undo_world(&mut world));
    let out = layers::layer_action(&mut world, &json!({})).expect("lists");
    assert_eq!(out["layers"][0]["name"], "spawns");
    assert_eq!(out["active"], 1);
    // Select: a tile layer becomes the paint layer and leaves object editing.
    let out =
        layers::layer_action(&mut world, &json!({ "action": "select", "layer": "ground" })).expect("selects");
    assert_eq!(
        (out["active"].clone(), out["object_layer"].clone()),
        (json!(1), Value::Null)
    );
    for (params, expected) in [
        (
            json!({ "action": "move", "layer": 0, "depth": 9 }),
            "mode2d_bad_layer",
        ),
        (
            json!({ "action": "object_add", "layer": "ground" }),
            "mode2d_bad_layer",
        ),
        (
            json!({ "action": "object_add", "layer": 0, "object": { "position": [1.0] } }),
            "mode2d_bad_params",
        ),
        (
            json!({ "action": "object_add", "layer": 0, "object": { "wings": 2 } }),
            "mode2d_bad_params",
        ),
        (
            json!({ "action": "object_set", "layer": 0, "id": 99, "object": {} }),
            "mode2d_no_object",
        ),
        (
            json!({ "action": "object_delete", "layer": 0, "id": 99 }),
            "mode2d_no_object",
        ),
        (
            json!({ "action": "select", "layer": 0, "id": 99 }),
            "mode2d_no_object",
        ),
        (json!({ "action": "fly" }), "mode2d_bad_params"),
    ] {
        let err = layers::layer_action(&mut world, &params).expect_err("refused");
        assert_eq!(err.code(), expected, "{params}");
    }
    let mut empty = World::new();
    assert_eq!(
        layers::layer_action(&mut empty, &json!({})).map_err(|e| e.code()),
        Err("mode2d_no_map")
    );
}

#[test]
fn the_layer_tree_and_guides_work_from_the_keyboard() {
    use crate::canvas_keys::test_input::{frame, key, tab_to};
    let ctx = egui::Context::default();
    let mut world = world_with(mode());
    let size = [420.0, 2400.0];
    let mut draw = |ui: &mut egui::Ui| super::panel::mode2d_panel(ui, &mut world);
    frame(&ctx, size, vec![], &mut draw);
    let press = |draw: &mut dyn FnMut(&mut egui::Ui)| {
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Space, egui::Modifiers::NONE)],
            &mut *draw,
        );
        frame(&ctx, size, vec![], &mut *draw);
    };
    tab_to(&ctx, size, "spawns", false, 80, &mut draw);
    press(&mut draw);
    tab_to(&ctx, size, "Add object", false, 20, &mut draw);
    press(&mut draw);
    tab_to(&ctx, size, "Rulers and guides", false, 80, &mut draw);
    press(&mut draw);
    tab_to(&ctx, size, "Add vertical guide", false, 10, &mut draw);
    press(&mut draw);
    drop(draw);
    let mode = world.resource::<Mode2d>();
    assert_eq!(mode.object_layer, Some(0));
    assert!(
        mode.selected_object.is_some(),
        "Add object added and selected one"
    );
    assert_eq!(mode.guides.guides.len(), 1);
    assert_eq!(mode.guides.guides[0].at, 48.0, "through the map's center");
}

#[test]
fn a_guide_drags_out_of_the_ruler_and_back_onto_it_to_go_away() {
    let mut pane = Pane::new();
    let press = egui::pos2(PANE.x * 0.5, guides::RULER_PX * 0.5);
    let to = pane.at((2, 2));
    pane.frame(vec![egui::Event::PointerMoved(press)]);
    pane.button(press, egui::PointerButton::Primary, true, egui::Modifiers::NONE);
    pane.frame(vec![egui::Event::PointerMoved(press + egui::vec2(0.0, 40.0))]);
    pane.frame(vec![egui::Event::PointerMoved(to)]);
    pane.button(to, egui::PointerButton::Primary, false, egui::Modifiers::NONE);
    let guides = pane.mode().guides.guides.clone();
    assert_eq!(guides.len(), 1);
    assert_eq!(guides[0].axis, guides::GuideAxis::Horizontal);
    // Row 2 of a 4-row, 16 px map: level y from 16 to 32.
    assert!((16.0..=32.0).contains(&guides[0].at), "{}", guides[0].at);
    assert!(
        ground(pane.mode()).iter().all(|&g| g == 0),
        "pulling a guide paints nothing"
    );
    assert_eq!(pane.undo_depth(), 0);
    // Pick it up again and drop it on the left ruler.
    pane.frame(vec![egui::Event::PointerMoved(to)]);
    pane.button(to, egui::PointerButton::Primary, true, egui::Modifiers::NONE);
    let ruler = egui::pos2(guides::RULER_PX * 0.5, to.y);
    pane.frame(vec![egui::Event::PointerMoved(to + egui::vec2(-30.0, 0.0))]);
    pane.frame(vec![egui::Event::PointerMoved(ruler)]);
    pane.button(ruler, egui::PointerButton::Primary, false, egui::Modifiers::NONE);
    assert!(pane.mode().guides.guides.is_empty());
    assert!(ground(pane.mode()).iter().all(|&g| g == 0));
}

// ---- Saved patterns (`editor.tile_pattern_*`) ------------------------------------------------

#[test]
fn a_stamp_saves_as_a_named_pattern_in_the_tileset_sidecar_and_comes_back() {
    let dir = project();
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.clone()),
    });
    let mut m = Mode2d::default();
    m.load_map(&dir.join("level.tilemap.ron"), false).expect("loads");
    m.brush = Brush2d::Tile(3 | FLIP_X_BIT);
    m.paint_now(&[(0, 0), (1, 0)], false).expect("paints");
    m.copy_stamp((0, 0), (2, 0)).expect("copies");
    world.insert_resource(m);
    let call = |world: &mut World, name: &str, params: Value| {
        let handler = patterns::methods()
            .into_iter()
            .find(|(s, _)| s.name == name)
            .map(|(_, h)| h)
            .expect("method");
        handler(world, &params)
    };
    let out = call(&mut world, "editor.tile_pattern_list", json!({})).expect("lists");
    assert_eq!(out["patterns"], json!([]), "no sidecar yet: an empty library");
    let out = call(&mut world, "editor.tile_pattern_save", json!({ "name": "fence" })).expect("saves");
    assert_eq!(
        out["patterns"],
        json!([{ "name": "fence", "width": 3, "height": 1 }])
    );
    let sidecar = dir.join("t.tilepatterns.ron");
    let lib = regia_2d::tilepatterns::TilePatternLibrary::from_ron(
        &std::fs::read_to_string(&sidecar).expect("written"),
    )
    .expect("parses");
    assert_eq!(
        lib.patterns[0].cells,
        vec![3 | FLIP_X_BIT, 3 | FLIP_X_BIT, 0],
        "local 2 is stored as 3"
    );
    // A fresh session (another map load) reads it back and pastes it.
    let mut m = world.remove_resource::<Mode2d>().expect("mode");
    m.load_map(&dir.join("level.tilemap.ron"), true).expect("reloads");
    m.layer = 1;
    world.insert_resource(m);
    let out = call(&mut world, "editor.tile_pattern_use", json!({ "name": "fence" })).expect("uses");
    assert_eq!(out["brush"], json!({ "stamp": true }));
    let stamp = world.resource::<Mode2d>().stamp.clone().expect("stamp");
    assert_eq!(stamp.cells, vec![3 | FLIP_X_BIT, 3 | FLIP_X_BIT, 0]);
    for (name, params, expected) in [
        (
            "editor.tile_pattern_use",
            json!({ "name": "gate" }),
            "mode2d_no_pattern",
        ),
        (
            "editor.tile_pattern_delete",
            json!({ "name": "gate" }),
            "mode2d_no_pattern",
        ),
        ("editor.tile_pattern_save", json!({}), "mode2d_bad_params"),
        (
            "editor.tile_pattern_save",
            json!({ "name": "  " }),
            "mode2d_bad_patterns",
        ),
        (
            "editor.tile_pattern_list",
            json!({ "tileset": "nope" }),
            "mode2d_bad_params",
        ),
    ] {
        let err = call(&mut world, name, params.clone()).expect_err("refused");
        assert_eq!(code(&err), expected, "{name} {params}");
    }
    let out = call(
        &mut world,
        "editor.tile_pattern_delete",
        json!({ "name": "fence" }),
    )
    .expect("deletes");
    assert_eq!(out["patterns"], json!([]));
    world.resource_mut::<Mode2d>().stamp = None;
    let err = call(&mut world, "editor.tile_pattern_save", json!({ "name": "x" })).expect_err("no stamp");
    assert_eq!(code(&err), "mode2d_no_stamp");
    // A broken sidecar is reported, not overwritten.
    std::fs::write(&sidecar, "not ron").expect("write");
    let mut m = world.remove_resource::<Mode2d>().expect("mode");
    m.patterns = None;
    world.insert_resource(m);
    let err = call(&mut world, "editor.tile_pattern_list", json!({})).expect_err("broken");
    assert_eq!(code(&err), "mode2d_bad_patterns");
    assert_eq!(std::fs::read_to_string(&sidecar).ok().as_deref(), Some("not ron"));
    let _ = std::fs::remove_dir_all(dir);
}

/// Review g17 row 7: a library cached for one sidecar is dropped when the next sidecar the stamp
/// points at does not parse; the error is returned and the file is left alone.
#[test]
fn a_broken_sidecar_drops_the_cached_library_of_another_one() {
    let dir = project();
    let mut m = Mode2d::default();
    m.load_map(&dir.join("level.tilemap.ron"), false).expect("loads");
    let sidecar = dir.join("t.tilepatterns.ron");
    assert!(m.pattern_library(0).is_ok(), "no sidecar yet: an empty library");
    // The cache now belongs to another file (as after a switch to another tileset), and this
    // tileset's sidecar is broken.
    if let Some(doc) = m.patterns.as_mut() {
        doc.path = dir.join("other.tilepatterns.ron");
    }
    std::fs::write(&sidecar, "not ron").expect("write");
    assert!(m.pattern_library(0).is_err(), "the broken file is reported");
    assert!(m.patterns.is_none(), "the other file's library is not kept");
    assert_eq!(std::fs::read_to_string(&sidecar).ok().as_deref(), Some("not ron"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_pattern_palette_saves_and_uses_from_the_keyboard() {
    use crate::canvas_keys::test_input::{frame, key, tab_to, text};
    let dir = project();
    let mut m = Mode2d::default();
    m.load_map(&dir.join("level.tilemap.ron"), false).expect("loads");
    m.layer = 1;
    m.brush = Brush2d::Tile(4);
    m.paint_now(&[(0, 0)], false).expect("paints");
    m.copy_stamp((0, 0), (0, 0)).expect("copies");
    let ctx = egui::Context::default();
    let mut world = world_with(m);
    let size = [420.0, 2400.0];
    let mut draw = |ui: &mut egui::Ui| super::panel::mode2d_panel(ui, &mut world);
    frame(&ctx, size, vec![], &mut draw);
    let space = vec![key(egui::Key::Space, egui::Modifiers::NONE)];
    tab_to(&ctx, size, "Patterns", false, 80, &mut draw);
    frame(&ctx, size, space.clone(), &mut draw);
    frame(&ctx, size, vec![], &mut draw);
    // The name field is named through `labelled_by`: Tab on to the next text input.
    let mut on_text = false;
    for _ in 0..10 {
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Tab, egui::Modifiers::NONE)],
            &mut draw,
        );
        let update = frame(&ctx, size, vec![], &mut draw);
        on_text = update
            .nodes
            .iter()
            .any(|(id, node)| *id == update.focus && node.role() == egui::accesskit::Role::TextInput);
        if on_text {
            break;
        }
    }
    assert!(on_text, "Tab reaches the pattern name field");
    frame(&ctx, size, vec![text("rock")], &mut draw);
    frame(&ctx, size, vec![], &mut draw);
    tab_to(&ctx, size, "Save stamp as pattern", false, 10, &mut draw);
    frame(&ctx, size, space.clone(), &mut draw);
    frame(&ctx, size, vec![], &mut draw);
    frame(&ctx, size, vec![], &mut draw);
    drop(draw);
    world.resource_mut::<Mode2d>().stamp = None;
    world.resource_mut::<Mode2d>().brush = Brush2d::Erase;
    let mut draw = |ui: &mut egui::Ui| super::panel::mode2d_panel(ui, &mut world);
    frame(&ctx, size, vec![], &mut draw);
    tab_to(&ctx, size, "rock", false, 150, &mut draw);
    frame(&ctx, size, space, &mut draw);
    frame(&ctx, size, vec![], &mut draw);
    drop(draw);
    let mode = world.resource::<Mode2d>();
    assert_eq!(mode.brush, Brush2d::Stamp, "Space on the pattern used it");
    assert_eq!(mode.stamp.as_ref().map(|s| s.cells.clone()), Some(vec![4]));
    assert!(dir.join("t.tilepatterns.ron").is_file());
    let _ = std::fs::remove_dir_all(dir);
}

// ---- Object drag with smart guides --------------------------------------------------------------

#[test]
fn dragging_an_object_snaps_to_its_neighbor_alt_drags_free_and_one_drag_is_one_entry() {
    let mut pane = Pane::new();
    // Two 16x16 objects: "a" at map px (0, 0) (level box x 0..16, y 48..64) and "b" at (64, 32).
    for (name, position) in [("a", [0.0, 0.0]), ("b", [64.0, 32.0])] {
        layers::layer_action(
            &mut pane.world,
            &json!({ "action": "object_add", "layer": 0, "object": { "name": name, "position": position, "size": [16.0, 16.0] } }),
        )
        .expect("adds");
    }
    let depth = pane.undo_depth();
    let level_to_screen = |pane: &Pane, p: [f32; 2]| {
        let mode = pane.mode();
        let v = pane
            .view
            .project(mode.origin + Vec3::new(p[0], p[1], 0.0), PANE)
            .expect("in view");
        egui::pos2(v.x, v.y)
    };
    let drag = |pane: &mut Pane, from: [f32; 2], to: [f32; 2], modifiers: egui::Modifiers| {
        let a = level_to_screen(pane, from);
        let b = level_to_screen(pane, to);
        pane.frame(vec![
            egui::Event::ModifiersChanged(modifiers),
            egui::Event::PointerMoved(a),
        ]);
        pane.button(a, egui::PointerButton::Primary, true, modifiers);
        pane.frame(vec![egui::Event::PointerMoved(a + (b - a) * 0.5)]);
        pane.frame(vec![egui::Event::PointerMoved(b)]);
        pane.button(b, egui::PointerButton::Primary, false, modifiers);
        pane.frame(vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE)]);
    };
    let position = |pane: &Pane, id: u32| {
        pane.mode()
            .doc
            .as_ref()
            .and_then(|d| d.map.object_layer(0))
            .and_then(|l| l.objects.iter().find(|o| o.id == id).map(|o| o.position))
            .expect("object")
    };
    // Grab "b" (level center 72, 24) and drop its left edge 0.5 units right of "a"'s right edge (16):
    // within the snap distance (6 px at about 0.18 units per px), x snaps to 16.
    drag(&mut pane, [72.0, 24.0], [24.5, 24.0], egui::Modifiers::NONE);
    assert_eq!(pane.mode().selected_object, Some(2));
    assert_eq!(
        position(&pane, 2)[0],
        16.0,
        "the left edge snapped to a's right edge"
    );
    assert_eq!(pane.undo_depth(), depth + 1, "one drag, one entry");
    assert!(crate::command::undo_world(&mut pane.world));
    assert_eq!(position(&pane, 2), [64.0, 32.0]);
    // With Alt the same drag lands where the pointer let go.
    drag(&mut pane, [72.0, 24.0], [27.2, 24.0], egui::Modifiers::ALT);
    assert!(
        (position(&pane, 2)[0] - 19.2).abs() < 0.5,
        "free: {:?}",
        position(&pane, 2)
    );
    // A click on empty space clears the selection; tiles are not painted while an object layer is active.
    pane.click((5, 3), egui::PointerButton::Primary, egui::Modifiers::NONE);
    assert_eq!(pane.mode().selected_object, None);
    assert!(ground(pane.mode()).iter().all(|&g| g == 0));
}
