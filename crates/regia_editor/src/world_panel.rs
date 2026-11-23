// SPDX-License-Identifier: MPL-2.0
//! The World Map panel (`Panel::WorldMap`, P-241, ADR-7802, `book/agents/world-streaming-editor.md`):
//! a top-down map of a streamed level's cells, and the buttons that load and unload them in the
//! edit world. The model is [`crate::world_edit::WorldEdit`]; every button has an agent method
//! (`editor.world_open`, `editor.world_load`, `editor.world_unload`, `editor.cancel_job`).
//!
//! The map draws the manifest's cells inside the world bounds (+X right, +Z down), shaded by each
//! cell's recorded content height (light = high), loaded cells filled with the accent, cells the
//! runtime would refuse outlined in the error color with a cross, selected cells outlined in the
//! strong text color, and the editor camera as a dot with its view cone. Click a cell to select it
//! (Ctrl or Shift adds), drag a box to select every cell it touches.
//!
//! Keyboard: the cell list beside the map has one focusable row per cell (`canvas_keys`): Up and
//! Down move between rows, Enter or Space selects or deselects the row's cell; every button is a
//! focusable widget reached with Tab. Colors come from the theme tokens; sizes are in points, so
//! the map scales with `appearance.ui_scale`.

use std::collections::BTreeSet;

use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};
use regia_scene::stream_grid::CellCoord;

use crate::agent_parity::{
    self, WORLD_LOAD_AROUND, WORLD_LOAD_CANCEL, WORLD_LOAD_SELECTED, WORLD_OPEN, WORLD_SELECT_NONE,
    WORLD_UNLOAD_DISCARD, WORLD_UNLOAD_KEEP, WORLD_UNLOAD_SAVE, WORLD_UNLOAD_SELECTED,
};
use crate::canvas_keys::{self, ListKeys, RowKey};
use crate::engine::ecs::prelude::*;
use crate::theme::ThemeTokens;
use crate::world_edit::{self, CellStatus, OpenWorld, WorldEdit};

/// Default radius of Load around camera, meters.
pub const DEFAULT_RADIUS_M: f64 = 256.0;

/// The panel's view state (not undone, not saved): the selection, the radius, the path field and
/// the unsaved-edits prompt.
#[derive(Resource, Debug, Clone)]
pub struct WorldMapUi {
    /// Selected cells.
    pub selected: BTreeSet<CellCoord>,
    /// Load around camera radius, meters.
    pub radius_m: f64,
    /// The path field (the streamed level's `.level.d`).
    pub path: String,
    /// Cells waiting for Save / Discard / Keep loaded.
    pub prompt: Option<Vec<CellCoord>>,
    /// The last refusal, shown under the buttons.
    pub error: Option<String>,
    /// The cell list's keyboard state.
    pub list: ListKeys,
    /// Where a box drag started, in screen points.
    drag_from: Option<Pos2>,
}

impl Default for WorldMapUi {
    fn default() -> Self {
        Self {
            selected: BTreeSet::new(),
            radius_m: DEFAULT_RADIUS_M,
            path: String::new(),
            prompt: None,
            error: None,
            list: ListKeys::default(),
            drag_from: None,
        }
    }
}

/// The accessible name of one cell row.
#[must_use]
pub fn cell_row_name(open: &OpenWorld, cell: CellCoord, selected: bool) -> String {
    let Some(info) = open.cells.get(&cell) else {
        return format!("Cell {cell}: not in the manifest");
    };
    let mut parts = Vec::new();
    match info.status {
        CellStatus::Ready if open.loaded.contains_key(&cell) => parts.push("loaded".to_string()),
        CellStatus::Ready => parts.push("ready".to_string()),
        CellStatus::Error(reason) => parts.push(format!("error {reason}")),
    }
    if selected {
        parts.push("selected".to_string());
    }
    let plural = if info.entity_count == 1 {
        "entity"
    } else {
        "entities"
    };
    parts.push(format!("{} {plural}", info.entity_count));
    format!("Cell {cell}: {}", parts.join(", "))
}

/// How the map paints one cell: the fill (or `None`) and the outline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellLook {
    /// The fill.
    pub fill: Color32,
    /// The outline.
    pub stroke: Stroke,
    /// Whether a cross marks it (a cell the runtime would refuse).
    pub cross: bool,
}

