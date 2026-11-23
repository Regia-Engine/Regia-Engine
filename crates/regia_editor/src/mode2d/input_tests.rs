// SPDX-License-Identifier: MPL-2.0
//! The 2D mode's pointer and keyboard paths through real egui input (`docs/specs/86-2d-toolkit.md`
//! § 8 item 11): no window, but every press, drag and click goes through a `Context`, so
//! `viewport_input`, `viewport_frame` and the 2D tab see what they see in the editor. Widgets are
//! found by the text they paint.
#![cfg(test)]

use std::collections::BTreeMap;

use regia_2d::tilemap::{FLIP_X_BIT, TileLayer};

use super::panel::{PanelState, mode2d_panel};
use super::tests::{PANE, framed, ground, map, project};
use super::*;

fn raw(size: egui::Vec2, events: Vec<egui::Event>) -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        events,
        ..egui::RawInput::default()
    }
}

fn button(pos: egui::Pos2, pressed: bool, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers,
    }
}

/// Every text shape of a frame: its string and center, in paint order.
fn texts(output: &egui::FullOutput) -> Vec<(String, egui::Pos2)> {
    fn walk(shape: &egui::Shape, out: &mut Vec<(String, egui::Pos2)>) {
        match shape {
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
            egui::Shape::Text(text) => out.push((
                text.galley.text().to_string(),
                shape.visual_bounding_rect().center(),
            )),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for clipped in &output.shapes {
        walk(&clipped.shape, &mut out);
    }
    out
}

/// The WCAG 2 contrast ratio of two opaque colors.
fn contrast(a: egui::Color32, b: egui::Color32) -> f32 {
    let luminance = |c: egui::Color32| {
        let [r, g, b] = [c.r(), c.g(), c.b()].map(|v| {
            let v = f32::from(v) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        });
        0.2126 * r + 0.7152 * g + 0.0722 * b
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// A front pane at the screen's top-left corner, with room around it to drag out of.
struct Pane {
    ctx: egui::Context,
    world: World,
    view: ViewCamera,
    layout: ViewportLayout,
    modifiers: egui::Modifiers,
    /// Play has the pane: the mode stays on, but the pane's 2D hooks do not run.
    possessed: bool,
    /// The shapes of the last frame.
    shapes: Vec<egui::epaint::ClippedShape>,
}

impl Pane {
    fn new() -> Self {
        let (mut mode, view) = framed(map());
        mode.layer = 1;
        let mut world = World::new();
        world.insert_resource(mode);
        Self {
            ctx: egui::Context::default(),
            world,
            view,
            layout: ViewportLayout::default(),
            modifiers: egui::Modifiers::NONE,
            possessed: false,
            shapes: Vec::new(),
        }
    }

    /// One frame, in the order `viewport.rs` calls the hooks.
    fn frame(&mut self, events: Vec<egui::Event>) {
        self.frame_passes(events, false);
    }

    /// One frame whose first pass asks egui for a discard, so the hooks run twice on the same
    /// input (as a layout change in the frame does). Returns the passes run.
    fn frame_twice(&mut self, events: Vec<egui::Event>) -> usize {
        self.frame_passes(events, true)
    }

    fn frame_passes(&mut self, events: Vec<egui::Event>, discard_first: bool) -> usize {
        let mut passes = 0;
        let Self {
            ctx,
            world,
            view,
            layout,
            possessed,
            shapes,
            ..
        } = self;
        let screen = egui::vec2(PANE.x + 200.0, PANE.y + 200.0);
        let front = layout
            .panes
            .iter()
            .position(|p| p.kind == ViewKind::Front)
            .expect("the default layout has a front pane");
        let mut camera = layout.panes[front].camera.clone();
        let mut output = ctx.run_ui(raw(screen, events), |ui| {
            passes += 1;
            if discard_first && ui.ctx().current_pass_index() == 0 {
                ui.ctx().request_discard("test: a second pass of the frame");
            }
            let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(PANE.x, PANE.y));
            let response = ui.interact(rect, egui::Id::new("pane"), egui::Sense::click_and_drag());
            if is_active(world) && !*possessed {
                paint_overlay(ui, world, view, rect);
                viewport_input(ui, world, &response, view, PANE);
            }
            viewport_frame(ui, world, layout, front, &mut camera, true, response.has_focus());
        });
        output.textures_delta.clear();
        *shapes = std::mem::take(&mut output.shapes);
        passes
    }

    fn mode(&self) -> &Mode2d {
        self.world.resource::<Mode2d>()
    }

    /// The screen position of the center of `cell`.
    fn at(&self, cell: (u32, u32)) -> egui::Pos2 {
        let mode = self.mode();
        let doc = mode.doc.as_ref().expect("map");
        let (min, max) = doc.map.tile_world_rect(cell.0, cell.1);
        let [min, max] = [min, max].map(|p| p.map(|v| v as f32));
        let center = Vec3::new((min[0] + max[0]) * 0.5, (min[1] + max[1]) * 0.5, 0.0);
        let pixel = self.view.project(mode.origin + center, PANE).expect("in view");
        egui::pos2(pixel.x, pixel.y)
    }

    fn hold(&mut self, modifiers: egui::Modifiers) {
        self.modifiers = modifiers;
        self.frame(vec![egui::Event::ModifiersChanged(modifiers)]);
    }

    fn move_to(&mut self, pos: egui::Pos2) {
        self.frame(vec![egui::Event::PointerMoved(pos)]);
    }

    fn press(&mut self, pos: egui::Pos2) {
        self.move_to(pos);
        let modifiers = self.modifiers;
        self.frame(vec![button(pos, true, modifiers)]);
    }

    fn release(&mut self, pos: egui::Pos2) {
        let modifiers = self.modifiers;
        self.frame(vec![button(pos, false, modifiers)]);
    }

    fn drag(&mut self, cells: &[(u32, u32)]) {
        self.press(self.at(cells[0]));
        for cell in &cells[1..] {
            self.move_to(self.at(*cell));
        }
        self.release(self.at(cells[cells.len() - 1]));
    }

    fn undo_depth(&self) -> usize {
        self.world
            .get_resource::<UndoStack>()
            .map_or(0, UndoStack::undo_depth)
    }

    fn row(&self, y: usize) -> Vec<u32> {
        ground(self.mode())[y * 6..(y + 1) * 6].to_vec()
    }
}

#[test]
fn a_click_paints_the_cell_under_the_pointer_as_one_undo_entry() {
    let mut pane = Pane::new();
    let pos = pane.at((2, 1));
    pane.press(pos);
    assert_eq!(pane.mode().hover, Some((2, 1)), "the view and the pointer agree");
    assert_eq!(pane.row(1), [0; 6], "paints on release, as a click");
    pane.release(pos);
    assert_eq!(pane.row(1), [0, 0, 1, 0, 0, 0]);
    assert_eq!(pane.undo_depth(), 1);
    assert!(!pane.mode().stroking());
    assert!(pane.mode().doc.as_ref().is_some_and(|d| d.dirty));
}

/// Review g17 row 5: a release frame run twice (a discard pass) paints one stroke. A random brush
/// bumps its seed when a stroke ends, so a repeated stroke would paint other tiles as a second
/// undo entry; the pane applies its input once per frame whatever the second pass sees.
#[test]
fn a_second_pass_of_a_random_brush_click_paints_once() {
    let mut pane = Pane::new();
    pane.world.resource_mut::<Mode2d>().random = Some(RandomBrush {
        tiles: vec![RandomTile { raw: 1, weight: 1 }],
        nothing: 0.0,
        seed: 11,
    });
    let pos = pane.at((2, 1));
    pane.press(pos);
    let modifiers = pane.modifiers;
    assert_eq!(
        pane.frame_twice(vec![button(pos, false, modifiers)]),
        2,
        "the release frame ran twice"
    );
    assert_eq!(pane.row(1), [0, 0, 1, 0, 0, 0]);
    assert_eq!(pane.undo_depth(), 1, "one stroke, one undo entry");
    assert_eq!(
        pane.mode().random.as_ref().map(|r| r.seed),
        Some(12),
        "the seed moved once"
    );
    assert!(!pane.mode().stroking());
}

/// Review g17 row 5: dragging a guide out of the top ruler with every frame run twice still makes
/// one guide, and it stays where it was dropped.
#[test]
fn a_guide_dragged_out_of_the_ruler_on_doubled_frames_is_one_guide() {
    let mut pane = Pane::new();
    assert!(pane.mode().guides.rulers, "rulers are on by default");
    let start = egui::pos2(PANE.x * 0.5, guides::RULER_PX * 0.5);
    let end = egui::pos2(PANE.x * 0.5, PANE.y * 0.5);
    let modifiers = pane.modifiers;
    pane.frame_twice(vec![egui::Event::PointerMoved(start)]);
    pane.frame_twice(vec![button(start, true, modifiers)]);
    pane.frame_twice(vec![egui::Event::PointerMoved(egui::pos2(
        start.x,
        PANE.y * 0.25,
    ))]);
    pane.frame_twice(vec![egui::Event::PointerMoved(end)]);
    pane.frame_twice(vec![button(end, false, modifiers)]);
    pane.frame(Vec::new());
    let guides = &pane.mode().guides.guides;
    assert_eq!(guides.len(), 1, "one drag-out, one guide: {guides:?}");
    assert_eq!(guides[0].axis, guides::GuideAxis::Horizontal);
    assert!(pane.mode().dragging_guide.is_none(), "the drag ended");
}

#[test]
fn a_drag_fills_the_line_and_undoes_as_one_entry() {
    let mut pane = Pane::new();
    pane.drag(&[(0, 1), (1, 1), (4, 1)]);
    assert_eq!(pane.row(1), [1, 1, 1, 1, 1, 0]);
    assert_eq!(pane.undo_depth(), 1);
    assert!(crate::command::undo_world(&mut pane.world));
    assert!(ground(pane.mode()).iter().all(|&g| g == 0));
}

#[test]
fn a_fast_drag_still_paints_the_cell_the_button_went_down_on() {
    // The first pointer sample after the press is already three cells away, past the threshold
    // where egui starts calling this a drag.
    let mut pane = Pane::new();
    pane.drag(&[(0, 2), (3, 2)]);
    assert_eq!(pane.row(2), [1, 1, 1, 1, 0, 0]);
    assert_eq!(pane.undo_depth(), 1);
}

#[test]
fn shift_erases_and_alt_leaves_the_map_alone() {
    let mut pane = Pane::new();
    pane.drag(&[(0, 1), (5, 1)]);
    pane.hold(egui::Modifiers::SHIFT);
    pane.drag(&[(1, 1), (2, 1)]);
    assert_eq!(pane.row(1), [1, 0, 0, 1, 1, 1]);
    assert_eq!(pane.undo_depth(), 2);

    pane.hold(egui::Modifiers::ALT);
    pane.drag(&[(0, 3), (5, 3)]);
    let pos = pane.at((0, 0));
    pane.press(pos);
    pane.release(pos);
    assert_eq!(pane.row(3), [0; 6], "Alt belongs to the camera");
    assert_eq!(pane.row(0), [0; 6]);
    assert_eq!(pane.undo_depth(), 2);
}

#[test]
fn leaving_the_pane_mid_drag_keeps_the_stroke_and_joins_the_line_on_return() {
    let mut pane = Pane::new();
    pane.press(pane.at((0, 1)));
    pane.move_to(pane.at((1, 1)));
    pane.move_to(egui::pos2(PANE.x + 100.0, 200.0));
    assert!(pane.mode().stroking());
    assert_eq!(pane.mode().hover, None);
    assert_eq!(pane.row(1), [1, 1, 0, 0, 0, 0], "nothing paints off the pane");
    pane.move_to(pane.at((4, 1)));
    pane.release(pane.at((4, 1)));
    assert_eq!(pane.row(1), [1, 1, 1, 1, 1, 0]);
    assert_eq!(pane.undo_depth(), 1);
}

#[test]
fn a_release_outside_the_pane_ends_the_stroke() {
    let mut pane = Pane::new();
    pane.press(pane.at((0, 1)));
    pane.move_to(pane.at((1, 1)));
    let outside = egui::pos2(PANE.x + 100.0, 200.0);
    pane.move_to(outside);
    pane.release(outside);
    assert!(!pane.mode().stroking());
    assert_eq!(pane.undo_depth(), 1);
}

#[test]
fn leaving_the_mode_mid_drag_keeps_the_stroke_s_undo_entry() {
    let mut pane = Pane::new();
    pane.press(pane.at((0, 1)));
    pane.move_to(pane.at((2, 1)));
    assert_eq!(pane.row(1), [1, 1, 1, 0, 0, 0]);
    pane.frame(vec![egui::Event::Key {
        key: egui::Key::B,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }]);
    assert!(!pane.mode().active);
    assert!(!pane.mode().stroking(), "the stroke ended with the mode");
    assert_eq!(pane.undo_depth(), 1, "the painted cells stay undoable");
    assert!(crate::command::undo_world(&mut pane.world));
    assert_eq!(pane.row(1), [0; 6]);
}

#[test]
fn a_stroke_the_pane_stopped_seeing_ends_when_the_button_is_up() {
    // Play takes the pane mid-drag: `viewport_input` stops running while the button comes up.
    let mut pane = Pane::new();
    pane.press(pane.at((0, 1)));
    pane.move_to(pane.at((2, 1)));
    pane.possessed = true;
    pane.release(pane.at((2, 1)));
    assert!(pane.mode().active);
    assert!(!pane.mode().stroking());
    assert_eq!(pane.undo_depth(), 1);
}

#[test]
fn the_status_line_sits_on_its_dark_backing() {
    let mut pane = Pane::new();
    pane.frame(Vec::new());
    let (text_at, text_index) = pane
        .shapes
        .iter()
        .enumerate()
        .find_map(|(index, clipped)| match &clipped.shape {
            egui::Shape::Text(text) if text.galley.text().starts_with("2D ·") => {
                Some((clipped.shape.visual_bounding_rect(), index))
            }
            _ => None,
        })
        .expect("the status line");
    let backing = pane.shapes[..text_index]
        .iter()
        .rev()
        .find_map(|clipped| match &clipped.shape {
            egui::Shape::Rect(rect) => Some(rect),
            _ => None,
        })
        .expect("a filled rectangle under the text");
    assert!(
        backing.rect.contains_rect(text_at),
        "{:?} vs {text_at:?}",
        backing.rect
    );
    assert_eq!(
        backing.fill.a(),
        255,
        "opaque: nothing behind it changes the contrast"
    );
    let contrast = contrast(
        backing.fill,
        crate::theme::tokens(&pane.world).overlay.selection_outline,
    );
    assert!(contrast >= 4.5, "status line contrast {contrast:.2}");
}

/// The 2D tab alone on a screen, clicked by the text of its widgets.
struct Tab {
    ctx: egui::Context,
    world: World,
    size: egui::Vec2,
    last: Vec<(String, egui::Pos2)>,
}

impl Tab {
    fn with(map: TileMapAsset, size: egui::Vec2) -> Self {
        let (mode, _) = framed(map);
        let mut world = World::new();
        world.insert_resource(mode);
        world.resource_mut::<Mode2d>().layer = 1;
        let mut tab = Self {
            ctx: egui::Context::default(),
            world,
            size,
            last: Vec::new(),
        };
        tab.frame(Vec::new());
        tab
    }

    fn new() -> Self {
        Self::with(map(), egui::vec2(400.0, 800.0))
    }

    fn frame(&mut self, events: Vec<egui::Event>) {
        let Self {
            ctx,
            world,
            size,
            last,
        } = self;
        let mut output = ctx.run_ui(raw(*size, events), |ui| {
            mode2d_panel(ui, world);
        });
        output.textures_delta.clear();
        *last = texts(&output);
    }

    fn find(&self, label: &str) -> Option<egui::Pos2> {
        self.last
            .iter()
            .find(|(text, _)| text == label)
            .map(|(_, pos)| *pos)
    }

    fn click(&mut self, label: &str) {
        let pos = self
            .find(label)
            .unwrap_or_else(|| panic!("no {label:?} in {:?}", self.last));
        self.frame(vec![egui::Event::PointerMoved(pos)]);
        self.frame(vec![button(pos, true, egui::Modifiers::NONE)]);
        self.frame(vec![button(pos, false, egui::Modifiers::NONE)]);
        self.frame(Vec::new());
    }

    fn mode(&self) -> &Mode2d {
        self.world.resource::<Mode2d>()
    }

    fn error(&self) -> Option<String> {
        self.world.resource::<PanelState>().error.clone()
    }
}

#[test]
fn the_palette_picks_a_tile_and_the_flips_stay_on_the_next_pick() {
    let mut tab = Tab::new();
    tab.click("5");
    assert_eq!(tab.mode().brush, Brush2d::Tile(6), "local 5 is gid 6");
    tab.click("Flip X");
    assert_eq!(tab.mode().brush, Brush2d::Tile(6 | FLIP_X_BIT));
    tab.click("7");
    assert_eq!(tab.mode().brush, Brush2d::Tile(8 | FLIP_X_BIT));
    tab.click("Flip X");
    assert_eq!(tab.mode().brush, Brush2d::Tile(8));
}

#[test]
fn the_tab_switches_erase_grid_and_the_mode() {
    let mut tab = Tab::new();
    tab.click("Erase");
    assert_eq!(tab.mode().brush, Brush2d::Erase);
    assert!(tab.mode().grid);
    tab.click("Grid");
    assert!(!tab.mode().grid);
    assert!(tab.mode().active);
    tab.click("2D mode");
    assert!(!tab.mode().active);
    tab.click("2D mode");
    assert!(tab.mode().active);
    assert!(tab.mode().pending_frame, "turning it on frames the map");
}

#[test]
fn the_tab_picks_a_layer_and_lists_only_tile_layers() {
    let mut map = map();
    map.layers.push(MapLayer::Tiles(TileLayer {
        name: "top".into(),
        visible: true,
        opacity: 1.0,
        offset: [0.0, 0.0],
        data: vec![0; 24],
        properties: BTreeMap::new(),
    }));
    let mut tab = Tab::with(map, egui::vec2(400.0, 800.0));
    tab.click("top");
    assert_eq!(tab.mode().layer, 2);
    tab.click("ground");
    assert_eq!(tab.mode().layer, 1);
    // The object layer is listed now (ADR-7853), but it takes no tiles: picking it makes it the
    // edited object layer and the paint layer stays.
    tab.click("spawns");
    assert_eq!(tab.mode().layer, 1, "an object layer takes no tiles");
    assert_eq!(tab.mode().object_layer, Some(0));
    tab.click("top");
    assert_eq!((tab.mode().layer, tab.mode().object_layer), (2, None));
}

#[test]
fn save_map_writes_the_edits_and_the_question_s_buttons_work() {
    let dir = project();
    let path = dir.join("level.tilemap.ron");
    let other = dir.join("other.tilemap.ron");
    std::fs::copy(&path, &other).expect("second map");
    let mut tab = Tab::new();
    open_map(&mut tab.world, &path, true).expect("opens");
    tab.frame(Vec::new());

    let paint = |tab: &mut Tab| {
        let mut mode = tab.world.resource_mut::<Mode2d>();
        mode.brush = Brush2d::Tile(3);
        mode.paint_now(&[(0, 0)], false).expect("paints");
        tab.frame(Vec::new());
    };
    paint(&mut tab);
    tab.click("Save map");
    assert_eq!(tab.error(), None);
    assert!(tab.mode().doc.as_ref().is_some_and(|d| !d.dirty));
    let saved = std::fs::read_to_string(&path).expect("read");
    let saved = TileMapAsset::from_ron(&saved).expect("parses");
    assert_eq!(saved.tile_layer(1).expect("ground").data[0], 3);

    // The question over unsaved edits: Cancel keeps the map, Discard replaces it.
    paint(&mut tab);
    tab.world.resource_mut::<PanelState>().pending_open = Some(other.clone());
    tab.frame(Vec::new());
    assert!(tab.find("The loaded map has unsaved edits.").is_some());
    tab.click("Cancel");
    assert!(tab.find("The loaded map has unsaved edits.").is_none());
    assert_eq!(
        tab.mode().doc.as_ref().map(|d| d.path.clone()),
        Some(path.clone())
    );

    tab.world.resource_mut::<PanelState>().pending_open = Some(other.clone());
    tab.frame(Vec::new());
    tab.click("Discard and open");
    assert_eq!(tab.mode().doc.as_ref().map(|d| d.path.clone()), Some(other));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_long_palette_scrolls_to_its_last_row_without_drifting() {
    let mut map = map();
    map.tilesets[0].tile_count = 4000;
    let mut tab = Tab::with(map, egui::vec2(400.0, 500.0));
    let inside = tab.find("0").expect("the first tile");
    tab.frame(vec![egui::Event::PointerMoved(inside)]);
    tab.frame(vec![egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, -1_000_000.0),
        phase: egui::TouchPhase::Move,
        modifiers: egui::Modifiers::NONE,
    }]);
    for _ in 0..240 {
        tab.frame(Vec::new());
    }
    let last = tab.find("3999").expect("the last tile is on screen");
    // The row of the last tile ends at the screen's bottom edge: a row height that disagrees
    // with the rows drawn would leave it one pixel short per row on screen.
    assert!(
        (last.y + 16.0 - 500.0).abs() <= 1.0,
        "last tile centered at {}",
        last.y
    );
    tab.click("3999");
    assert_eq!(tab.mode().brush, Brush2d::Tile(4000));
}

/// The pane clock steps once per egui frame, however often the overlay is painted in it (a
/// discard re-run, or two Front panes): it used to advance once per call.
#[test]
fn the_pane_clock_ticks_once_per_frame() {
    // Two panes, each in its own context: one painted twice per frame, one once.
    let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(PANE.x, PANE.y));
    let run = |paints: usize| {
        let (mut mode, view) = framed(map());
        mode.clock.playing = true;
        let mut world = World::new();
        world.insert_resource(mode);
        let ctx = egui::Context::default();
        let mut times = Vec::new();
        for _ in 0..3 {
            let mut output = ctx.run_ui(raw(egui::vec2(PANE.x, PANE.y), Vec::new()), |ui| {
                for _ in 0..paints {
                    paint_overlay(ui, &mut world, &view, rect);
                }
            });
            output.textures_delta.clear();
            times.push(world.resource::<Mode2d>().clock.time_ms());
        }
        times
    };
    let once = run(1);
    assert!(once[0] > 0, "the clock plays: {once:?}");
    assert_eq!(run(2), once, "one step per frame, not per paint");
}

fn key(key: egui::Key) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }
}

