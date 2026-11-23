// SPDX-License-Identifier: MPL-2.0
//! Keyboard route of the 2D tab (a11y FU-5 remainder, ADR-7742): the tile palette and the map
//! cursor work with no pointer at all. Synthetic key events through an `egui::Context`.

use super::panel::mode2d_panel;
use super::*;
use crate::canvas_keys::test_input::{focused_name, frame, key, tab_to};
use egui::{Key, Modifiers};

const SIZE: [f32; 2] = [400.0, 900.0];

fn world() -> World {
    let dir = super::tests::project();
    let mut world = World::new();
    let mut mode = Mode2d::default();
    mode.load_map(&dir.join("level.tilemap.ron"), false)
        .expect("loads");
    world.insert_resource(mode);
    world
}

fn press(ctx: &egui::Context, world: &mut World, k: Key) -> Option<String> {
    frame(ctx, SIZE, vec![key(k, Modifiers::NONE)], |ui| {
        mode2d_panel(ui, world)
    });
    let update = frame(ctx, SIZE, vec![], |ui| mode2d_panel(ui, world));
    focused_name(&update)
}

#[test]
fn arrows_move_the_palette_selection_and_enter_picks_the_tile() {
    let ctx = egui::Context::default();
    let mut world = world();
    tab_to(&ctx, SIZE, "Tile 0", false, 80, |ui| mode2d_panel(ui, &mut world));
    assert_eq!(
        press(&ctx, &mut world, Key::ArrowRight).as_deref(),
        Some("Tile 1")
    );
    assert_eq!(
        press(&ctx, &mut world, Key::ArrowRight).as_deref(),
        Some("Tile 2")
    );
    assert_eq!(press(&ctx, &mut world, Key::ArrowLeft).as_deref(), Some("Tile 1"));
    press(&ctx, &mut world, Key::Enter);
    assert_eq!(
        world.resource::<Mode2d>().brush,
        Brush2d::Tile(2),
        "local 1 is gid 2"
    );
}

#[test]
fn the_map_cursor_paints_one_cell_from_the_keyboard_as_one_undo_entry() {
    let ctx = egui::Context::default();
    let mut world = world();
    world.resource_mut::<Mode2d>().brush = Brush2d::Tile(6);
    tab_to(&ctx, SIZE, "Map cursor", false, 80, |ui| {
        mode2d_panel(ui, &mut world)
    });
    press(&ctx, &mut world, Key::ArrowRight);
    press(&ctx, &mut world, Key::ArrowDown);
    assert_eq!(
        world.resource::<Mode2d>().hover,
        Some((1, 1)),
        "the cursor shows as the hover cell"
    );
    press(&ctx, &mut world, Key::Enter);
    let cell = |world: &World| super::tests::ground(world.resource::<Mode2d>())[6 + 1];
    assert_eq!(cell(&world), 6, "cell (1, 1) holds the brush");
    assert_eq!(
        world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth),
        1
    );
    press(&ctx, &mut world, Key::Delete);
    assert_eq!(cell(&world), 0, "Delete erases the cell");
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 2);
    assert!(crate::command::undo_world(&mut world));
    assert_eq!(cell(&world), 6, "undo restores the painted tile");
}

/// One frame of key `k` whose first pass asks egui for a discard, so the frame runs twice with the
/// same input (as a layout change does); then one plain frame. Returns the key frame's passes.
fn press_with_second_pass(ctx: &egui::Context, world: &mut World, k: Key) -> usize {
    let mut passes = 0;
    frame(ctx, SIZE, vec![key(k, Modifiers::NONE)], |ui| {
        passes += 1;
        if ui.ctx().current_pass_index() == 0 {
            ui.ctx().request_discard("test: a second pass of the key frame");
        }
        mode2d_panel(ui, world);
    });
    frame(ctx, SIZE, vec![], |ui| mode2d_panel(ui, world));
    passes
}

/// Review finding 7 of the w11 tilemap lane (ADR-7742): egui runs a frame's UI twice when a pass
/// asks for a discard, and the second pass sees the same keys. The 2D tab applies its action list
/// once per frame, so an arrow moves the map cursor one cell (not two) and Enter paints one undo
/// entry.
#[test]
fn a_second_pass_of_the_key_frame_moves_and_paints_once() {
    let ctx = egui::Context::default();
    let mut world = world();
    world.resource_mut::<Mode2d>().brush = Brush2d::Tile(6);
    tab_to(&ctx, SIZE, "Map cursor", false, 80, |ui| {
        mode2d_panel(ui, &mut world)
    });
    assert_eq!(
        press_with_second_pass(&ctx, &mut world, Key::ArrowRight),
        2,
        "the key frame ran twice"
    );
    assert_eq!(
        world.resource::<Mode2d>().hover,
        Some((1, 0)),
        "one cell per press, not one per pass"
    );
    press_with_second_pass(&ctx, &mut world, Key::ArrowDown);
    assert_eq!(world.resource::<Mode2d>().hover, Some((1, 1)));
    press_with_second_pass(&ctx, &mut world, Key::Enter);
    let cell = |world: &World| super::tests::ground(world.resource::<Mode2d>())[6 + 1];
    assert_eq!(cell(&world), 6, "cell (1, 1) holds the brush");
    assert_eq!(
        world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth),
        1,
        "one undo entry"
    );
}