/// The look of `cell` from the theme tokens: height shading (`height` in 0..=1), the accent when
/// loaded, the error color for a refused cell, the strong text color when selected.
#[must_use]
pub fn cell_look(
    tokens: &ThemeTokens,
    status: &CellStatus,
    loaded: bool,
    selected: bool,
    height: f32,
) -> CellLook {
    let shaded = tokens
        .well
        .lerp_to_gamma(tokens.muted, 0.15 + 0.65 * height.clamp(0.0, 1.0));
    let fill = if loaded {
        tokens.signal.lerp_to_gamma(shaded, 0.35)
    } else {
        shaded
    };
    let error = matches!(status, CellStatus::Error(_));
    let stroke = if selected {
        Stroke::new(2.0, tokens.strong)
    } else if error {
        Stroke::new(1.5, tokens.error)
    } else {
        Stroke::new(1.0, tokens.line)
    };
    CellLook {
        fill,
        stroke,
        cross: error,
    }
}

/// Maps cells to screen rectangles inside `rect`.
#[derive(Debug, Clone, Copy)]
struct MapFrame {
    origin: Pos2,
    min: CellCoord,
    px: f32,
}

impl MapFrame {
    fn new(rect: Rect, bounds: [CellCoord; 2]) -> Self {
        let cols = (bounds[1].x - bounds[0].x + 1).max(1) as f32;
        let rows = (bounds[1].z - bounds[0].z + 1).max(1) as f32;
        let px = (rect.width() / cols).min(rect.height() / rows).clamp(1.0, 64.0);
        Self {
            origin: rect.min,
            min: bounds[0],
            px,
        }
    }

    fn cell_rect(&self, cell: CellCoord) -> Rect {
        let x = (cell.x - self.min.x) as f32 * self.px;
        let y = (cell.z - self.min.z) as f32 * self.px;
        Rect::from_min_size(self.origin + Vec2::new(x, y), Vec2::splat(self.px))
    }

    /// The screen point of the world ground point `(x, z)` with cells of `size` meters.
    fn point(&self, x: f64, z: f64, size: f64) -> Pos2 {
        let cx = (x / size - self.min.x as f64) as f32 * self.px;
        let cz = (z / size - self.min.z as f64) as f32 * self.px;
        self.origin + Vec2::new(cx, cz)
    }

    fn cell_at(&self, pos: Pos2) -> CellCoord {
        let local = pos - self.origin;
        CellCoord::new(
            self.min.x + (local.x / self.px).floor() as i64,
            self.min.z + (local.y / self.px).floor() as i64,
        )
    }
}

/// Draws the World Map panel.
pub fn world_map_panel(ui: &mut egui::Ui, world: &mut World) {
    let _span = tracing::trace_span!("world_map_panel").entered();
    world_edit::tick(world);
    let mut state = world.remove_resource::<WorldMapUi>().unwrap_or_default();
    if state.path.is_empty()
        && let Some(path) = world
            .get_resource::<crate::level_io::OpenLevel>()
            .and_then(|open| open.path.clone())
            .filter(|p| crate::level_io::is_level_dir(p))
    {
        state.path = path.display().to_string();
    }
    draw(ui, world, &mut state);
    world.insert_resource(state);
}

fn draw(ui: &mut egui::Ui, world: &mut World, state: &mut WorldMapUi) {
    let tokens = crate::theme::tokens(world).clone();
    // Wraps in a narrow dock instead of cutting Open world off (bug 19, ADR-8586).
    ui.horizontal_wrapped(|ui| {
        let label = ui.label("Streamed level");
        ui.add(
            egui::TextEdit::singleline(&mut state.path)
                .hint_text("world.level.d")
                .desired_width(260.0),
        )
        .labelled_by(label.id)
        .on_hover_text(
            "The streamed level in its directory form (.level.d); its cooked index is the manifest",
        );
        if agent_parity::action_enabled(ui, !state.path.trim().is_empty(), &WORLD_OPEN, None).clicked() {
            // The agent's rule: a path outside the project is refused (review sprint-w12-rev-g7 #11).
            state.error = world_edit::world_path(world, state.path.trim())
                .and_then(|path| world_edit::open_path(world, &path, None))
                .err()
                .map(|e| e.to_string());
            state.selected.clear();
            state.prompt = None;
        }
    });
    let opened = world
        .get_resource::<WorldEdit>()
        .and_then(|edit| edit.world.as_ref())
        .is_some();
    if !opened {
        ui.label(
            egui::RichText::new("No world is open. Cook the project, then open a streamed level.")
                .color(tokens.muted),
        );
        show_error(ui, &tokens, state);
        return;
    }
    toolbar(ui, world, state, &tokens);
    show_error(ui, &tokens, state);
    prompt(ui, world, state, &tokens);
    ui.separator();
    ui.horizontal_top(|ui| {
        let list_width = (ui.available_width() * 0.35).clamp(160.0, 320.0);
        ui.vertical(|ui| {
            ui.set_width(list_width);
            cell_list(ui, world, state);
        });
        map(ui, world, state, &tokens);
    });
}