/// Spec 86 § 6: with the mode on and the viewport focused, `,` / `.` step the tile layer and
/// `[` / `]` the terrain, wrapping; with the mode off the keys change nothing.
#[test]
fn comma_period_and_brackets_step_the_layer_and_the_terrain() {
    let mut pane = Pane::new();
    let dir = project();
    {
        let mut mode = pane.world.resource_mut::<Mode2d>();
        let doc = mode.doc.as_mut().expect("map");
        doc.map.layers.push(MapLayer::Tiles(TileLayer {
            name: "detail".into(),
            visible: true,
            opacity: 1.0,
            offset: [0.0, 0.0],
            data: vec![0; 24],
            properties: BTreeMap::new(),
        }));
        mode.load_rules(&dir.join("walls.autotile.ron")).expect("rules");
        mode.brush = Brush2d::Terrain("wall".into());
    }
    assert_eq!(pane.mode().layer, 1);
    pane.frame(vec![key(egui::Key::Period)]);
    assert_eq!(pane.mode().layer, 2, "`.` is the next tile layer");
    pane.frame(vec![key(egui::Key::Period)]);
    assert_eq!(
        pane.mode().layer,
        1,
        "wraps past the last tile layer, skipping the object layer"
    );
    pane.frame(vec![key(egui::Key::Comma)]);
    assert_eq!(pane.mode().layer, 2, "`,` wraps back to the last tile layer");
    pane.frame(vec![key(egui::Key::Comma)]);
    assert_eq!(pane.mode().layer, 1);
    pane.frame(vec![key(egui::Key::CloseBracket)]);
    assert_eq!(
        pane.mode().brush,
        Brush2d::Terrain("water".into()),
        "`]` is the next terrain"
    );
    pane.frame(vec![key(egui::Key::CloseBracket)]);
    assert_eq!(pane.mode().brush, Brush2d::Terrain("wall".into()), "and wraps");
    pane.frame(vec![key(egui::Key::OpenBracket)]);
    assert_eq!(
        pane.mode().brush,
        Brush2d::Terrain("water".into()),
        "`[` wraps back"
    );
    // Mode off: the keys belong to the rest of the editor.
    pane.world.resource_mut::<Mode2d>().active = false;
    pane.frame(vec![key(egui::Key::Period), key(egui::Key::CloseBracket)]);
    assert_eq!(pane.mode().layer, 1);
    assert_eq!(pane.mode().brush, Brush2d::Terrain("water".into()));
    let _ = std::fs::remove_dir_all(dir);
}

