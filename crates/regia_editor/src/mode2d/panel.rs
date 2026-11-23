// SPDX-License-Identifier: MPL-2.0
//! The 2D tab (`Panel::Mode2d`, `docs/specs/86-2d-toolkit.md` § 6): open a map and a rule set,
//! pick the layer and the brush, save. It draws the [`Mode2d`] resource the `editor.mode2d_*`
//! methods change, so a person and an agent see one state.

use std::path::{Path, PathBuf};

use regia_2d::stamp::StampOp;
use regia_2d::tilemap::{
    FLIP_DIAGONAL_BIT, FLIP_X_BIT, FLIP_Y_BIT, GID_MASK, MapLayer, TilesetDef, split_gid,
};
use regia_ecs::prelude::*;

use super::tiles::{self, TileTextures};
use super::{
    AutotileMode, Brush2d, Mode2d, Mode2dError, RandomBrush, RandomTile, Tool2d, open_map, save_tilemap,
};

/// Side of a palette cell, points.
const CELL: f32 = 32.0;
/// Gap between palette cells, points.
const GAP: f32 = 2.0;

/// The panel's state besides [`Mode2d`] (a world resource).
#[derive(Resource, Debug, Default)]
pub struct PanelState {
    /// The last failed action.
    pub error: Option<String>,
    /// A map waiting for a decision about the unsaved edits of the loaded one.
    pub pending_open: Option<PathBuf>,
    /// The tileset the palette shows.
    pub tileset: usize,
    /// The palette cell (local id) that takes the keyboard focus on the next draw, scrolled into
    /// view (arrow keys move it, ADR-7742).
    pub palette_focus: Option<u32>,
    /// Palette columns in the last frame (Up / Down step by one row).
    pub palette_columns: u32,
    /// A palette scroll offset to apply on the next draw (bring `palette_focus` into view).
    pub palette_scroll: Option<f32>,
    /// The local ids the palette drew in the last frame (`start..end`).
    pub palette_shown: (u32, u32),
    /// The map cursor's cell: arrow keys move it while it has the keyboard focus, Enter or Space
    /// paints the brush there and Delete erases it, one undo entry per press.
    pub cursor: (u32, u32),
    /// The name typed for Save stamp as pattern.
    pub pattern_name: String,
}

/// What a click asks for; applied after drawing (loading needs the world).
enum Action {
    Active(bool),
    Grid(bool),
    Play(bool),
    ResetClock,
    PickMap,
    PickRules,
    ClearRules,
    Save,
    ReloadImages,
    SaveThenOpen,
    DiscardThenOpen,
    CancelOpen,
    Layer(usize),
    Brush(Brush2d),
    Tileset(usize),
    Stamp(StampOp),
    ClearStamp,
    /// Move the palette's keyboard focus to a local id (and scroll it into view).
    PaletteFocus(u32),
    /// Move the map cursor to a cell (shown as the hover cell).
    Cursor(u32, u32),
    /// Paint (or erase) the map cursor's cell as one undo entry.
    PaintCursor(bool),
    /// The palette's column count changed.
    PaletteColumns(u32),
    /// The palette cell asked for took the focus.
    PaletteFocused,
    /// The palette drew local ids `.0..1` this frame.
    PaletteShown(u32, u32),
    /// The paint tool.
    Tool(Tool2d),
    /// The rectangle tool fills or outlines.
    RectFilled(bool),
    /// The bucket fills the connected region or every matching cell.
    Contiguous(bool),
    /// The terrain brush's connect or path mode.
    Autotile(AutotileMode),
    /// The random scatter brush (none: off).
    Random(Option<RandomBrush>),
    /// A layer-tree action (`editor.tile_layers` parameters), one undo entry when it edits.
    Layers(serde_json::Value),
    /// A rulers-and-guides action (`editor.mode2d_guides` parameters).
    Guides(serde_json::Value),
    /// A saved-pattern action (`list`, `save`, `use`, `delete`) with its parameters.
    Patterns(&'static str, serde_json::Value),
    /// Read the stamp tileset's patterns (the Patterns section is open).
    LoadPatterns,
    /// The typed pattern name.
    PatternName(String),
}

impl Action {
    /// Bookkeeping of the keyboard route: it neither reports nor clears an error.
    fn silent(&self) -> bool {
        matches!(
            self,
            Self::PaletteColumns(_)
                | Self::PaletteFocused
                | Self::PaletteShown(..)
                | Self::Cursor(..)
                | Self::LoadPatterns
                | Self::PatternName(_)
        )
    }
}

/// Palette rows of a tileset drawn `columns` wide.
#[must_use]
pub fn palette_rows(tile_count: u32, columns: u32) -> u32 {
    tile_count.div_ceil(columns.max(1))
}

/// The tile brush's GID with its flip bits swapped for `flips` (`x`, `y`, `diagonal`).
#[must_use]
pub fn with_flips(raw: u32, flips: [bool; 3]) -> u32 {
    let bit = |on: bool, bit: u32| if on { bit } else { 0 };
    (raw & GID_MASK)
        | bit(flips[0], FLIP_X_BIT)
        | bit(flips[1], FLIP_Y_BIT)
        | bit(flips[2], FLIP_DIAGONAL_BIT)
}

/// One tile of the palette, focusable and named `Tile <local>` for assistive technology.
fn palette_cell(
    ui: &mut egui::Ui,
    set: &TilesetDef,
    texture: Option<&egui::TextureHandle>,
    local: u32,
    on: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(CELL, CELL), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, true, on, format!("Tile {local}"))
    });
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        match (texture, tiles::corner_uvs(set, local, 0)) {
            (Some(texture), Some(uv)) => {
                let uv =
                    egui::Rect::from_min_max(egui::pos2(uv[0][0], uv[0][1]), egui::pos2(uv[2][0], uv[2][1]));
                painter.image(texture.id(), rect, uv, egui::Color32::WHITE);
            }
            _ => {
                painter.rect_filled(rect, 0.0, ui.visuals().faint_bg_color);
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    local.to_string(),
                    egui::FontId::proportional(10.0),
                    ui.visuals().text_color(),
                );
            }
        }
        if on {
            painter.rect_stroke(rect, 0.0, ui.visuals().selection.stroke, egui::StrokeKind::Inside);
        }
        if response.has_focus() {
            painter.rect_stroke(
                rect,
                0.0,
                ui.visuals().widgets.active.fg_stroke,
                egui::StrokeKind::Outside,
            );
        }
    }
    response.on_hover_text(format!(
        "{} tile {local}, gid {}",
        set.name,
        set.first_gid + local
    ))
}