fn show_error(ui: &mut egui::Ui, tokens: &ThemeTokens, state: &WorldMapUi) {
    if let Some(error) = &state.error {
        ui.label(egui::RichText::new(error).color(tokens.error));
    }
}

fn toolbar(ui: &mut egui::Ui, world: &mut World, state: &mut WorldMapUi, tokens: &ThemeTokens) {
    let (job, last, summary) = {
        let edit = world.resource::<WorldEdit>();
        let summary = edit.world.as_ref().map_or_else(String::new, |open| {
            format!(
                "{} cells, {} loaded, {} m cells",
                open.cells.len(),
                open.loaded.len(),
                open.grid.cell_size()
            )
        });
        (edit.job_id().map(str::to_string), edit.last.clone(), summary)
    };
    ui.label(summary);
    let selected: Vec<CellCoord> = state.selected.iter().copied().collect();
    let idle = job.is_none();
    ui.horizontal_wrapped(|ui| {
        let can = idle && !selected.is_empty();
        if agent_parity::action_enabled(ui, can, &WORLD_LOAD_SELECTED, None).clicked() {
            state.error = world_edit::start_load(world, &selected)
                .err()
                .map(|e| e.to_string());
        }
        // Once per egui frame: the discarded second pass would unload again and show
        // `world_cell_not_loaded` after the unload worked (review sprint-w12-rev-g7 #10).
        if agent_parity::action_enabled(ui, can, &WORLD_UNLOAD_SELECTED, None).clicked()
            && canvas_keys::first_in_this_frame(ui, egui::Id::new("world-map-unload"))
        {
            unload_selected(world, state, &selected);
        }
        if agent_parity::action_enabled(ui, !selected.is_empty(), &WORLD_SELECT_NONE, None).clicked() {
            state.selected.clear();
        }
    });
    ui.horizontal(|ui| {
        let label = ui.label("Radius");
        ui.add(
            egui::DragValue::new(&mut state.radius_m)
                .range(0.0..=100_000.0)
                .speed(8.0)
                .suffix(" m"),
        )
        .labelled_by(label.id)
        .on_hover_text("Load around camera radius");
        if agent_parity::action_enabled(ui, idle, &WORLD_LOAD_AROUND, None).clicked() {
            state.error = load_around(world, state.radius_m).err();
        }
    });
    if let Some(id) = job {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(format!("Loading ({id})"));
            if agent_parity::action_button(ui, &WORLD_LOAD_CANCEL).clicked() {
                world.resource::<WorldEdit>().cancel_load();
            }
        });
    } else if let Some(last) = last {
        ui.label(egui::RichText::new(last).color(tokens.muted));
    }
}

fn load_around(world: &mut World, radius: f64) -> Result<String, String> {
    let [x, z] = world_edit::camera_ground(world).ok_or("The editor has no camera to load around")?;
    let cells = world
        .resource::<WorldEdit>()
        .world
        .as_ref()
        .ok_or("No world is open")?
        .cells_around(x, z, radius)
        .map_err(|e| e.to_string())?;
    if cells.is_empty() {
        return Err("No cell of the world is within that radius of the camera".into());
    }
    world_edit::start_load(world, &cells).map_err(|e| e.to_string())
}

/// Toggles each of `cells` in `selected`, once per egui frame for the control `key`: a Ctrl or
/// Shift click (or a row's Activate) seen again by egui's discarded second pass must not toggle the
/// cell back (review sprint-w12-rev-g7 #10). Returns whether it toggled.
fn toggle_once(
    ctx: &egui::Context,
    key: &str,
    frame: u64,
    selected: &mut BTreeSet<CellCoord>,
    cells: impl IntoIterator<Item = CellCoord>,
) -> bool {
    if !canvas_keys::first_in_frame(ctx, egui::Id::new(key), frame) {
        return false;
    }
    for cell in cells {
        if !selected.remove(&cell) {
            selected.insert(cell);
        }
    }
    true
}

