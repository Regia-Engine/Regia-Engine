// SPDX-License-Identifier: MPL-2.0
//! Modular kit painting on an XZ grid. Pieces remain ordinary level entities or prefab instances.
//!
//! The grid and palette are session state. A stroke uses existing spawn/despawn commands grouped
//! on the shared undo stack. Integer boundary traversal fills a face-connected straight line.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::{Quat, Vec2, Vec3};
use regia_render::MeshSource;
use regia_scene::prefab::PrefabInstance;
use regia_scene::prelude::{EntityGuid, Transform};
use serde_json::{Value, json};

use crate::command::{CommandGroup, DespawnEntities, EditorCommand, SetTransform, SpawnEntities, UndoStack};

/// Maximum number of cells in one undoable kit stroke.
pub const MAX_STROKE_CELLS: usize = 4096;
const CELL_LIMIT: i32 = 1_000_000;

/// Palette and placement settings of the kit grid tool (never written to the level).
#[derive(Resource, Clone, Debug)]
pub struct KitPaint {
    /// Whether this tool owns the viewport pointer.
    pub active: bool,
    /// Project-relative folder whose mesh and prefab files form the palette.
    pub folder: String,
    /// Selected project-relative piece path.
    pub piece: String,
    /// Width, depth and floor spacing of one cell, meters.
    pub grid: f32,
    /// Floor number; its height is `floor * grid`.
    pub floor: i32,
    /// Clockwise rotation in degrees: 0, 90, 180 or 270.
    pub rotation: u32,
    /// Current grid column/row, also editable with the keyboard.
    pub cursor: [i32; 2],
    /// Last result or validation error.
    pub message: String,
    pieces: Arc<Vec<String>>,
    drag: Option<([i32; 2], bool)>,
    drag_pane: Option<egui::Id>,
    input_frame: Option<u64>,
    input_panes: HashSet<egui::Id>,
    action_frame: Option<u64>,
    mode_frame: Option<u64>,
}

impl Default for KitPaint {
    fn default() -> Self {
        Self {
            active: false,
            folder: "assets/kits".into(),
            piece: String::new(),
            grid: 1.0,
            floor: 0,
            rotation: 0,
            cursor: [0, 0],
            message: String::new(),
            pieces: Arc::default(),
            drag: None,
            drag_pane: None,
            input_frame: None,
            input_panes: HashSet::new(),
            action_frame: None,
            mode_frame: None,
        }
    }
}

impl KitPaint {
    pub(crate) fn cancel_drag(&mut self) {
        self.drag = None;
        self.drag_pane = None;
    }
}

/// Cells crossed between two cell centers, in order, with face adjacency and no gaps.
///
/// Corner ties cross X first when moving toward positive X, Y first in reverse, so
/// reversing a stroke visits the same cells in reverse order. Work is bounded before allocation.
///
/// # Errors
/// Coordinates outside ±1,000,000 or a line longer than [`MAX_STROKE_CELLS`].
pub fn line_cells(from: [i32; 2], to: [i32; 2]) -> Result<Vec<[i32; 2]>, String> {
    if from
        .into_iter()
        .chain(to)
        .any(|v| v.unsigned_abs() > CELL_LIMIT as u32)
    {
        return Err("cell coordinate is outside the supported range".into());
    }
    let nx = i64::from(from[0].abs_diff(to[0]));
    let ny = i64::from(from[1].abs_diff(to[1]));
    if nx + ny + 1 > MAX_STROKE_CELLS as i64 {
        return Err("kit stroke has too many cells".into());
    }
    let (sx, sy) = ((to[0] - from[0]).signum(), (to[1] - from[1]).signum());
    let (mut ix, mut iy) = (0, 0);
    let mut p = from;
    let mut cells = vec![p];
    while ix < nx || iy < ny {
        let x_crossing = (2 * ix + 1) * ny;
        let y_crossing = (2 * iy + 1) * nx;
        if ix < nx && (iy == ny || x_crossing < y_crossing || (x_crossing == y_crossing && sx > 0)) {
            p[0] += sx;
            ix += 1;
        } else {
            p[1] += sy;
            iy += 1;
        }
        cells.push(p);
    }
    Ok(cells)
}

fn valid_settings(state: &KitPaint) -> Result<(), String> {
    if !state.grid.is_finite()
        || !(0.01..=10_000.0).contains(&state.grid)
        || state.floor.unsigned_abs() > CELL_LIMIT as u32
        || ![0, 90, 180, 270].contains(&state.rotation)
    {
        return Err("grid must be 0.01..=10000 meters, floor within ±1000000, rotation 0/90/180/270".into());
    }
    Ok(())
}