/// The arrow keys of a focused palette cell: the local id the focus moves to (`columns` per row,
/// clamped to the tileset). Consumes the keys and stops egui's own arrow focus move.
fn palette_keys(ui: &egui::Ui, local: u32, columns: u32, count: u32) -> Option<u32> {
    use egui::{Key, Modifiers};
    let mut target = None;
    ui.input_mut(|input| {
        let columns = columns.max(1);
        if input.consume_key(Modifiers::NONE, Key::ArrowLeft) {
            target = local.checked_sub(1);
        }
        if input.consume_key(Modifiers::NONE, Key::ArrowRight) {
            target = Some(local + 1).filter(|&t| t < count);
        }
        if input.consume_key(Modifiers::NONE, Key::ArrowUp) {
            target = local.checked_sub(columns);
        }
        if input.consume_key(Modifiers::NONE, Key::ArrowDown) {
            target = Some(local + columns).filter(|&t| t < count);
        }
    });
    if target.is_some() {
        ui.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
    }
    target
}

/// The map cursor: a focusable row named "Map cursor". With the focus, the arrows move the cell
/// (clamped to the map), Enter or Space paints the brush there, Delete erases it.
fn map_cursor(ui: &mut egui::Ui, mode: &Mode2d, state: &PanelState, actions: &mut Vec<Action>) {
    use egui::{Key, Modifiers};
    let Some(doc) = &mode.doc else { return };
    let (w, h) = (doc.map.width, doc.map.height);
    let (x, y) = (state.cursor.0.min(w - 1), state.cursor.1.min(h - 1));
    let text = format!("Map cursor: cell ({x}, {y}). Arrows move, Enter paints, Delete erases");
    let galley = ui.painter().layout_no_wrap(
        text,
        egui::TextStyle::Body.resolve(ui.style()),
        ui.visuals().text_color(),
    );
    let (rect, response) = ui.allocate_exact_size(galley.size() + egui::vec2(8.0, 4.0), egui::Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Map cursor"));
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact(&response);
        ui.painter().rect_filled(rect, 2.0, visuals.weak_bg_fill);
        if response.has_focus() {
            ui.painter()
                .rect_stroke(rect, 2.0, ui.visuals().selection.stroke, egui::StrokeKind::Inside);
        }
        ui.painter()
            .galley(rect.min + egui::vec2(4.0, 2.0), galley, ui.visuals().text_color());
    }
    if response.has_focus() {
        let mut moved = None;
        let mut erase = false;
        ui.input_mut(|input| {
            for (key, dx, dy) in [
                (Key::ArrowLeft, -1i64, 0i64),
                (Key::ArrowRight, 1, 0),
                (Key::ArrowUp, 0, -1),
                (Key::ArrowDown, 0, 1),
            ] {
                if input.consume_key(Modifiers::NONE, key) {
                    let nx = (i64::from(x) + dx).clamp(0, i64::from(w) - 1) as u32;
                    let ny = (i64::from(y) + dy).clamp(0, i64::from(h) - 1) as u32;
                    moved = Some((nx, ny));
                }
            }
            erase = input.consume_key(Modifiers::NONE, Key::Delete);
        });
        if moved.is_some() {
            ui.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
        }
        let (cx, cy) = moved.unwrap_or((x, y));
        // The cursor shows in the pane as the hover cell while the row has the focus.
        if moved.is_some() || mode.hover != Some((cx, cy)) {
            actions.push(Action::Cursor(cx, cy));
        }
        if erase {
            actions.push(Action::PaintCursor(true));
        }
    }
    if response.clicked() {
        actions.push(Action::PaintCursor(false));
    }
}