/// Spec 86 § 6: "Save and open" on the unsaved-edits question writes the edited map first, then
/// opens the other one.
#[test]
fn save_and_open_writes_the_edits_then_opens_the_other_map() {
    let dir = project();
    let path = dir.join("level.tilemap.ron");
    let other = dir.join("other.tilemap.ron");
    std::fs::copy(&path, &other).expect("second map");
    let mut tab = Tab::new();
    open_map(&mut tab.world, &path, true).expect("opens");
    {
        let mut mode = tab.world.resource_mut::<Mode2d>();
        mode.brush = Brush2d::Tile(5);
        mode.paint_now(&[(2, 1)], false).expect("paints");
    }
    tab.frame(Vec::new());
    assert!(tab.mode().doc.as_ref().is_some_and(|d| d.dirty));
    tab.world.resource_mut::<PanelState>().pending_open = Some(other.clone());
    tab.frame(Vec::new());
    assert!(tab.find("The loaded map has unsaved edits.").is_some());
    tab.click("Save and open");
    assert_eq!(tab.error(), None);
    assert_eq!(
        tab.mode().doc.as_ref().map(|d| (d.path.clone(), d.dirty)),
        Some((other.clone(), false))
    );
    let saved = std::fs::read_to_string(&path).expect("read");
    let saved = TileMapAsset::from_ron(&saved).expect("parses");
    assert_eq!(
        saved.tile_layer(1).expect("ground").data[6 + 2],
        5,
        "the edit was saved"
    );
    let untouched = std::fs::read_to_string(&other).expect("read");
    let untouched = TileMapAsset::from_ron(&untouched).expect("parses");
    assert_eq!(untouched.tile_layer(1).expect("ground").data[6 + 2], 0);
    let _ = std::fs::remove_dir_all(dir);
}