fn unload_selected(world: &mut World, state: &mut WorldMapUi, selected: &[CellCoord]) {
    let loaded: Vec<CellCoord> = {
        let edit = world.resource::<WorldEdit>();
        let Some(open) = edit.world.as_ref() else {
            return;
        };
        selected
            .iter()
            .copied()
            .filter(|c| open.loaded.contains_key(c))
            .collect()
    };
    if loaded.is_empty() {
        state.error = Some("None of the selected cells is loaded".into());
        return;
    }
    match world_edit::unload(world, &loaded, None) {
        Ok(_) => state.error = None,
        Err(e) if e.code == "world_unsaved" => state.prompt = Some(loaded),
        Err(e) => state.error = Some(e.to_string()),
    }
}

fn prompt(ui: &mut egui::Ui, world: &mut World, state: &mut WorldMapUi, tokens: &ThemeTokens) {
    let Some(cells) = state.prompt.clone() else {
        return;
    };
    egui::Frame::group(ui.style()).show(ui, |ui| {
        let names: Vec<String> = cells.iter().map(CellCoord::label).collect();
        ui.label(
            egui::RichText::new(format!(
                "Unsaved edits in {}. Save them to the cell files before unloading?",
                names.join(", ")
            ))
            .color(tokens.warn),
        );
        ui.horizontal(|ui| {
            let mut choice = None;
            if agent_parity::action_button(ui, &WORLD_UNLOAD_SAVE).clicked() {
                choice = Some(Some(true));
            }
            if agent_parity::action_button(ui, &WORLD_UNLOAD_DISCARD).clicked() {
                choice = Some(Some(false));
            }
            if agent_parity::action_button(ui, &WORLD_UNLOAD_KEEP).clicked() {
                choice = Some(None);
            }
            match choice {
                Some(Some(save)) => {
                    state.error = world_edit::unload(world, &cells, Some(save))
                        .err()
                        .map(|e| e.to_string());
                    state.prompt = None;
                }
                Some(None) => state.prompt = None,
                None => {}
            }
        });
    });
}

fn cell_list(ui: &mut egui::Ui, world: &mut World, state: &mut WorldMapUi) {
    let (cells, names) = {
        let edit = world.resource::<WorldEdit>();
        let Some(open) = edit.world.as_ref() else {
            return;
        };
        let cells: Vec<CellCoord> = open.cells.keys().copied().collect();
        let names: Vec<String> = cells
            .iter()
            .map(|c| cell_row_name(open, *c, state.selected.contains(c)))
            .collect();
        (cells, names)
    };
    let current = state.list.current;
    let events = canvas_keys::list_rows(
        ui,
        "world_map_cells",
        &mut state.list,
        cells.len(),
        canvas_keys::LIST_HEIGHT,
        current,
        &|i| names[i].clone(),
    );
    let frame = ui.ctx().cumulative_frame_nr();
    for event in events {
        if let RowKey::Activate(i) = event
            && let Some(cell) = cells.get(i)
        {
            toggle_once(ui.ctx(), "world-map-row", frame, &mut state.selected, [*cell]);
        }
    }
}