fn tile_palette(
    ui: &mut egui::Ui,
    mode: &Mode2d,
    state: &PanelState,
    textures: Option<&TileTextures>,
    actions: &mut Vec<Action>,
) {
    let Some(doc) = &mode.doc else { return };
    let index = state.tileset.min(doc.map.tilesets.len().saturating_sub(1));
    let Some(set) = doc.map.tilesets.get(index) else {
        ui.weak("The map has no tileset.");
        return;
    };
    if doc.map.tilesets.len() > 1 {
        egui::ComboBox::from_id_salt("mode2d-tileset")
            .selected_text(set.name.as_str())
            .show_ui(ui, |ui| {
                for (i, other) in doc.map.tilesets.iter().enumerate() {
                    if ui.selectable_label(i == index, other.name.as_str()).clicked() {
                        actions.push(Action::Tileset(i));
                    }
                }
            });
    }
    if let Some(error) = doc.images.get(index).and_then(|i| i.error.as_ref()) {
        ui.colored_label(ui.visuals().warn_fg_color, error);
    }
    let selected = match mode.brush {
        Brush2d::Tile(raw) => Some(raw),
        _ => None,
    };
    if let Some(raw) = selected {
        ui.horizontal(|ui| {
            let mut flips = [
                raw & FLIP_X_BIT != 0,
                raw & FLIP_Y_BIT != 0,
                raw & FLIP_DIAGONAL_BIT != 0,
            ];
            let before = flips;
            ui.checkbox(&mut flips[0], "Flip X");
            ui.checkbox(&mut flips[1], "Flip Y");
            ui.checkbox(&mut flips[2], "Diagonal");
            if flips != before {
                actions.push(Action::Brush(Brush2d::Tile(with_flips(raw, flips))));
            }
        });
    }
    let columns = (((ui.available_width() + GAP) / (CELL + GAP)).floor() as u32).max(1);
    if columns != state.palette_columns {
        actions.push(Action::PaletteColumns(columns));
    }
    let rows = palette_rows(set.tile_count, columns) as usize;
    let texture = textures.and_then(|t| t.get(index));
    ui.scope(|ui| {
        // Before `show_rows`: it takes its row pitch from the spacing of this `Ui`.
        ui.spacing_mut().item_spacing = egui::vec2(GAP, GAP);
        let mut area = egui::ScrollArea::vertical().id_salt("mode2d-palette");
        if let Some(offset) = state.palette_scroll {
            area = area.vertical_scroll_offset(offset);
        }
        area.show_rows(ui, CELL, rows, |ui, range| {
            let shown = (
                range.start as u32 * columns,
                (range.end as u32 * columns).min(set.tile_count),
            );
            if shown != state.palette_shown {
                actions.push(Action::PaletteShown(shown.0, shown.1));
            }
            for row in range {
                ui.horizontal(|ui| {
                    let first = row as u32 * columns;
                    for local in first..(first + columns).min(set.tile_count) {
                        let gid = set.first_gid + local;
                        let on = selected.is_some_and(|raw| split_gid(raw).0 == gid);
                        let response = palette_cell(ui, set, texture, local, on);
                        if state.palette_focus == Some(local) {
                            response.request_focus();
                            actions.push(Action::PaletteFocused);
                        }
                        if response.has_focus()
                            && let Some(target) = palette_keys(ui, local, columns, set.tile_count)
                        {
                            actions.push(Action::PaletteFocus(target));
                        }
                        // Enter or Space on the focused cell clicks it (egui's keyboard click).
                        if response.clicked() {
                            let flips = selected.unwrap_or(0) & !GID_MASK;
                            actions.push(Action::Brush(Brush2d::Tile(gid | flips)));
                            // With the random brush on, a pick adds the tile to its set.
                            if let Some(random) = &mode.random
                                && !random.tiles.iter().any(|t| t.raw & GID_MASK == gid)
                                && random.tiles.len() < super::tools::MAX_RANDOM_TILES
                            {
                                let mut random = random.clone();
                                random.tiles.push(RandomTile {
                                    raw: gid | flips,
                                    weight: 1,
                                });
                                actions.push(Action::Random(Some(random)));
                            }
                        }
                    }
                });
            }
        });
    });
}

fn draw(ui: &mut egui::Ui, mode: &Mode2d, state: &PanelState, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        let mut active = mode.active;
        if ui.checkbox(&mut active, "2D mode").changed() {
            actions.push(Action::Active(active));
        }
        let mut grid = mode.grid;
        if ui.checkbox(&mut grid, "Grid").changed() {
            actions.push(Action::Grid(grid));
        }
        // Animated tiles: play and pause per pane, back to time zero.
        let mut playing = mode.clock.playing;
        if ui.checkbox(&mut playing, "Play animations").changed() {
            actions.push(Action::Play(playing));
        }
        if ui.small_button("Rewind").clicked() {
            actions.push(Action::ResetClock);
        }
    });
    ui.horizontal(|ui| {
        if ui.button("Open map…").clicked() {
            actions.push(Action::PickMap);
        }
        let dirty = mode.doc.as_ref().is_some_and(|d| d.dirty);
        if ui.add_enabled(dirty, egui::Button::new("Save map")).clicked() {
            actions.push(Action::Save);
        }
        if ui
            .add_enabled(mode.doc.is_some(), egui::Button::new("Reload images"))
            .clicked()
        {
            actions.push(Action::ReloadImages);
        }
    });
    match &mode.doc {
        Some(doc) => {
            let name = doc.path.file_name().and_then(|n| n.to_str()).unwrap_or("map");
            ui.label(format!(
                "{name}{}: {} × {} tiles",
                if doc.dirty { " *" } else { "" },
                doc.map.width,
                doc.map.height
            ));
        }
        None => {
            ui.weak("No map. Open a .tilemap.ron.");
        }
    }
    ui.horizontal(|ui| {
        if ui.button("Open rules…").clicked() {
            actions.push(Action::PickRules);
        }
        if ui
            .add_enabled(mode.rules.is_some(), egui::Button::new("Clear rules"))
            .clicked()
        {
            actions.push(Action::ClearRules);
        }
    });
    if let Some(rules) = &mode.rules {
        let name = rules.path.file_name().and_then(|n| n.to_str()).unwrap_or("rules");
        ui.label(name);
    }
    if state.pending_open.is_some() {
        ui.separator();
        ui.colored_label(ui.visuals().warn_fg_color, "The loaded map has unsaved edits.");
        ui.horizontal(|ui| {
            if ui.button("Save and open").clicked() {
                actions.push(Action::SaveThenOpen);
            }
            if ui.button("Discard and open").clicked() {
                actions.push(Action::DiscardThenOpen);
            }
            if ui.button("Cancel").clicked() {
                actions.push(Action::CancelOpen);
            }
        });
    }
    if let Some(error) = &state.error {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }
    if mode.doc.is_none() {
        return;
    }

    ui.separator();
    layer_tree(ui, mode, actions);

    ui.separator();
    ui.strong("Brush");
    if ui
        .selectable_label(mode.brush == Brush2d::Erase, "Erase")
        .clicked()
    {
        actions.push(Action::Brush(Brush2d::Erase));
    }
    if let Some(rules) = &mode.rules {
        for terrain in &rules.set.terrains {
            let on = matches!(&mode.brush, Brush2d::Terrain(name) if *name == terrain.name);
            if ui.selectable_label(on, terrain.name.as_str()).clicked() {
                actions.push(Action::Brush(Brush2d::Terrain(terrain.name.clone())));
            }
        }
    }
    stamp_section(ui, mode, actions);
    tool_section(ui, mode, actions);
    random_section(ui, mode, actions);
    patterns_section(ui, mode, state, actions);
    guides_section(ui, mode, actions);
}

