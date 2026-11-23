// SPDX-License-Identifier: MPL-2.0
//! Headless panel-text checks for the w12 smoke's text bugs (ADR-8586): what a panel draws, read
//! from egui's shapes after a few frames. Bug 12 (Outliner rows of unnamed entities) and bug 25
//! (empty states that are one plain sentence, with no table header over nothing).

use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_scene::prelude::{EntityGuid, Name, Transform};

/// The texts `draw` paints in a `size` panel after three frames (layout settles over two).
fn panel_texts(world: &mut World, size: egui::Vec2, draw: fn(&mut egui::Ui, &mut World)) -> Vec<String> {
    shown_texts(world, size, draw)
        .into_iter()
        .map(|(text, _)| text)
        .collect()
}

/// [`panel_texts`] with whether each text shows whole (inside its clip rect and the screen).
fn shown_texts(
    world: &mut World,
    size: egui::Vec2,
    draw: fn(&mut egui::Ui, &mut World),
) -> Vec<(String, bool)> {
    let ctx = egui::Context::default();
    let mut texts = Vec::new();
    for frame in 0..3 {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            time: Some(f64::from(frame) * 0.1),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default().show(ui, |ui| draw(ui, world));
        });
        output.textures_delta.clear();
        texts.clear();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        for clipped in &output.shapes {
            collect(&clipped.shape, clipped.clip_rect.intersect(screen), &mut texts);
        }
    }
    texts
}

fn collect(shape: &egui::Shape, clip: egui::Rect, out: &mut Vec<(String, bool)>) {
    match shape {
        egui::Shape::Text(text) => {
            let rect = text.galley.rect.translate(text.pos.to_vec2());
            out.push((
                text.galley.text().to_owned(),
                clip.expand(0.5).contains_rect(rect),
            ));
        }
        egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| collect(s, clip, out)),
        _ => {}
    }
}

fn is_hex_id(text: &str) -> bool {
    text.len() == 16 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Bug 12: an entity without a Name shows `<Kind> (unnamed)`, never its 16-digit hex id; a named
/// entity keeps its name.
#[test]
fn unnamed_entities_show_their_kind_not_a_hex_id() {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>();
    let world = app.world_mut();
    world.spawn((
        Name::new("Hero".to_string()),
        Transform::default(),
        EntityGuid::new_random(),
    ));
    let unnamed = world.spawn((Transform::default(), EntityGuid::new_random())).id();
    regia_editor::visibility::mark_internal_entities(world);
    let texts = panel_texts(
        world,
        egui::vec2(600.0, 400.0),
        regia_editor::outliner::outliner_panel,
    );
    assert!(texts.iter().any(|t| t == "Hero"), "{texts:?}");
    assert!(
        texts.iter().any(|t| t.ends_with(" (unnamed)")),
        "the unnamed row reads `<Kind> (unnamed)`: {texts:?}"
    );
    let hex = format!("{:016x}", unnamed.to_bits());
    assert!(
        !texts.iter().any(|t| t.contains(&hex) || is_hex_id(t)),
        "no raw id as row text: {texts:?}"
    );
    // The agent still reads the id-based name.
    assert_eq!(regia_editor::outliner::display_name(world, unnamed), hex);
}

/// Bug 25: an empty Tests panel says so in one sentence and draws no table header.
#[test]
fn an_empty_tests_panel_is_one_sentence() {
    let mut world = World::new();
    let texts = panel_texts(
        &mut world,
        egui::vec2(600.0, 300.0),
        regia_editor::tests_panel::tests_panel,
    );
    assert!(
        texts.iter().any(|t| t == regia_editor::tests_panel::TESTS_EMPTY),
        "{texts:?}"
    );
    for header in ["tags", "owner", "last"] {
        assert!(
            !texts.iter().any(|t| t == header),
            "no `{header}` header over no rows: {texts:?}"
        );
    }
}

/// Bug 25: Size Map and Reference Graph without an asset say what to do in one sentence.
#[test]
fn size_map_and_reference_graph_without_an_asset_are_one_sentence() {
    let mut world = World::new();
    let texts = panel_texts(
        &mut world,
        egui::vec2(600.0, 300.0),
        regia_editor::asset_panels::size_map_panel,
    );
    assert!(
        texts
            .iter()
            .any(|t| t == regia_editor::asset_panels::SIZE_MAP_EMPTY),
        "{texts:?}"
    );
    let mut world = World::new();
    let texts = panel_texts(
        &mut world,
        egui::vec2(600.0, 300.0),
        regia_editor::asset_panels::reference_graph_panel,
    );
    assert!(
        texts
            .iter()
            .any(|t| t == regia_editor::asset_panels::REFERENCE_GRAPH_EMPTY),
        "{texts:?}"
    );
}

/// Bug 19: in a narrow dock the Reference Graph toolbar wraps, so "Open size map" shows whole.
#[test]
fn the_reference_graph_toolbar_wraps_in_a_narrow_dock() {
    let mut world = World::new();
    let texts = shown_texts(
        &mut world,
        egui::vec2(260.0, 300.0),
        regia_editor::asset_panels::reference_graph_panel,
    );
    assert!(
        texts.iter().any(|(t, whole)| t == "Open size map" && *whole),
        "the button text shows whole: {texts:?}"
    );
}