fn map(ui: &mut egui::Ui, world: &mut World, state: &mut WorldMapUi, tokens: &ThemeTokens) {
    let camera = world.get_resource::<crate::viewport::EditorCamera>().cloned();
    let edit = world.resource::<WorldEdit>();
    let Some(open) = edit.world.as_ref() else {
        return;
    };
    let side = ui
        .available_width()
        .min(ui.available_height().max(160.0))
        .max(120.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(side), Sense::click_and_drag());
    let frame = MapFrame::new(rect, open.bounds);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, tokens.panel);
    let (lo, hi) = open
        .cells
        .values()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), c| {
            (lo.min(c.max_y), hi.max(c.max_y))
        });
    let span = (hi - lo).max(f64::EPSILON);
    for (coord, cell) in &open.cells {
        let height = if hi > lo {
            ((cell.max_y - lo) / span) as f32
        } else {
            0.5
        };
        let look = cell_look(
            tokens,
            &cell.status,
            open.loaded.contains_key(coord),
            state.selected.contains(coord),
            height,
        );
        let r = frame.cell_rect(*coord).shrink(0.5);
        painter.rect_filled(r, 0.0, look.fill);
        painter.rect_stroke(r, 0.0, look.stroke, egui::StrokeKind::Inside);
        if look.cross {
            painter.line_segment([r.left_top(), r.right_bottom()], look.stroke);
            painter.line_segment([r.right_top(), r.left_bottom()], look.stroke);
        }
    }
    if let Some(camera) = camera {
        let size = open.grid.cell_size();
        let eye = camera.eye();
        let at = frame.point(f64::from(eye.x), f64::from(eye.z), size);
        let color = tokens.overlay.icon_camera;
        let (sin, cos) = camera.yaw.sin_cos();
        let forward = Vec2::new(-sin, -cos);
        let half = (camera.fov_deg.to_radians() * 0.5).clamp(0.05, 1.5);
        let reach = frame.px * 2.0;
        for side in [-half, half] {
            let dir = egui::emath::Rot2::from_angle(side) * forward;
            painter.line_segment([at, at + dir * reach], Stroke::new(1.5, color));
        }
        painter.circle_filled(at, 4.0, color);
    }
    let loaded = open.loaded.len();
    let total = open.cells.len();
    canvas_keys::name_canvas(
        ui.ctx(),
        response.id,
        &format!("World map, {total} cells, {loaded} loaded"),
    );
    let add = ui.input(|i| i.modifiers.command || i.modifiers.shift);
    if response.drag_started() {
        state.drag_from = response.interact_pointer_pos();
    }
    if let (Some(from), Some(to)) = (state.drag_from, response.interact_pointer_pos())
        && response.dragged()
    {
        painter.rect_stroke(
            Rect::from_two_pos(from, to),
            0.0,
            Stroke::new(1.0, tokens.signal),
            egui::StrokeKind::Inside,
        );
    }
    let mut picked: Option<BTreeSet<CellCoord>> = None;
    if response.drag_stopped()
        && let (Some(from), Some(to)) = (state.drag_from.take(), response.interact_pointer_pos())
    {
        let (a, b) = (frame.cell_at(from), frame.cell_at(to));
        picked = Some(
            open.cells
                .keys()
                .copied()
                .filter(|c| {
                    (a.x.min(b.x)..=a.x.max(b.x)).contains(&c.x)
                        && (a.z.min(b.z)..=a.z.max(b.z)).contains(&c.z)
                })
                .collect(),
        );
    } else if response.clicked()
        && let Some(pos) = response.interact_pointer_pos()
    {
        let cell = frame.cell_at(pos);
        picked = Some(
            open.cells
                .contains_key(&cell)
                .then_some(cell)
                .into_iter()
                .collect(),
        );
    }
    if let Some(picked) = picked {
        if add {
            let frame = ui.ctx().cumulative_frame_nr();
            toggle_once(ui.ctx(), "world-map-pick", frame, &mut state.selected, picked);
        } else {
            state.selected = picked;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas_keys::test_input::{frame, key, tab_to};
    use crate::world_edit::tests::{editor_app, fixture};

    /// Review sprint-w12-rev-g7 #10: a Ctrl click toggles a cell once per frame; the discarded
    /// second pass leaves it selected.
    #[test]
    fn a_toggle_click_applies_once_per_egui_frame() {
        let ctx = egui::Context::default();
        let mut selected = BTreeSet::new();
        let cell = CellCoord::new(1, 2);
        assert!(toggle_once(&ctx, "pick", 9, &mut selected, [cell]));
        assert!(
            !toggle_once(&ctx, "pick", 9, &mut selected, [cell]),
            "the second pass"
        );
        assert!(selected.contains(&cell), "the cell stays selected");
        assert!(toggle_once(&ctx, "pick", 10, &mut selected, [cell]));
        assert!(selected.is_empty(), "the next frame's click deselects it");
    }

    /// Review sprint-w12-rev-g7 #11: the World Map and `editor.world_open` refuse a level outside
    /// the project.
    #[test]
    fn a_streamed_level_outside_the_project_is_refused() {
        let root = regia_test_utils::unique_temp_dir("world-path-outside");
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let err = world_edit::world_path(&world, "../outside.level.d").expect_err("outside");
        assert_eq!(err.code, "path_outside_project");
        let inside = world_edit::world_path(&world, "levels/world.level.d").expect("inside");
        assert!(inside.ends_with("levels/world.level.d"), "{}", inside.display());
        let _ = std::fs::remove_dir_all(&root);
    }

    const SIZE: [f32; 2] = [900.0, 700.0];

    /// The keyboard path: Tab to a cell row, Enter selects it, Tab to Load selected, Enter loads it
    /// (a job; the editor applies it), and the map's accessible name counts the loaded cell.
    #[test]
    fn a_cell_loads_from_the_keyboard_alone() {
        let fx = fixture("world-panel-keys");
        let mut app = editor_app();
        let world = app.world_mut();
        world_edit::open_path(world, &fx.dir, Some(&fx.cache)).expect("open");
        let ctx = egui::Context::default();
        let mut draw = |ui: &mut egui::Ui| world_map_panel(ui, world);
        tab_to(&ctx, SIZE, "Cell 0_1: ready, 1 entity", false, 60, &mut draw);
        frame(
            &ctx,
            SIZE,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
        frame(&ctx, SIZE, vec![], &mut draw);
        tab_to(&ctx, SIZE, "Load selected", true, 60, &mut draw);
        frame(
            &ctx,
            SIZE,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
        drop(draw);
        world_edit::wait_load(world);
        let mut draw = |ui: &mut egui::Ui| world_map_panel(ui, world);
        let update = frame(&ctx, SIZE, vec![], &mut draw);
        let canvases = canvas_keys::test_input::labels_of(&update, egui::accesskit::Role::Canvas);
        assert!(
            canvases.iter().any(|l| l == "World map, 9 cells, 1 loaded"),
            "{canvases:?}"
        );
        drop(draw);
        let edit = world.resource::<WorldEdit>();
        let open = edit.world.as_ref().expect("open");
        assert_eq!(
            open.loaded.keys().copied().collect::<Vec<_>>(),
            vec![CellCoord::new(0, 1)]
        );
        let _ = std::fs::remove_dir_all(&fx.root);
    }

    /// The map marks loaded cells with the accent, refused cells with the error color and a cross,
    /// and shades by height; every color is a theme token.
    #[test]
    fn the_map_marks_loaded_and_refused_cells_with_theme_tokens() {
        let tokens = ThemeTokens::regia_default();
        let plain = cell_look(&tokens, &CellStatus::Ready, false, false, 0.0);
        let high = cell_look(&tokens, &CellStatus::Ready, false, false, 1.0);
        let loaded = cell_look(&tokens, &CellStatus::Ready, true, false, 0.0);
        let refused = cell_look(&tokens, &CellStatus::Error("cell_missing"), false, false, 0.0);
        let selected = cell_look(&tokens, &CellStatus::Ready, false, true, 0.0);
        assert_ne!(plain.fill, high.fill, "height shades the cell");
        assert_ne!(plain.fill, loaded.fill, "a loaded cell is marked");
        assert_eq!(loaded.fill, tokens.signal.lerp_to_gamma(plain.fill, 0.35));
        assert!(refused.cross && refused.stroke.color == tokens.error);
        assert_eq!(selected.stroke.color, tokens.strong);
        assert!(!plain.cross);
    }

    /// Unloading a cell with unsaved edits from the panel asks first; Keep loaded changes nothing,
    /// Discard and unload drops the edit (nothing written).
    #[test]
    fn unloading_an_edited_cell_asks_save_discard_or_keep() {
        let fx = fixture("world-panel-prompt");
        let mut app = editor_app();
        let world = app.world_mut();
        world_edit::open_path(world, &fx.dir, Some(&fx.cache)).expect("open");
        let cell = CellCoord::new(2, 2);
        world_edit::start_load(world, &[cell]).expect("start");
        world_edit::wait_load(world);
        world_edit::tick(world);
        let member = world.resource::<WorldEdit>().world.as_ref().expect("open").loaded[&cell].entities()[0];
        world
            .get_mut::<regia_scene::Transform>(member)
            .expect("t")
            .translation
            .x = -3.0;
        let mut state = WorldMapUi::default();
        state.selected.insert(cell);
        unload_selected(world, &mut state, &[cell]);
        assert_eq!(state.prompt, Some(vec![cell]), "the prompt opens");
        assert!(world.get_entity(member).is_ok(), "nothing unloaded yet");
        let ctx = egui::Context::default();
        world.insert_resource(state);
        let mut draw = |ui: &mut egui::Ui| world_map_panel(ui, world);
        tab_to(&ctx, SIZE, "Discard and unload", false, 60, &mut draw);
        frame(
            &ctx,
            SIZE,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
        drop(draw);
        assert!(world.get_entity(member).is_err(), "unloaded");
        assert!(world.resource::<WorldMapUi>().prompt.is_none());
        let rel = regia_scene::level_dir::entity_rel_path(crate::world_edit::tests::guid_of(cell));
        let text = std::fs::read_to_string(fx.dir.join(rel)).expect("file");
        assert!(!text.contains("-3.0"), "discard wrote nothing: {text}");
        let _ = std::fs::remove_dir_all(&fx.root);
    }
}