/// The pattern palette: the saved patterns of the stamp's tileset (Use, Delete) and Save stamp as
/// pattern under the typed name. Every control is a focusable widget with a spoken name.
fn patterns_section(ui: &mut egui::Ui, mode: &Mode2d, state: &PanelState, actions: &mut Vec<Action>) {
    use serde_json::json;
    egui::CollapsingHeader::new("Patterns")
        .id_salt("mode2d-patterns")
        .default_open(false)
        .show(ui, |ui| {
            actions.push(Action::LoadPatterns);
            if let Some(doc) = &mode.patterns {
                if doc.library.patterns.is_empty() {
                    ui.weak("No saved patterns for this tileset");
                }
                for pattern in &doc.library.patterns {
                    ui.horizontal(|ui| {
                        if ui
                            .button(pattern.name.as_str())
                            .on_hover_text(format!(
                                "Use pattern {} ({}x{}) as the stamp",
                                pattern.name, pattern.width, pattern.height
                            ))
                            .clicked()
                        {
                            actions.push(Action::Patterns("use", json!({ "name": pattern.name })));
                        }
                        if ui
                            .small_button("Delete")
                            .on_hover_text(format!("Delete pattern {}", pattern.name))
                            .clicked()
                        {
                            actions.push(Action::Patterns("delete", json!({ "name": pattern.name })));
                        }
                    });
                }
            }
            let mut name = state.pattern_name.clone();
            ui.horizontal(|ui| {
                let label = ui.label("Name");
                if ui.text_edit_singleline(&mut name).labelled_by(label.id).changed() {
                    actions.push(Action::PatternName(name.clone()));
                }
                let enabled = mode.stamp.is_some() && !name.trim().is_empty();
                if ui
                    .add_enabled(enabled, egui::Button::new("Save stamp as pattern"))
                    .clicked()
                {
                    actions.push(Action::Patterns("save", json!({ "name": name.trim() })));
                }
            });
        });
}

/// Rulers, snapping and the guide list: add a guide through the map's center, edit its position,
/// remove it. The pane does the same with the pointer (drag out of a ruler, drop back to remove).
fn guides_section(ui: &mut egui::Ui, mode: &Mode2d, actions: &mut Vec<Action>) {
    egui::CollapsingHeader::new("Rulers and guides")
        .id_salt("mode2d-guides")
        .default_open(false)
        .show(ui, |ui| guides_body(ui, mode, actions));
}

fn guides_body(ui: &mut egui::Ui, mode: &Mode2d, actions: &mut Vec<Action>) {
    use serde_json::json;
    ui.horizontal(|ui| {
        let mut rulers = mode.guides.rulers;
        if ui.checkbox(&mut rulers, "Rulers").changed() {
            actions.push(Action::Guides(json!({ "action": "set", "rulers": rulers })));
        }
        let mut snap = mode.guides.snap;
        if ui.checkbox(&mut snap, "Snap to guides").changed() {
            actions.push(Action::Guides(json!({ "action": "set", "snap": snap })));
        }
    });
    let center = mode.doc.as_ref().map_or([0.0, 0.0], |d| {
        let [w, h] = d.map.pixel_size();
        [w as f32 * 0.5, h as f32 * 0.5]
    });
    ui.horizontal(|ui| {
        if ui.button("Add vertical guide").clicked() {
            actions.push(Action::Guides(
                json!({ "action": "add", "axis": "vertical", "at": center[0] }),
            ));
        }
        if ui.button("Add horizontal guide").clicked() {
            actions.push(Action::Guides(
                json!({ "action": "add", "axis": "horizontal", "at": center[1] }),
            ));
        }
    });
    for (index, guide) in mode.guides.guides.iter().enumerate() {
        ui.horizontal(|ui| {
            let what = match guide.axis {
                super::guides::GuideAxis::Vertical => "Vertical guide x",
                super::guides::GuideAxis::Horizontal => "Horizontal guide y",
            };
            let label = ui.label(what);
            let mut at = guide.at;
            if ui
                .add(egui::DragValue::new(&mut at).speed(1.0))
                .labelled_by(label.id)
                .changed()
            {
                actions.push(Action::Guides(
                    json!({ "action": "move", "index": index, "at": at }),
                ));
            }
            if ui
                .small_button("Remove")
                .on_hover_text(format!("Remove guide {index}"))
                .clicked()
            {
                actions.push(Action::Guides(json!({ "action": "remove", "index": index })));
            }
        });
    }
}