fn resolve(world: &World, rel: &str) -> Result<PathBuf, String> {
    let root = crate::agent_ext::project_root(world).map_err(|e| e.message)?;
    regia_agent::req_path::project_path_in(&root, "piece", rel).map_err(|e| e.message)
}

fn is_prefab(path: &str) -> bool {
    path.ends_with(".prefab.ron") || path.ends_with(".prefab.variant.ron")
}
fn is_piece(path: &str) -> bool {
    is_prefab(path) || path.ends_with(".glb") || path.ends_with(".gltf")
}

fn palette(world: &World, folder: &str) -> Result<Vec<String>, String> {
    let full = resolve(world, folder)?;
    let mut pieces = Vec::new();
    for entry in std::fs::read_dir(&full).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if is_piece(&name) {
            let rel = format!("{}/{name}", folder.trim_end_matches('/'));
            // The project path check also refuses symlinks that leave the project.
            let path = resolve(world, &rel)?;
            if path.is_file() {
                pieces.push(rel);
            }
        }
        if pieces.len() > MAX_STROKE_CELLS {
            return Err("kit palette has too many pieces".into());
        }
    }
    pieces.sort();
    Ok(pieces)
}

fn prefab_ids(world: &World, pieces: &[String]) -> Result<BTreeSet<u128>, String> {
    let mut ids = BTreeSet::new();
    for piece in pieces.iter().filter(|p| is_prefab(p)) {
        let path = regia_asset::meta_path_for(&resolve(world, piece)?);
        match std::fs::read(path) {
            Ok(bytes) => {
                ids.insert(
                    regia_asset::MetaSidecar::from_ron(&bytes)
                        .map_err(|e| e.to_string())?
                        .id
                        .0
                        .as_u128(),
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(ids)
}

fn at_cell(transform: &Transform, state: &KitPaint) -> Option<[i32; 2]> {
    let p = transform.translation / state.grid;
    if !p.is_finite() || (p.y - state.floor as f32).abs() > 0.001 {
        return None;
    }
    let cell = [p.x.round(), p.z.round()];
    if (p.x - cell[0]).abs() > 0.001 || (p.z - cell[1]).abs() > 0.001 {
        return None;
    }
    Some([cell[0] as i32, cell[1] as i32])
}

fn stroke(world: &mut World, state: &KitPaint, cells: &[[i32; 2]], erase: bool) -> Result<Value, String> {
    let _span = tracing::info_span!("kit_paint.stroke").entered();
    valid_settings(state)?;
    if cells.is_empty()
        || cells.len() > MAX_STROKE_CELLS
        || cells
            .iter()
            .flatten()
            .any(|v| v.unsigned_abs() > CELL_LIMIT as u32)
    {
        return Err("cells must contain 1..=4096 coordinates within ±1000000".into());
    }
    let pieces = palette(world, &state.folder)?;
    if !erase && !pieces.contains(&state.piece) {
        return Err("choose a piece from the kit folder".into());
    }
    if !world.contains_resource::<AppTypeRegistry>() {
        return Err("the editor scene type registry is missing".into());
    }
    let prefab_ids = prefab_ids(world, &pieces)?;
    let cells: BTreeSet<_> = cells.iter().copied().collect();
    let mut occupied = BTreeSet::new();
    let mut erase_ids = Vec::new();
    for entity in world.iter_entities() {
        if entity.contains::<ChildOf>() {
            continue;
        }
        let belongs = entity
            .get::<MeshSource>()
            .is_some_and(|m| pieces.contains(&m.mesh))
            || entity
                .get::<PrefabInstance>()
                .is_some_and(|p| prefab_ids.contains(&p.prefab));
        if !belongs {
            continue;
        }
        if let Some(cell) = entity.get::<Transform>().and_then(|t| at_cell(t, state)) {
            occupied.insert(cell);
            if cells.contains(&cell) {
                erase_ids.push(entity.id().to_bits());
            }
        }
    }
    if erase {
        let changed = erase_ids.len();
        if changed > 0 {
            let mut command = DespawnEntities {
                entities: erase_ids,
                snapshot: None,
            };
            command.apply(world);
            world
                .get_resource_or_insert_with(UndoStack::new)
                .push(Box::new(command));
        }
        return Ok(json!({"changed":changed, "entities":[], "undoable":true}));
    }
    let full = resolve(world, &state.piece)?;
    // Existing prefab spawning creates its own undo command. Isolate these temporary commands,
    // then commit one group, preserving the original redo history if any piece fails to spawn.
    let original_stack = world.remove_resource::<UndoStack>().unwrap_or_default();
    world.insert_resource(UndoStack::new());
    let mut commands = Vec::new();
    let result = (|| {
        let mut placed = Vec::new();
        for cell in cells.difference(&occupied) {
            let position = Vec3::new(cell[0] as f32, state.floor as f32, cell[1] as f32) * state.grid;
            let transform = Transform {
                translation: position,
                rotation: Quat::from_rotation_y(-(state.rotation as f32).to_radians()),
                ..Transform::default()
            };
            let entity = if is_prefab(&state.piece) {
                let root = crate::content::spawn_prefab_asset(world, &full, position)?;
                let mut rotate = SetTransform::capture(world, root, transform);
                rotate.apply(world);
                world.resource_mut::<UndoStack>().push(Box::new(rotate));
                root
            } else {
                let guid = Guid::new_random();
                let name = Path::new(&state.piece)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Kit piece");
                let mesh = MeshSource::new(&state.piece, "");
                let mut command = SpawnEntities::new_entity(world, guid, name, transform)
                    .and_then(|c| c.with_component(world, mesh.as_partial_reflect()))
                    .map_err(|e| e.to_string())?;
                command.apply(world);
                world.resource_mut::<UndoStack>().push(Box::new(command));
                world
                    .iter_entities()
                    .find(|e| e.get::<EntityGuid>().is_some_and(|g| g.0 == guid))
                    .map(|e| e.id())
                    .ok_or("kit piece could not be spawned")?
            };
            drain_commands(world, &mut commands);
            placed.push(entity.to_bits());
        }
        Ok::<_, String>(placed)
    })();
    drain_commands(world, &mut commands);
    let mut group = CommandGroup {
        label: "Paint kit cells".into(),
        commands,
    };
    if result.is_err() {
        group.revert(world);
    }
    world.insert_resource(original_stack);
    let placed = result?;
    if !group.commands.is_empty() {
        world.resource_mut::<UndoStack>().push(Box::new(group));
    }
    Ok(json!({"changed":placed.len(), "entities":placed, "undoable":true}))
}

// Drain after each cell so even a maximum-length stroke cannot hit the temporary stack's cap.
fn drain_commands(world: &mut World, commands: &mut Vec<Box<dyn EditorCommand>>) {
    let mut cell = Vec::new();
    while let Some(command) = world.resource_mut::<UndoStack>().take_top() {
        cell.push(command);
    }
    commands.extend(cell.into_iter().rev());
}

fn state_json(state: &KitPaint) -> Value {
    json!({"enabled":state.active,"folder":state.folder,"piece":state.piece,"grid":state.grid,
        "floor":state.floor,"rotation":state.rotation,"cells":[state.cursor],"pieces":&*state.pieces})
}

fn operate(world: &mut World, params: &Value) -> Result<Value, String> {
    let mut state = world.get_resource::<KitPaint>().cloned().unwrap_or_default();
    for (key, slot) in [("folder", &mut state.folder), ("piece", &mut state.piece)] {
        if let Some(value) = params.get(key) {
            *slot = value
                .as_str()
                .ok_or_else(|| format!("{key} must be a string"))?
                .into();
        }
    }
    if let Some(v) = params.get("grid") {
        state.grid = v.as_f64().ok_or("grid must be a number")? as f32;
    }
    if let Some(v) = params.get("floor") {
        state.floor = v
            .as_i64()
            .and_then(|v| i32::try_from(v).ok())
            .ok_or("floor must be a 32-bit integer")?;
    }
    if let Some(v) = params.get("rotation") {
        state.rotation = v
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or("rotation must be an integer")?;
    }
    if let Some(v) = params.get("enabled") {
        state.active = v.as_bool().ok_or("enabled must be a boolean")?;
        if !state.active {
            state.cancel_drag();
        }
    }
    let op = match params.get("op") {
        Some(v) => v.as_str().ok_or("op must be a string")?,
        None => "paint",
    };
    let erase = match op {
        "paint" => false,
        "erase" => true,
        _ => return Err("op must be paint or erase".into()),
    };
    valid_settings(&state)?;
    let cells = params
        .get("cells")
        .map(|v| {
            let values = v
                .as_array()
                .filter(|a| !a.is_empty() && a.len() <= MAX_STROKE_CELLS)
                .ok_or("cells must be a nonempty bounded array")?;
            values
                .iter()
                .map(|v| {
                    let a = v
                        .as_array()
                        .filter(|a| a.len() == 2)
                        .ok_or("each cell must be [column,row]")?;
                    let mut out = [0; 2];
                    for (out, v) in out.iter_mut().zip(a) {
                        *out = v
                            .as_i64()
                            .and_then(|v| i32::try_from(v).ok())
                            .ok_or("cell coordinates must be 32-bit integers")?;
                    }
                    Ok(out)
                })
                .collect::<Result<Vec<_>, String>>()
        })
        .transpose()?;
    if params.get("folder").is_some() {
        state.pieces = Arc::new(palette(world, &state.folder)?);
    }
    if !state.piece.is_empty() && !state.pieces.contains(&state.piece) {
        state.pieces = Arc::new(palette(world, &state.folder)?);
        if !state.pieces.contains(&state.piece) {
            return Err("piece is not in the kit folder".into());
        }
    }
    let result = if let Some(cells) = cells {
        stroke(world, &state, &cells, erase)?
    } else {
        state_json(&state)
    };
    if state.active {
        world
            .get_resource_or_insert_with(crate::terrain_tool::TerrainTool::default)
            .active = false;
        if let Some(mut region) = world.get_resource_mut::<crate::terrain_region::TerrainRegion>() {
            region.active = false;
            region.cancel_drag();
        }
    }
    world.insert_resource(state);
    Ok(result)
}

fn handler(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    operate(world, params).map_err(|message| RpcError {
        code: regia_agent::protocol::INVALID_PARAMS,
        message,
        data: Some(json!({"code":"kit_paint_invalid","reason":"kit_paint_invalid","hint":"see book/agents/kit-painting.md"})),
    })
}

/// Schema and dispatch handler for kit painting and session settings.
pub fn methods() -> Vec<(MethodSchema, crate::viewport_agent::Handler)> {
    vec![(
        MethodSchema {
            name: "editor.kit_paint",
            tool: "kit_paint",
            description: "Paint or erase kit cells as one undo step, or configure the grid palette.",
            params: &[
                ParamSchema {
                    name: "piece",
                    kind: "string",
                    required: false,
                    description: "Project-relative .glb, .gltf, .prefab.ron or .prefab.variant.ron in folder.",
                },
                ParamSchema {
                    name: "cells",
                    kind: "array",
                    required: false,
                    description: "1..=4096 [column,row] pairs; omit to configure/read state.",
                },
                ParamSchema {
                    name: "floor",
                    kind: "integer",
                    required: false,
                    description: "Floor index; world Y = floor * grid.",
                },
                ParamSchema {
                    name: "rotation",
                    kind: "integer",
                    required: false,
                    description: "Clockwise degrees: 0,90,180,270.",
                },
                ParamSchema {
                    name: "grid",
                    kind: "number",
                    required: false,
                    description: "Cell and floor spacing, 0.01..=10000 meters; default 1.",
                },
                ParamSchema {
                    name: "folder",
                    kind: "string",
                    required: false,
                    description: "Project-relative kit folder; refreshes the palette.",
                },
                ParamSchema {
                    name: "op",
                    kind: "string",
                    required: false,
                    description: "paint (default) | erase; erase affects this palette's pieces only.",
                },
                ParamSchema {
                    name: "enabled",
                    kind: "boolean",
                    required: false,
                    description: "Arm or disarm the viewport grid tool.",
                },
            ],
            result: "{changed,entities,undoable} | {enabled,folder,piece,grid,floor,rotation,cells,pieces}",
            mutating: true,
            permission: Permission::EditScene,
        },
        handler,
    )]
}

/// Focusable palette controls. Invoked from the viewport toolbar while the tool is open.
pub fn panel(ui: &mut egui::Ui, world: &mut World) {
    let _span = tracing::trace_span!("kit_paint.panel").entered();
    use crate::agent_parity::{self as parity, action_button, action_button_text};
    let mut state = world.get_resource_or_insert_with(KitPaint::default).clone();
    let folder_label = ui.label("Kit folder");
    ui.text_edit_singleline(&mut state.folder)
        .labelled_by(folder_label.id);
    let refresh = action_button(ui, &parity::KIT_PAINT_REFRESH).clicked();
    ui.add(
        egui::Slider::new(&mut state.grid, 0.01..=10000.0)
            .logarithmic(true)
            .text("Grid size"),
    );
    let floor_response = ui.add(
        egui::DragValue::new(&mut state.floor)
            .range(-CELL_LIMIT..=CELL_LIMIT)
            .prefix("Floor "),
    );
    crate::a11y_audit::named_drag_value(floor_response, "Floor", f64::from(state.floor));
    let mut turns = state.rotation / 90;
    ui.add(egui::Slider::new(&mut turns, 0..=3).text("Quarter turns"));
    state.rotation = turns * 90;
    ui.horizontal(|ui| {
        for (name, value) in ["Column", "Row"].into_iter().zip(&mut state.cursor) {
            let response = ui.add(
                egui::DragValue::new(value)
                    .range(-CELL_LIMIT..=CELL_LIMIT)
                    .prefix(format!("{name} ")),
            );
            crate::a11y_audit::named_drag_value(response, name, f64::from(*value));
        }
    });
    ui.label("Pieces");
    egui::ScrollArea::vertical()
        .max_height(ui.spacing().interact_size.y * 8.0)
        .show(ui, |ui| {
            for piece in &*state.pieces {
                let text = if piece == &state.piece {
                    format!("Selected: {piece}")
                } else {
                    piece.clone()
                };
                if action_button_text(ui, &parity::KIT_PAINT_PIECE, text).clicked() {
                    state.piece = piece.clone();
                }
            }
        });
    let mut op = None;
    ui.horizontal(|ui| {
        if action_button(ui, &parity::KIT_PAINT_PLACE).clicked() {
            op = Some(false);
        }
        if action_button(ui, &parity::KIT_PAINT_ERASE).clicked() {
            op = Some(true);
        }
    });
    ui.label(
        "Viewport: arrows move cell, Enter places, Delete erases, R turns, PageUp/PageDown changes floor.",
    );
    ui.label(&state.message);
    let frame = ui.ctx().cumulative_frame_nr();
    if state.action_frame != Some(frame) && (refresh || op.is_some()) {
        state.action_frame = Some(frame);
        if refresh {
            match palette(world, &state.folder) {
                Ok(pieces) => {
                    state.piece = refreshed_piece(&state.piece, &pieces);
                    state.pieces = Arc::new(pieces);
                    state.message.clear();
                }
                Err(e) => state.message = e,
            }
        }
        if let Some(erase) = op {
            state.message =
                stroke(world, &state, &[state.cursor], erase).map_or_else(|e| e, |v| v.to_string());
        }
    }
    world.insert_resource(state);
}

/// Toolbar switch and tool window; idle/closed tools perform no file or scene work.
pub fn toolbar(ui: &mut egui::Ui, world: &mut World) {
    let mut active = world.get_resource::<KitPaint>().is_some_and(|s| s.active);
    if crate::agent_parity::action_toggle(ui, &crate::agent_parity::KIT_PAINT_MODE, &mut active).changed() {
        let frame = ui.ctx().cumulative_frame_nr();
        let mut state = world.get_resource_or_insert_with(KitPaint::default);
        if state.mode_frame == Some(frame) {
            active = state.active;
        } else {
            state.active = active;
            if !active {
                state.cancel_drag();
            }
            state.mode_frame = Some(frame);
        }
        if active {
            world
                .get_resource_or_insert_with(crate::terrain_tool::TerrainTool::default)
                .active = false;
            if let Some(mut region) = world.get_resource_mut::<crate::terrain_region::TerrainRegion>() {
                region.active = false;
                region.cancel_drag();
            }
        }
    }
    if active {
        egui::Window::new("Kit painting")
            .vscroll(true)
            .show(ui.ctx(), |ui| panel(ui, world));
    }
}

/// Processes one focused pane's keyboard or pointer stroke and returns whether the tool owns it.
pub fn viewport_input(
    ui: &egui::Ui,
    world: &mut World,
    response: &egui::Response,
    camera: &crate::viewport::EditorCamera,
    size: Vec2,
) -> bool {
    let Some(mut state) = world.get_resource::<KitPaint>().filter(|s| s.active).cloned() else {
        return false;
    };
    if world
        .get_resource::<crate::terrain_tool::TerrainTool>()
        .is_some_and(|s| s.active)
    {
        state.active = false;
        state.cancel_drag();
        world.insert_resource(state);
        return false;
    }
    if state.drag.is_some() && state.drag_pane != Some(response.id) {
        return true;
    }
    if !response.hovered() && !response.has_focus() && state.drag.is_none() {
        return true;
    }
    let frame = ui.ctx().cumulative_frame_nr();
    if state.input_frame != Some(frame) {
        state.input_panes.clear();
        state.input_frame = Some(frame);
    }
    if !state.input_panes.insert(response.id) {
        return true;
    }
    let keys = response.has_focus() || (response.hovered() && !ui.ctx().egui_wants_keyboard_input());
    let mut op = None;
    if keys {
        let pressed = |key| ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, key));
        if pressed(egui::Key::PageUp) {
            state.floor = state.floor.saturating_add(1).min(CELL_LIMIT);
        }
        if pressed(egui::Key::PageDown) {
            state.floor = state.floor.saturating_sub(1).max(-CELL_LIMIT);
        }
        if pressed(egui::Key::R) {
            state.rotation = ((state.rotation / 90 + 1) % 4) * 90;
        }
        for (key, axis, delta) in [
            (egui::Key::ArrowLeft, 0, -1),
            (egui::Key::ArrowRight, 0, 1),
            (egui::Key::ArrowUp, 1, -1),
            (egui::Key::ArrowDown, 1, 1),
        ] {
            if pressed(key) {
                state.cursor[axis] = state.cursor[axis]
                    .saturating_add(delta)
                    .clamp(-CELL_LIMIT, CELL_LIMIT);
            }
        }
        if pressed(egui::Key::Enter) {
            op = Some((vec![state.cursor], false));
        }
        if pressed(egui::Key::Delete) {
            op = Some((vec![state.cursor], true));
        }
        if pressed(egui::Key::Escape) {
            state.drag = None;
            state.active = false;
        }
    }
    // Only pointer movement changes the keyboard cursor, so an unmoved mouse cannot undo arrows.
    let pointer_moves = ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO || i.pointer.any_pressed());
    let mut pointer_hit = false;
    if pointer_moves && let Some(p) = response.hover_pos() {
        let p = p - response.rect.left_top();
        let (origin, direction) = crate::viewport::pick_ray(camera, Vec2::new(p.x, p.y), size);
        if direction.y.abs() > 1e-6 {
            let t = (state.floor as f32 * state.grid - origin.y) / direction.y;
            if t.is_finite() && t >= 0.0 {
                pointer_hit = true;
                let hit = origin + direction * t;
                state.cursor = [
                    (hit.x / state.grid)
                        .round()
                        .clamp(-CELL_LIMIT as f32, CELL_LIMIT as f32) as i32,
                    (hit.z / state.grid)
                        .round()
                        .clamp(-CELL_LIMIT as f32, CELL_LIMIT as f32) as i32,
                ];
            }
        }
    }
    if response.hovered() && pointer_hit && !ui.input(|i| i.modifiers.alt) {
        if ui.input(|i| i.pointer.primary_pressed() || i.pointer.secondary_pressed()) {
            response.request_focus();
        }
        if ui.input(|i| i.pointer.primary_pressed()) {
            state.drag = Some((state.cursor, false));
            state.drag_pane = Some(response.id);
        }
        if ui.input(|i| i.pointer.secondary_pressed()) {
            state.drag = Some((state.cursor, true));
            state.drag_pane = Some(response.id);
        }
    }
    if let Some((from, erase)) = state.drag
        && ui.input(|i| {
            if erase {
                i.pointer.secondary_released()
            } else {
                i.pointer.primary_released()
            }
        })
    {
        state.drag = None;
        match line_cells(from, state.cursor) {
            Ok(cells) => op = Some((cells, erase)),
            Err(e) => state.message = e,
        }
    }
    if let Some((cells, erase)) = op {
        state.message = stroke(world, &state, &cells, erase).map_or_else(|e| e, |v| v.to_string());
    }
    world.insert_resource(state);
    true
}

/// The floor grid and cell preview, bounded to 21 lines on each axis.
pub fn paint_overlay(ui: &egui::Ui, world: &World, camera: &crate::viewport::EditorCamera, rect: egui::Rect) {
    let Some(state) = world.get_resource::<KitPaint>().filter(|s| s.active) else {
        return;
    };
    let _span = tracing::trace_span!("kit_paint.overlay").entered();
    let painter = ui.painter_at(rect);
    let tokens = crate::theme::tokens(world);
    let project = |x: f32, z: f32| {
        crate::viewport::project_to_screen(
            camera,
            Vec3::new(x * state.grid, state.floor as f32 * state.grid, z * state.grid),
            Vec2::new(rect.width(), rect.height()),
        )
        .map(|p| rect.left_top() + egui::vec2(p.x, p.y))
    };
    let line = |a: [f32; 2], b: [f32; 2], color| {
        if let (Some(a), Some(b)) = (project(a[0], a[1]), project(b[0], b[1])) {
            painter.line_segment([a, b], egui::Stroke::new(1.0, color));
        }
    };
    let [x, z] = state.cursor.map(|v| v as f32);
    for n in -10..=10 {
        let n = n as f32 - 0.5;
        line([x + n, z - 10.5], [x + n, z + 9.5], tokens.overlay.grid_minor);
        line([x - 10.5, z + n], [x + 9.5, z + n], tokens.overlay.grid_minor);
    }
    let cells = state
        .drag
        .and_then(|(start, _)| line_cells(start, state.cursor).ok())
        .unwrap_or_else(|| vec![state.cursor]);
    for [x, z] in cells {
        let (x, z) = (x as f32, z as f32);
        for (a, b) in [
            ([x - 0.5, z - 0.5], [x + 0.5, z - 0.5]),
            ([x + 0.5, z - 0.5], [x + 0.5, z + 0.5]),
            ([x + 0.5, z + 0.5], [x - 0.5, z + 0.5]),
            ([x - 0.5, z + 0.5], [x - 0.5, z - 0.5]),
        ] {
            line(a, b, tokens.overlay.selection_outline);
        }
    }
}

/// The piece selected after a palette refresh: the current one while the folder still has it, else
/// the first piece (review sprint-w12-rev-g8 #4).
fn refreshed_piece(current: &str, pieces: &[String]) -> String {
    if !current.is_empty() && pieces.iter().any(|p| p == current) {
        current.to_owned()
    } else {
        pieces.first().cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_keeps_the_selected_piece_while_the_folder_has_it() {
        let pieces = vec![
            "assets/kit/floor.prefab.ron".to_owned(),
            "assets/kit/wall.prefab.ron".to_owned(),
        ];
        assert_eq!(
            refreshed_piece("assets/kit/wall.prefab.ron", &pieces),
            "assets/kit/wall.prefab.ron"
        );
        assert_eq!(
            refreshed_piece("assets/kit/gone.prefab.ron", &pieces),
            "assets/kit/floor.prefab.ron"
        );
        assert_eq!(refreshed_piece("", &pieces), "assets/kit/floor.prefab.ron");
        assert_eq!(refreshed_piece("x", &[]), "");
    }

    fn fixture() -> (App, PathBuf, KitPaint) {
        let dir = regia_test_utils::unique_temp_dir("kit_paint");
        std::fs::create_dir_all(dir.join("assets/kit")).expect("folder");
        std::fs::write(
            dir.join("assets/kit/piece.gltf"),
            r#"{"asset":{"version":"2.0"},"scenes":[{"nodes":[]}],"scene":0}"#,
        )
        .expect("mesh");
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        crate::agent_ext::install(app.world_mut());
        app.world_mut().insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
        app.world_mut().init_resource::<UndoStack>();
        let open = crate::level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        let state = KitPaint {
            folder: "assets/kit".into(),
            piece: "assets/kit/piece.gltf".into(),
            grid: 2.0,
            floor: 3,
            rotation: 90,
            ..Default::default()
        };
        (app, dir, state)
    }

    fn meshes(world: &World) -> usize {
        world
            .iter_entities()
            .filter(|e| e.contains::<MeshSource>())
            .count()
    }

    #[test]
    fn kit_paint_line_is_bounded_continuous_and_covers_both_ends() {
        for end in [
            [0, 0],
            [8, 0],
            [-8, 0],
            [0, -7],
            [3, 9],
            [-9, 3],
            [7, -4],
            [-7, -4],
        ] {
            let line = line_cells([0, 0], end).expect("line");
            assert_eq!(line.first(), Some(&[0, 0]));
            assert_eq!(line.last(), Some(&end));
            assert_eq!(
                line.len(),
                (end[0].unsigned_abs() + end[1].unsigned_abs() + 1) as usize
            );
            for pair in line.windows(2) {
                assert_eq!(
                    pair[0][0].abs_diff(pair[1][0]) + pair[0][1].abs_diff(pair[1][1]),
                    1
                );
            }
            assert_eq!(line.iter().copied().collect::<BTreeSet<_>>().len(), line.len());
            assert_eq!(
                line.iter().rev().copied().collect::<Vec<_>>(),
                line_cells(end, [0, 0]).expect("reverse")
            );
        }
        assert!(line_cells([0, 0], [4096, 0]).is_err());
        assert!(line_cells([i32::MIN, 0], [0, 0]).is_err());
    }

    #[test]
    fn kit_paint_mesh_line_floor_rotation_erase_and_undo() {
        let (mut app, dir, state) = fixture();
        let world = app.world_mut();
        let cells = line_cells([0, 0], [5, 2]).expect("line");
        let out = stroke(world, &state, &cells, false).expect("paint");
        assert_eq!(out["changed"], cells.len());
        assert_eq!(meshes(world), cells.len());
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        for id in out["entities"].as_array().expect("ids") {
            let e = Entity::try_from_bits(id.as_u64().expect("bits")).expect("entity");
            let transform = world.get::<Transform>(e).expect("transform");
            assert_eq!(transform.translation.y, 6.0);
            assert!((transform.rotation * Vec3::Z + Vec3::X).length() < 1e-5);
            assert!(
                world.get::<regia_scene::prelude::LevelMember>(e).is_some(),
                "saved with the level"
            );
        }
        assert_eq!(
            stroke(world, &state, &cells, false).expect("repeat")["changed"],
            0
        );
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            1,
            "repeat does not dirty history"
        );
        assert!(crate::command::undo_world(world));
        assert_eq!(meshes(world), 0);
        assert!(crate::command::redo_world(world));
        assert_eq!(meshes(world), cells.len());
        assert_eq!(
            stroke(world, &state, &cells, true).expect("erase")["changed"],
            cells.len()
        );
        assert_eq!(meshes(world), 0);
        assert!(crate::command::undo_world(world));
        assert_eq!(meshes(world), cells.len());
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn kit_paint_long_stroke_keeps_every_undo_command_past_the_stack_cap() {
        let (mut app, dir, state) = fixture();
        let world = app.world_mut();
        let cells = line_cells([0, 0], [crate::command::UNDO_CAP as i32 + 1, 0]).expect("line");
        stroke(world, &state, &cells, false).expect("paint");
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        assert_eq!(meshes(world), cells.len());
        assert!(crate::command::undo_world(world));
        assert_eq!(meshes(world), 0);
        assert!(crate::command::redo_world(world));
        assert_eq!(meshes(world), cells.len());
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn kit_paint_prefabs_remain_instances_and_redo_keeps_the_turn() {
        let (mut app, dir, mut state) = fixture();
        let world = app.world_mut();
        let command = SpawnEntities::new_entity(world, Guid::new_random(), "Block", Transform::default())
            .expect("template");
        let file = dir.join("assets/kit/block.prefab.ron");
        std::fs::write(&file, command.file.to_ron_string().expect("ron")).expect("prefab");
        state.piece = "assets/kit/block.prefab.ron".into();
        stroke(world, &state, &[[1, 2], [2, 2]], false).expect("paint");
        let instances = |world: &World| {
            world
                .iter_entities()
                .filter(|e| e.contains::<PrefabInstance>())
                .count()
        };
        assert_eq!(instances(world), 2);
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        assert!(crate::command::undo_world(world));
        assert_eq!(instances(world), 0);
        assert!(crate::command::redo_world(world));
        assert_eq!(instances(world), 2);
        for entity in world.iter_entities().filter(|e| e.contains::<PrefabInstance>()) {
            let t = entity.get::<Transform>().expect("transform");
            assert_eq!(t.translation.y, 6.0);
            assert!((t.rotation * Vec3::Z + Vec3::X).length() < 1e-5);
        }
        assert_eq!(
            stroke(world, &state, &[[1, 2]], true).expect("erase")["changed"],
            1
        );
        assert!(crate::command::undo_world(world));
        assert_eq!(instances(world), 2);
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn kit_paint_refusal_preserves_scene_and_redo() {
        let (mut app, dir, mut state) = fixture();
        let world = app.world_mut();
        stroke(world, &state, &[[0, 0]], false).expect("paint");
        assert!(crate::command::undo_world(world));
        let before = world.resource::<UndoStack>().state_id();
        state.piece = "assets/kit/broken.prefab.ron".into();
        std::fs::write(dir.join(&state.piece), "not a prefab").expect("bad prefab");
        assert!(stroke(world, &state, &[[2, 0]], false).is_err());
        assert_eq!(meshes(world), 0);
        assert_eq!(world.resource::<UndoStack>().redo_depth(), 1);
        assert_eq!(world.resource::<UndoStack>().state_id(), before);
        assert!(crate::command::redo_world(world));
        assert_eq!(meshes(world), 1);
        assert!(operate(world, &json!({"floor":4,"grid":0.0})).is_err());
        assert!(operate(world, &json!({"folder":"../outside"})).is_err());
        assert!(operate(world, &json!({"op":123,"cells":[[1,1]]})).is_err());
        std::fs::remove_dir_all(dir).expect("cleanup");
    }
    #[test]
    fn kit_paint_panel_keyboard_place_is_named_and_one_edit_in_two_passes() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        let (mut app, dir, state) = fixture();
        let world = app.world_mut();
        world.insert_resource(state);
        let ctx = egui::Context::default();
        let repeat_pass = std::cell::Cell::new(false);
        let mut draw = |ui: &mut egui::Ui| {
            if repeat_pass.get() && ui.ctx().current_pass_index() == 0 {
                ui.ctx().request_discard("exercise repeated layout");
            }
            egui::CentralPanel::default().show(ui, |ui| panel(ui, world));
        };
        let tree = frame(&ctx, [1000.0, 1000.0], vec![], &mut draw);
        assert!(
            crate::a11y_audit::unnamed_widgets(&tree).is_empty(),
            "{:?}",
            crate::a11y_audit::widget_names(&tree)
        );
        tab_to(&ctx, [1000.0, 1000.0], "Place at cell", false, 40, &mut draw);
        repeat_pass.set(true);
        frame(
            &ctx,
            [1000.0, 1000.0],
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
        assert_eq!(
            meshes(world),
            1,
            "Enter places exactly one piece across two passes"
        );
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        std::fs::remove_dir_all(dir).expect("cleanup");
    }
}