/// The map's layers as a tree (map, then its layers top first, then an object layer's objects):
/// each row names its kind and depth; a tile layer row picks the paint layer, an object layer
/// row the edited object layer. "Raise" and "Lower" change a layer's depth. The selected object
/// has its fields below. Every control is a focusable widget with a spoken name.
fn layer_tree(ui: &mut egui::Ui, mode: &Mode2d, actions: &mut Vec<Action>) {
    use serde_json::json;
    let Some(doc) = &mode.doc else { return };
    let map_name = doc.path.file_name().and_then(|n| n.to_str()).unwrap_or("map");
    ui.strong("Layers").on_hover_text(format!(
        "The layers of {map_name}, top first (the top layer draws last)"
    ));
    let count = doc.map.layers.len();
    for (index, layer) in doc.map.layers.iter().enumerate().rev() {
        let (kind, on) = match layer {
            MapLayer::Tiles(_) => ("tiles", mode.object_layer.is_none() && index == mode.layer),
            MapLayer::Objects(_) => ("objects", mode.object_layer == Some(index)),
        };
        ui.horizontal(|ui| {
            ui.add_space(ui.spacing().indent);
            if ui.selectable_label(on, layer.name()).clicked() {
                match layer {
                    MapLayer::Tiles(_) => actions.push(Action::Layer(index)),
                    MapLayer::Objects(_) => {
                        actions.push(Action::Layers(json!({ "action": "select", "layer": index })));
                    }
                }
            }
            ui.weak(format!("{kind}, depth {index}"));
            let name = layer.name();
            if ui
                .add_enabled(index + 1 < count, egui::Button::new("Raise").small())
                .on_hover_text(format!("Draw {name} one layer higher"))
                .clicked()
            {
                actions.push(Action::Layers(
                    json!({ "action": "move", "layer": index, "depth": index + 1 }),
                ));
            }
            if ui
                .add_enabled(index > 0, egui::Button::new("Lower").small())
                .on_hover_text(format!("Draw {name} one layer lower"))
                .clicked()
            {
                actions.push(Action::Layers(
                    json!({ "action": "move", "layer": index, "depth": index - 1 }),
                ));
            }
        });
        if let MapLayer::Objects(objects) = layer
            && mode.object_layer == Some(index)
        {
            for object in &objects.objects {
                ui.horizontal(|ui| {
                    ui.add_space(ui.spacing().indent * 2.0);
                    let label = if object.name.is_empty() {
                        format!("Object {}", object.id)
                    } else {
                        format!("Object {} {}", object.id, object.name)
                    };
                    if ui
                        .selectable_label(mode.selected_object == Some(object.id), label)
                        .clicked()
                    {
                        actions.push(Action::Layers(
                            json!({ "action": "select", "layer": index, "id": object.id }),
                        ));
                    }
                });
            }
            object_fields(ui, mode, index, objects, actions);
        }
    }
}

/// The selected object's name, class, position and size, and Add object / Delete object.
fn object_fields(
    ui: &mut egui::Ui,
    mode: &Mode2d,
    layer: usize,
    objects: &regia_2d::tilemap::ObjectLayer,
    actions: &mut Vec<Action>,
) {
    use serde_json::json;
    ui.horizontal(|ui| {
        ui.add_space(ui.spacing().indent * 2.0);
        if ui.button("Add object").clicked() {
            // At the map cursor's cell, one tile in size.
            let doc = mode.doc.as_ref();
            let (tw, th) = doc.map_or((16.0, 16.0), |d| (d.map.tile_width as f32, d.map.tile_height as f32));
            let (cx, cy) = mode.hover.unwrap_or((0, 0));
            actions.push(Action::Layers(json!({
                "action": "object_add", "layer": layer,
                "object": { "name": "object", "position": [cx as f32 * tw, cy as f32 * th], "size": [tw, th] },
            })));
        }
        if let Some(id) = mode.selected_object
            && ui.button("Delete object").clicked()
        {
            actions.push(Action::Layers(json!({ "action": "object_delete", "layer": layer, "id": id })));
        }
    });
    let Some(object) = mode
        .selected_object
        .and_then(|id| objects.objects.iter().find(|o| o.id == id))
    else {
        return;
    };
    let mut name = object.name.clone();
    let mut class = object.class.clone();
    let mut position = object.position;
    let mut size = object.size;
    egui::Grid::new(("mode2d-object", object.id))
        .num_columns(2)
        .show(ui, |ui| {
            let label = ui.label("Name");
            ui.text_edit_singleline(&mut name).labelled_by(label.id);
            ui.end_row();
            let label = ui.label("Class");
            ui.text_edit_singleline(&mut class).labelled_by(label.id);
            ui.end_row();
            for (text, values) in [("Position", &mut position), ("Size", &mut size)] {
                let label = ui.label(text);
                ui.horizontal(|ui| {
                    for value in values.iter_mut() {
                        ui.add(egui::DragValue::new(value).speed(1.0))
                            .labelled_by(label.id);
                    }
                });
                ui.end_row();
            }
        });
    let mut changes = serde_json::Map::new();
    if name != object.name {
        changes.insert("name".into(), json!(name));
    }
    if class != object.class {
        changes.insert("class".into(), json!(class));
    }
    if position != object.position {
        changes.insert("position".into(), json!(position));
    }
    if size != object.size {
        changes.insert("size".into(), json!(size));
    }
    for (key, value) in &object.properties {
        ui.horizontal(|ui| {
            ui.add_space(ui.spacing().indent * 2.0);
            ui.weak(format!("{key}: {value:?}"));
        });
    }
    if !changes.is_empty() {
        actions.push(Action::Layers(json!({
            "action": "object_set", "layer": layer, "id": object.id, "object": changes,
        })));
    }
}

/// The paint tools, the rectangle and bucket options and the terrain mode. Every control is a
/// focusable widget with its text as its name (Tab reaches it, Space or Enter presses it).
fn tool_section(ui: &mut egui::Ui, mode: &Mode2d, actions: &mut Vec<Action>) {
    // One row: the tools, then the active tool's option.
    ui.horizontal_wrapped(|ui| {
        ui.strong("Tool");
        for tool in Tool2d::ALL {
            let button = ui.selectable_label(mode.tool == tool, tool.label());
            let button = if tool == Tool2d::Pick {
                button.on_hover_text(
                    "Click picks a tile, drag picks a stamp (Ctrl does the same with any tool)",
                )
            } else {
                button
            };
            if button.clicked() {
                actions.push(Action::Tool(tool));
            }
        }
        match mode.tool {
            Tool2d::Rect => {
                let mut filled = mode.rect_filled;
                if ui.checkbox(&mut filled, "Filled").changed() {
                    actions.push(Action::RectFilled(filled));
                }
            }
            Tool2d::Bucket => {
                let mut contiguous = mode.bucket_contiguous;
                if ui.checkbox(&mut contiguous, "Contiguous").changed() {
                    actions.push(Action::Contiguous(contiguous));
                }
            }
            Tool2d::Paint | Tool2d::Line | Tool2d::Pick => {}
        }
    });
    if mode.rules.is_some() {
        ui.horizontal(|ui| {
            ui.label("Terrain:");
            for (value, text) in [(AutotileMode::Connect, "Connect"), (AutotileMode::Path, "Path")] {
                if ui.selectable_label(mode.autotile_mode == value, text).clicked() {
                    actions.push(Action::Autotile(value));
                }
            }
        });
    }
}

/// The random scatter brush: on or off, the chance of nothing, each tile's weight. With it on, a
/// palette click adds the tile.
fn random_section(ui: &mut egui::Ui, mode: &Mode2d, actions: &mut Vec<Action>) {
    egui::CollapsingHeader::new("Random brush")
        .id_salt("mode2d-random")
        .default_open(mode.random.is_some())
        .show(ui, |ui| random_body(ui, mode, actions));
}

fn random_body(ui: &mut egui::Ui, mode: &Mode2d, actions: &mut Vec<Action>) {
    let mut on = mode.random.is_some();
    if ui.checkbox(&mut on, "Paint random tiles").changed() {
        let first = match mode.brush {
            Brush2d::Tile(raw) => raw,
            _ => 1,
        };
        actions.push(Action::Random(on.then(|| RandomBrush {
            tiles: vec![RandomTile {
                raw: first,
                weight: 1,
            }],
            nothing: 0.0,
            seed: 0,
        })));
        if on && !matches!(mode.brush, Brush2d::Tile(_)) {
            actions.push(Action::Brush(Brush2d::Tile(first)));
        }
    }
    let Some(random) = &mode.random else { return };
    let mut next = random.clone();
    let mut nothing = next.nothing * 100.0;
    ui.horizontal(|ui| {
        ui.label("Chance of nothing");
        ui.add(egui::Slider::new(&mut nothing, 0.0..=100.0).suffix(" %"));
    });
    let mut remove = None;
    for (index, tile) in next.tiles.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            let gid = tile.raw & GID_MASK;
            ui.label(format!("Tile {gid}"));
            ui.label("weight");
            ui.add(egui::DragValue::new(&mut tile.weight).range(1..=1000))
                .on_hover_text(format!("Weight of tile {gid}"));
            if ui
                .add_enabled(random.tiles.len() > 1, egui::Button::new("Remove"))
                .on_hover_text(format!("Remove tile {gid} from the random brush"))
                .clicked()
            {
                remove = Some(index);
            }
        });
    }
    if let Some(index) = remove {
        next.tiles.remove(index);
    }
    next.nothing = (nothing / 100.0).clamp(0.0, 1.0);
    if next != *random {
        actions.push(Action::Random(Some(next)));
    }
}

/// The stamp: select it as the brush, rotate or mirror it, drop it. Ctrl-drag in the front pane
/// copies a rectangle of the active layer.
fn stamp_section(ui: &mut egui::Ui, mode: &Mode2d, actions: &mut Vec<Action>) {
    ui.add_space(4.0);
    let Some(stamp) = &mode.stamp else {
        ui.weak("Stamp: Ctrl-drag cells in the pane to copy them");
        return;
    };
    ui.horizontal(|ui| {
        let label = format!("Stamp {}x{}", stamp.width, stamp.height);
        if ui.selectable_label(mode.brush == Brush2d::Stamp, label).clicked() {
            actions.push(Action::Brush(Brush2d::Stamp));
        }
        for (text, op) in [
            ("Rotate right", StampOp::RotateCw),
            ("Rotate left", StampOp::RotateCcw),
            ("Flip X", StampOp::FlipX),
            ("Flip Y", StampOp::FlipY),
        ] {
            if ui.small_button(text).clicked() {
                actions.push(Action::Stamp(op));
            }
        }
        if ui.small_button("Drop").clicked() {
            actions.push(Action::ClearStamp);
        }
    });
}

/// A native file dialog for `.ron` files, starting in the project.
fn pick(world: &World, title: &str, label: &str) -> Option<PathBuf> {
    let mut dialog = crate::file_dialog::FileDialog::new()
        .set_title(title)
        .add_filter(label, &["ron"]);
    if let Some(root) = crate::mesh_source::project_root(world) {
        dialog = dialog.set_directory(root);
    }
    dialog.pick_file()
}

/// Opens `path`, or holds it for the unsaved-edits question.
fn open(world: &mut World, path: PathBuf, discard: bool) -> Result<(), Mode2dError> {
    match open_map(world, &path, discard) {
        Err(Mode2dError::Unsaved) => {
            world
                .get_resource_or_insert_with(PanelState::default)
                .pending_open = Some(path);
            Ok(())
        }
        other => {
            let mut state = world.get_resource_or_insert_with(PanelState::default);
            state.pending_open = None;
            state.tileset = 0;
            other
        }
    }
}

fn apply_pending(world: &mut World, save_first: bool) -> Result<(), Mode2dError> {
    let Some(path) = world
        .get_resource_mut::<PanelState>()
        .and_then(|mut s| s.pending_open.take())
    else {
        return Ok(());
    };
    if save_first {
        let mut mode = world.get_resource_or_insert_with(Mode2d::default);
        save_tilemap(&mut mode, None)?;
    }
    open(world, path, true)
}

fn load_rules(world: &mut World, path: &Path) -> Result<(), Mode2dError> {
    world
        .get_resource_or_insert_with(Mode2d::default)
        .load_rules(path)
}

fn apply(world: &mut World, action: Action) -> Result<(), Mode2dError> {
    let mut mode = world.get_resource_or_insert_with(Mode2d::default);
    match action {
        Action::Active(active) => {
            mode.pending_frame = active;
            mode.active = active;
            Ok(())
        }
        Action::Grid(grid) => {
            mode.grid = grid;
            Ok(())
        }
        Action::Play(playing) => {
            mode.clock.playing = playing;
            Ok(())
        }
        Action::ResetClock => {
            mode.clock.reset();
            Ok(())
        }
        Action::Save => save_tilemap(&mut mode, None).map(|_| ()),
        Action::ReloadImages => {
            mode.reload_images();
            Ok(())
        }
        Action::ClearRules => {
            mode.rules = None;
            if matches!(mode.brush, Brush2d::Terrain(_)) {
                mode.brush = Brush2d::Erase;
            }
            Ok(())
        }
        Action::Layer(layer) => mode.select_layer(layer),
        Action::Tool(tool) => {
            mode.tool = tool;
            Ok(())
        }
        Action::RectFilled(filled) => {
            mode.rect_filled = filled;
            Ok(())
        }
        Action::Contiguous(contiguous) => {
            mode.bucket_contiguous = contiguous;
            Ok(())
        }
        Action::Autotile(value) => {
            mode.autotile_mode = value;
            Ok(())
        }
        Action::Random(random) => {
            mode.random = random;
            Ok(())
        }
        Action::Layers(params) => super::layers::layer_action(world, &params).map(|_| ()),
        Action::Guides(params) => super::guides::guide_action(&mut mode, &params).map(|_| ()),
        Action::Patterns(action, params) => {
            super::patterns::pattern_action(&mut mode, action, &params).map(|_| ())
        }
        Action::LoadPatterns => {
            // Read once per tileset (the library is cached on the mode); a broken file shows its
            // error, and the previous tileset's library is dropped with it (review g17 row 7).
            super::patterns::pattern_action(&mut mode, "list", &serde_json::json!({})).map(|_| ())
        }
        Action::PatternName(name) => {
            world
                .get_resource_or_insert_with(PanelState::default)
                .pattern_name = name;
            Ok(())
        }
        Action::Brush(brush) => {
            mode.brush = brush;
            Ok(())
        }
        Action::Tileset(tileset) => {
            world.get_resource_or_insert_with(PanelState::default).tileset = tileset;
            Ok(())
        }
        Action::Stamp(op) => mode.transform_stamp(op),
        Action::Cursor(x, y) => {
            mode.hover = Some((x, y));
            world.get_resource_or_insert_with(PanelState::default).cursor = (x, y);
            Ok(())
        }
        Action::PaintCursor(erase) => {
            let cell = world.get_resource_or_insert_with(PanelState::default).cursor;
            paint_cell(world, cell, erase)
        }
        Action::PaletteColumns(columns) => {
            world
                .get_resource_or_insert_with(PanelState::default)
                .palette_columns = columns;
            Ok(())
        }
        Action::PaletteShown(start, end) => {
            world
                .get_resource_or_insert_with(PanelState::default)
                .palette_shown = (start, end);
            Ok(())
        }
        Action::PaletteFocus(local) => {
            let mut state = world.get_resource_or_insert_with(PanelState::default);
            state.palette_focus = Some(local);
            if !(state.palette_shown.0..state.palette_shown.1).contains(&local) {
                let row = local / state.palette_columns.max(1);
                state.palette_scroll = Some(row as f32 * (CELL + GAP));
            }
            Ok(())
        }
        Action::PaletteFocused => {
            let mut state = world.get_resource_or_insert_with(PanelState::default);
            state.palette_focus = None;
            state.palette_scroll = None;
            Ok(())
        }
        Action::ClearStamp => {
            mode.clear_stamp();
            Ok(())
        }
        Action::PickMap => match pick(world, "Open tile map", "Tile map") {
            Some(path) => open(world, path, false),
            None => Ok(()),
        },
        Action::PickRules => match pick(world, "Open auto-tiling rules", "Auto-tiling rules") {
            Some(path) => load_rules(world, &path),
            None => Ok(()),
        },
        Action::SaveThenOpen => apply_pending(world, true),
        Action::DiscardThenOpen => apply_pending(world, false),
        Action::CancelOpen => {
            world
                .get_resource_or_insert_with(PanelState::default)
                .pending_open = None;
            Ok(())
        }
    }
}

/// Paints (or erases) one cell of the active layer with the brush as one undo entry, as
/// `editor.tile_stroke` with one cell does.
fn paint_cell(world: &mut World, cell: (u32, u32), erase: bool) -> Result<(), Mode2dError> {
    let mut mode = world.remove_resource::<Mode2d>().unwrap_or_default();
    mode.hover = Some(cell);
    let result = mode.paint_now(&[cell], erase);
    let command = match &result {
        Ok(changes) if !changes.is_empty() => mode.doc.as_ref().map(|doc| super::TilePaintCommand {
            path: doc.path.clone(),
            layer: mode.layer,
            changes: changes.clone(),
            erase,
        }),
        _ => None,
    };
    world.insert_resource(mode);
    result?;
    if let Some(command) = command {
        super::push_stroke(world, command);
    }
    Ok(())
}

/// Applies `actions` once per egui frame for the list named `list` (a second pass of the frame sees
/// the same keys: the map cursor would move two cells).
fn run(ctx: &egui::Context, list: &str, world: &mut World, actions: Vec<Action>) {
    if actions.is_empty()
        || !super::first_apply_this_frame(ctx, egui::Id::new(("mode2d-panel-applied-frame", list)))
    {
        return;
    }
    for action in actions {
        let silent = action.silent();
        let result = apply(world, action);
        if !silent || result.is_err() {
            world.resource_mut::<PanelState>().error = result.err().map(|e| e.to_string());
        }
    }
}

/// Draws the 2D tab: the map, rules, layer and brush controls, the new-map and tileset-binding
/// section ([`super::tilemap_new::section_ui`]), the rule editor
/// ([`super::autotile_editor::section_ui`]), the map cursor and the palette.
pub fn mode2d_panel(ui: &mut egui::Ui, world: &mut World) {
    tiles::sync_textures(ui.ctx(), world);
    world.get_resource_or_insert_with(Mode2d::default);
    world.get_resource_or_insert_with(PanelState::default);
    let mut actions = Vec::new();
    draw(
        ui,
        world.resource::<Mode2d>(),
        world.resource::<PanelState>(),
        &mut actions,
    );
    run(ui.ctx(), "controls", world, actions);
    ui.add_space(4.0);
    super::tilemap_new::section_ui(ui, world);
    super::autotile_editor::section_ui(ui, world);
    let mut actions = Vec::new();
    let (mode, state) = (world.resource::<Mode2d>(), world.resource::<PanelState>());
    if mode.doc.is_some() {
        ui.add_space(4.0);
        map_cursor(ui, mode, state, &mut actions);
        ui.add_space(4.0);
        tile_palette(
            ui,
            mode,
            state,
            world.get_resource::<TileTextures>(),
            &mut actions,
        );
    }
    run(ui.ctx(), "cursor", world, actions);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_rows_round_up_and_survive_a_zero_width() {
        assert_eq!(palette_rows(32, 8), 4);
        assert_eq!(palette_rows(33, 8), 5);
        assert_eq!(palette_rows(0, 8), 0);
        assert_eq!(palette_rows(3, 0), 3);
    }

    #[test]
    fn flips_replace_the_old_ones_and_keep_the_gid() {
        let raw = 7 | FLIP_X_BIT | FLIP_DIAGONAL_BIT;
        assert_eq!(with_flips(raw, [false, true, false]), 7 | FLIP_Y_BIT);
        assert_eq!(with_flips(raw, [false, false, false]), 7);
    }

    fn fixture() -> (PathBuf, World) {
        let dir = regia_test_utils::unique_temp_dir("mode2d-panel");
        let map = super::super::tests::map();
        for name in ["a.tilemap.ron", "b.tilemap.ron"] {
            std::fs::write(dir.join(name), map.to_ron().expect("serializes")).expect("write fixture");
        }
        let mut world = World::new();
        world.insert_resource(PanelState::default());
        open(&mut world, dir.join("a.tilemap.ron"), false).expect("opens");
        (dir, world)
    }

    fn dirty(world: &mut World) {
        let mut mode = world.resource_mut::<Mode2d>();
        mode.set_layer(1).expect("tile layer");
        mode.paint_now(&[(0, 0)], false).expect("paints");
    }

    #[test]
    fn opening_over_unsaved_edits_asks_and_discard_replaces_the_map() {
        let (dir, mut world) = fixture();
        dirty(&mut world);
        open(&mut world, dir.join("b.tilemap.ron"), false).expect("held, not an error");
        assert_eq!(
            world.resource::<PanelState>().pending_open,
            Some(dir.join("b.tilemap.ron"))
        );
        assert!(
            world.resource::<Mode2d>().doc.as_ref().is_some_and(|d| d.dirty),
            "not dropped yet"
        );
        apply_pending(&mut world, false).expect("discards");
        let mode = world.resource::<Mode2d>();
        let doc = mode.doc.as_ref().expect("map");
        assert_eq!(doc.path, dir.join("b.tilemap.ron"));
        assert!(!doc.dirty);
        assert!(world.resource::<PanelState>().pending_open.is_none());
    }

    #[test]
    fn save_and_open_writes_the_edits_first() {
        let (dir, mut world) = fixture();
        dirty(&mut world);
        open(&mut world, dir.join("b.tilemap.ron"), false).expect("held");
        apply_pending(&mut world, true).expect("saves and opens");
        let saved = std::fs::read_to_string(dir.join("a.tilemap.ron")).expect("read");
        let saved = regia_2d::tilemap::TileMapAsset::from_ron(&saved).expect("parses");
        assert_eq!(
            saved.tile_layer(1).expect("layer").data[0],
            1,
            "the stroke reached the file"
        );
    }

    #[test]
    fn clearing_the_rules_leaves_no_terrain_brush() {
        let (_dir, mut world) = fixture();
        world.resource_mut::<Mode2d>().brush = Brush2d::Terrain("wall".into());
        apply(&mut world, Action::ClearRules).expect("clears");
        assert_eq!(world.resource::<Mode2d>().brush, Brush2d::Erase);
    }
}
