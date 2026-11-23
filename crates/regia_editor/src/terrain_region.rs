// SPDX-License-Identifier: MPL-2.0
//! Rectangular terrain transfers using the existing stroke undo and heightmap interchange.
//!
//! The clipboard is session memory, not a content format. Coordinates are sample columns/rows;
//! quarter turns permute samples without interpolation. Both UI and agent call `operate`.

use std::collections::BTreeSet;
use std::sync::Arc;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_app::prelude::*;
use regia_core::math::{IVec2, Vec2, Vec3};
use regia_terrain::brush::{StrokeRecord, StrokeSample};
use regia_terrain::{Brush, BrushTarget, DirtyRect, EDIT_CELL_SIZE, StrokeUndo, Terrain, TerrainPaint};
use serde_json::{Value, json};

use crate::command::{CommandGroup, EditorCommand, UndoStack};
use crate::terrain_panel::{HeightmapFormat, MAX_HEIGHTMAP_FILE, decode_png16, encode_png16};
use crate::terrain_tool::{TerrainStrokeCommand, TerrainTool};

/// Maximum clipboard samples, including each weight plane (bounded undo memory).
pub const MAX_REGION_VALUES: u64 = 1 << 22;

#[derive(Clone, Debug)]
struct RegionData {
    width: u32,
    height: u32,
    heights: Vec<f32>,
    weights: Vec<Vec<u8>>,
}

/// Session selection, destination and clipboard of the terrain Region mode.
#[derive(Resource, Clone, Debug)]
pub struct TerrainRegion {
    /// Rectangle selection mode is armed in the viewport.
    pub active: bool,
    /// Source rectangle `[column, row, width, height]` in samples.
    pub rect: [u32; 4],
    /// Destination minimum column/row, updated by the hovered terrain sample.
    pub cursor: [u32; 2],
    /// Clockwise rotation in degrees, one of 0, 90, 180, 270.
    pub rotation: u32,
    /// Added to pasted heights, in meters; zero copies bits unchanged.
    pub height_offset: f32,
    /// Project-relative interchange file.
    pub path: String,
    /// Whether headerless RAW interchange samples use big-endian byte order.
    pub big_endian: bool,
    /// Last operation result or validation error.
    pub message: String,
    clipboard: Option<Arc<RegionData>>,
    drag: Option<[u32; 2]>,
    drag_pane: Option<egui::Id>,
    input_frame: Option<u64>,
    action_frame: Option<u64>,
    mode_frame: Option<u64>,
    /// The sculpt tool's on/off state when Region mode turned on, put back when it turns off
    /// (review sprint-w12-rev-g8 #3).
    tool_before: Option<bool>,
}

impl Default for TerrainRegion {
    fn default() -> Self {
        Self {
            active: false,
            rect: [0, 0, 2, 2],
            cursor: [0, 0],
            rotation: 0,
            height_offset: 0.0,
            path: "assets/maps/region.png".into(),
            big_endian: false,
            message: String::new(),
            clipboard: None,
            drag: None,
            drag_pane: None,
            input_frame: None,
            action_frame: None,
            mode_frame: None,
            tool_before: None,
        }
    }
}

impl TerrainRegion {
    pub(crate) fn cancel_drag(&mut self) {
        self.drag = None;
        self.drag_pane = None;
    }
}

fn checked_rect(terrain: &Terrain, rect: [u32; 4], planes: u32) -> Result<(), String> {
    let [x, y, w, h] = rect;
    if w == 0
        || h == 0
        || x.checked_add(w).is_none_or(|v| v > terrain.map.width())
        || y.checked_add(h).is_none_or(|v| v > terrain.map.height())
    {
        return Err("region must be nonempty and wholly inside the terrain".into());
    }
    if u64::from(w) * u64::from(h) > MAX_REGION_VALUES / u64::from(planes.max(1)) {
        return Err(format!("region exceeds {MAX_REGION_VALUES} height/weight values"));
    }
    Ok(())
}

fn capture(world: &World, entity: Entity, rect: [u32; 4]) -> Result<RegionData, String> {
    let terrain = world.get::<Terrain>(entity).ok_or("no terrain")?;
    let paint = world.get::<TerrainPaint>(entity);
    let layers = paint.map_or(0, |p| p.0.layers());
    checked_rect(terrain, rect, layers + 1)?;
    if paint.is_some_and(|p| p.0.width() != terrain.map.width() || p.0.height() != terrain.map.height()) {
        return Err("terrain and weight grid sizes differ".into());
    }
    let [x, y, width, height] = rect;
    let mut heights = Vec::with_capacity((width * height) as usize);
    let mut weights = vec![Vec::with_capacity((width * height) as usize); layers as usize];
    for row in y..y + height {
        for col in x..x + width {
            heights.push(terrain.map.height_at_sample(col.into(), row.into()));
            if let Some(paint) = paint {
                for (layer, plane) in weights.iter_mut().enumerate() {
                    plane.push(paint.0.get(layer as u32, col, row));
                }
            }
        }
    }
    Ok(RegionData {
        width,
        height,
        heights,
        weights,
    })
}

fn rotated(col: u32, row: u32, w: u32, h: u32, turns: u32) -> [u32; 2] {
    match turns {
        1 => [h - 1 - row, col],
        2 => [w - 1 - col, h - 1 - row],
        3 => [row, w - 1 - col],
        _ => [col, row],
    }
}

fn paste(
    world: &mut World,
    entity: Entity,
    data: &RegionData,
    at: [u32; 2],
    rotation: u32,
    offset: f32,
) -> Result<usize, String> {
    let _span = tracing::info_span!("terrain_region.paste").entered();
    if ![0, 90, 180, 270].contains(&rotation) || !offset.is_finite() {
        return Err("rotation must be 0, 90, 180 or 270; height offset must be finite".into());
    }
    let turns = rotation / 90;
    let (w, h) = if turns.is_multiple_of(2) {
        (data.width, data.height)
    } else {
        (data.height, data.width)
    };
    let terrain = world.get::<Terrain>(entity).ok_or("no terrain")?;
    checked_rect(terrain, [at[0], at[1], w, h], data.weights.len() as u32 + 1)?;
    let paint = world.get::<TerrainPaint>(entity);
    if !data.weights.is_empty()
        && paint.is_none_or(|p| {
            p.0.layers() as usize != data.weights.len()
                || p.0.width() != terrain.map.width()
                || p.0.height() != terrain.map.height()
        })
    {
        return Err("destination must have the same weight layers and grid size".into());
    }
    let dirty = DirtyRect {
        col_start: at[0],
        col_end: at[0] + w,
        row_start: at[1],
        row_end: at[1] + h,
    };
    let mut heights = Vec::new();
    let mut weights = vec![Vec::new(); data.weights.len()];
    let mut tiles = BTreeSet::new();
    for row in 0..data.height {
        for col in 0..data.width {
            let i = (row * data.width + col) as usize;
            let [dx, dy] = rotated(col, row, data.width, data.height, turns);
            let (x, y) = (at[0] + dx, at[1] + dy);
            let position = IVec2::new(x as i32, y as i32);
            let before = terrain.map.height_at_sample(x.into(), y.into());
            let after = if offset == 0.0 {
                data.heights[i]
            } else {
                data.heights[i] + offset
            };
            if !after.is_finite() {
                return Err("height offset would produce a non-finite height".into());
            }
            if before.to_bits() != after.to_bits() {
                heights.push(StrokeSample {
                    position,
                    before,
                    after,
                });
                tiles.insert((x / EDIT_CELL_SIZE, y / EDIT_CELL_SIZE));
            }
            if let Some(paint) = paint {
                for (layer, samples) in weights.iter_mut().enumerate() {
                    let before = paint.0.get(layer as u32, x, y);
                    let after = data.weights[layer][i];
                    if before != after {
                        samples.push(StrokeSample {
                            position,
                            before,
                            after,
                        });
                    }
                }
            }
        }
    }
    let count = heights.len() + weights.iter().map(Vec::len).sum::<usize>();
    if count == 0 {
        return Ok(0);
    }
    let mut group = CommandGroup::new("Paste terrain region");
    if !heights.is_empty() {
        group.push(Box::new(TerrainStrokeCommand::new(
            entity,
            StrokeUndo::Height(StrokeRecord {
                brush: Brush::default(),
                samples: heights,
                dirty,
                tiles,
            }),
        )));
    }
    for (layer, samples) in weights.into_iter().enumerate() {
        if !samples.is_empty() {
            group.push(Box::new(TerrainStrokeCommand::new(
                entity,
                StrokeUndo::LayerWeight {
                    layer,
                    record: StrokeRecord {
                        brush: Brush {
                            target: BrushTarget::LayerWeight { layer },
                            ..Brush::default()
                        },
                        samples,
                        dirty,
                        tiles: BTreeSet::new(),
                    },
                },
            )));
        }
    }
    group.apply(world);
    world
        .get_resource_or_insert_with(UndoStack::new)
        .push(Box::new(group));
    Ok(count)
}

fn file_path(world: &World, rel: &str) -> Result<std::path::PathBuf, String> {
    let root = crate::agent_ext::project_root(world).map_err(|e| e.message)?;
    regia_agent::req_path::project_path_in(&root, "path", rel).map_err(|e| e.message)
}

fn export(
    world: &World,
    entity: Entity,
    rect: [u32; 4],
    path: &str,
    endian: regia_terrain::RawEndian,
) -> Result<Value, String> {
    let data = capture(world, entity, rect)?;
    if data.width < 2 || data.height < 2 {
        return Err("heightmap interchange needs at least 2x2 samples".into());
    }
    let terrain = world.get::<Terrain>(entity).ok_or("no terrain")?;
    let scale = terrain.map.height_scale();
    let inv = if scale.abs() > f32::EPSILON {
        65535.0 / scale
    } else {
        0.0
    };
    let offset = terrain.map.height_offset();
    let mut low = 0u64;
    let mut high = 0u64;
    let samples = data
        .heights
        .iter()
        .map(|&h| {
            let s = ((h - offset) * inv).round();
            low += u64::from(s < 0.0);
            high += u64::from(s > 65535.0);
            s.clamp(0.0, 65535.0) as u16
        })
        .collect();
    let hf = regia_asset::Heightfield {
        width: data.width,
        height: data.height,
        samples,
    };
    let full = file_path(world, path)?;
    let bytes = match HeightmapFormat::of_path(&full).ok_or("use .png, .r16 or .raw")? {
        HeightmapFormat::Png => encode_png16(&hf)?,
        HeightmapFormat::R16(_) => regia_terrain::encode_r16(&hf, endian),
    };
    crate::level_io::write_atomic(&full, &bytes)?;
    Ok(
        json!({"width": data.width, "height": data.height, "bytes": bytes.len(), "clamped_low": low, "clamped_high": high}),
    )
}

fn import(
    world: &mut World,
    entity: Entity,
    rect: [u32; 4],
    path: &str,
    endian: regia_terrain::RawEndian,
) -> Result<usize, String> {
    let full = file_path(world, path)?;
    let terrain = world.get::<Terrain>(entity).ok_or("no terrain")?;
    checked_rect(terrain, rect, 1)?;
    let max_bytes = (MAX_REGION_VALUES * 2 + (1 << 20)).min(MAX_HEIGHTMAP_FILE);
    if std::fs::metadata(&full).map_err(|e| e.to_string())?.len() > max_bytes {
        return Err("region heightmap file is too large".into());
    }
    let bytes = std::fs::read(&full).map_err(|e| e.to_string())?;
    let hf = match HeightmapFormat::of_path(&full).ok_or("use .png, .r16 or .raw")? {
        HeightmapFormat::Png => {
            let reader = png::Decoder::new(std::io::Cursor::new(&bytes))
                .read_info()
                .map_err(|e| e.to_string())?;
            if [reader.info().width, reader.info().height] != [rect[2], rect[3]] {
                return Err("file size must match the selected region".into());
            }
            decode_png16(&bytes)?
        }
        HeightmapFormat::R16(_) => {
            regia_terrain::decode_r16(&bytes, rect[2], rect[3], endian).map_err(|e| e.to_string())?
        }
    };
    if [hf.width, hf.height] != [rect[2], rect[3]] {
        return Err("file size must match the selected region".into());
    }
    let scale = terrain.map.height_scale();
    let offset = terrain.map.height_offset();
    let data = RegionData {
        width: hf.width,
        height: hf.height,
        heights: hf
            .samples
            .iter()
            .map(|&s| f32::from(s) / 65535.0 * scale + offset)
            .collect(),
        weights: Vec::new(),
    };
    paste(world, entity, &data, [rect[0], rect[1]], 0, 0.0)
}

fn array<const N: usize>(params: &Value, key: &str, default: [u32; N]) -> Result<[u32; N], String> {
    let Some(value) = params.get(key) else {
        return Ok(default);
    };
    let values = value
        .as_array()
        .filter(|a| a.len() == N)
        .ok_or_else(|| format!("{key} needs {N} integers"))?;
    let mut out = default;
    for (out, v) in out.iter_mut().zip(values) {
        *out = v
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| format!("{key} needs nonnegative 32-bit integers"))?;
    }
    Ok(out)
}

fn operate(world: &mut World, params: &Value) -> Result<Value, String> {
    let _span = tracing::info_span!("terrain_region.operate").entered();
    if world
        .get_resource::<TerrainTool>()
        .is_some_and(TerrainTool::stroking)
    {
        return Err("finish the active terrain stroke first".into());
    }
    crate::terrain_tool::drain_stroke_log(world);
    let entity = if let Some(value) = params.get("entity") {
        value
            .as_u64()
            .and_then(Entity::try_from_bits)
            .ok_or("entity must be a live terrain id")?
    } else {
        crate::terrain_tool::target_terrain(world).ok_or("no terrain")?
    };
    let state = world.get_resource_or_insert_with(TerrainRegion::default).clone();
    let rect = array(params, "rect", state.rect)?;
    let at = array(params, "at", state.cursor)?;
    let rotation = match params.get("rotation") {
        Some(v) => v
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or("rotation must be an integer")?,
        None => state.rotation,
    };
    let offset = match params.get("height_offset") {
        Some(v) => v.as_f64().ok_or("height_offset must be a number")? as f32,
        None => state.height_offset,
    };
    let path = match params.get("path") {
        Some(v) => v.as_str().ok_or("path must be a string")?,
        None => &state.path,
    };
    let endian = match params.get("endian") {
        None => {
            if state.big_endian {
                regia_terrain::RawEndian::Big
            } else {
                regia_terrain::RawEndian::Little
            }
        }
        Some(Value::String(s)) if s == "little" => regia_terrain::RawEndian::Little,
        Some(Value::String(s)) if s == "big" => regia_terrain::RawEndian::Big,
        _ => return Err("endian must be little or big".into()),
    };
    match params
        .get("op")
        .and_then(Value::as_str)
        .ok_or("op must be copy, paste, export or import")?
    {
        "copy" => {
            let data = capture(world, entity, rect)?;
            let result = json!({"width": data.width, "height": data.height, "layers": data.weights.len()});
            let mut state = world.resource_mut::<TerrainRegion>();
            state.clipboard = Some(Arc::new(data));
            state.rect = rect;
            Ok(result)
        }
        "paste" => {
            let data = state.clipboard.ok_or("copy a region first")?;
            let changed = paste(world, entity, &data, at, rotation, offset)?;
            Ok(json!({"changed": changed, "undoable": true}))
        }
        "export" => export(world, entity, rect, path, endian),
        "import" => Ok(json!({"changed": import(world, entity, rect, path, endian)?, "undoable": true})),
        _ => Err("op must be copy, paste, export or import".into()),
    }
}

fn handler(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    operate(world, params).map_err(|message| RpcError {
        code: regia_agent::protocol::INVALID_PARAMS,
        message,
        data: Some(json!({"code": "terrain_region_invalid", "reason": "terrain_region_invalid", "hint": "see book/agents/terrain-editing.md"})),
    })
}

/// Schema and dispatch handler for the region operations.
pub fn methods() -> Vec<(MethodSchema, crate::viewport_agent::Handler)> {
    vec![(
        MethodSchema {
            name: "editor.terrain_region",
            tool: "terrain_region",
            description: "Copy, paste, export or import a terrain rectangle; edits are one exact undo step.",
            params: &[
                ParamSchema {
                    name: "op",
                    kind: "string",
                    required: true,
                    description: "copy | paste | export | import.",
                },
                ParamSchema {
                    name: "entity",
                    kind: "integer",
                    required: false,
                    description: "Terrain id; default selected terrain.",
                },
                ParamSchema {
                    name: "rect",
                    kind: "array",
                    required: false,
                    description: "[column,row,width,height], default selection.",
                },
                ParamSchema {
                    name: "at",
                    kind: "array",
                    required: false,
                    description: "Paste minimum [column,row], default cursor.",
                },
                ParamSchema {
                    name: "rotation",
                    kind: "integer",
                    required: false,
                    description: "Clockwise degrees: 0,90,180,270; default panel value.",
                },
                ParamSchema {
                    name: "height_offset",
                    kind: "number",
                    required: false,
                    description: "Paste height offset in meters; default panel value.",
                },
                ParamSchema {
                    name: "endian",
                    kind: "string",
                    required: false,
                    description: "RAW byte order: little or big; PNG always follows its standard.",
                },
                ParamSchema {
                    name: "path",
                    kind: "string",
                    required: false,
                    description: "Project-relative .png, .r16 or .raw; default panel file.",
                },
            ],
            result: "{width,height,layers} | {changed,undoable} | {width,height,bytes,clamped_low,clamped_high}",
            mutating: true,
            permission: Permission::EditScene,
        },
        handler,
    )]
}

/// Region controls; every action and coordinate is keyboard accessible through a normal widget.
pub fn panel(ui: &mut egui::Ui, world: &mut World) {
    use crate::agent_parity::{self as parity, action_button, action_toggle};
    let mut state = world.get_resource_or_insert_with(TerrainRegion::default).clone();
    ui.heading("Terrain region");
    let old_active = state.active;
    let mode_changed = action_toggle(ui, &parity::TERRAIN_REGION_MODE, &mut state.active).changed();
    if mode_changed {
        let frame = ui.ctx().cumulative_frame_nr();
        if state.mode_frame == Some(frame) {
            state.active = old_active;
        } else {
            state.mode_frame = Some(frame);
        }
    }
    ui.horizontal_wrapped(|ui| {
        for (label, value) in ["Column", "Row", "Width", "Height"]
            .into_iter()
            .zip(&mut state.rect)
        {
            let response = ui.add(egui::DragValue::new(value).prefix(format!("{label} ")));
            crate::a11y_audit::named_drag_value(response, label, f64::from(*value));
        }
    });
    ui.horizontal_wrapped(|ui| {
        for (label, value) in ["Paste column", "Paste row"].into_iter().zip(&mut state.cursor) {
            let response = ui.add(egui::DragValue::new(value).prefix(format!("{label} ")));
            crate::a11y_audit::named_drag_value(response, label, f64::from(*value));
        }
    });
    let mut turns = state.rotation / 90;
    ui.add(egui::Slider::new(&mut turns, 0..=3).text("Quarter turns"));
    state.rotation = turns * 90;
    let offset_response = ui.add(
        egui::DragValue::new(&mut state.height_offset)
            .prefix("Height offset ")
            .suffix(" m"),
    );
    crate::a11y_audit::named_drag_value(offset_response, "Height offset", f64::from(state.height_offset));
    let file_label = ui.label("Region file");
    ui.text_edit_singleline(&mut state.path)
        .labelled_by(file_label.id);
    action_toggle(ui, &parity::TERRAIN_REGION_ENDIAN, &mut state.big_endian);
    let mut op = None;
    ui.horizontal_wrapped(|ui| {
        for (action, name) in [
            (&parity::TERRAIN_REGION_COPY, "copy"),
            (&parity::TERRAIN_REGION_PASTE, "paste"),
            (&parity::TERRAIN_REGION_EXPORT, "export"),
            (&parity::TERRAIN_REGION_IMPORT, "import"),
        ] {
            if action_button(ui, action).clicked() {
                op = Some(name);
            }
        }
    });
    let frame = ui.ctx().cumulative_frame_nr();
    let run = op.filter(|_| state.action_frame != Some(frame));
    if run.is_some() {
        state.action_frame = Some(frame);
    }
    if mode_changed {
        apply_mode_change(world, &mut state);
    }
    ui.label(&state.message);
    world.insert_resource(state);
    if let Some(op) = run {
        let result = operate(world, &json!({"op": op}));
        world.resource_mut::<TerrainRegion>().message = result.map_or_else(|e| e, |v| v.to_string());
    }
}

/// Region mode just turned on or off (`state.active`): on, the sculpt tool turns on (Region drags
/// run through it) and kit painting off, remembering whether the tool was on; off, the drag ends
/// and the tool goes back to how it was.
fn apply_mode_change(world: &mut World, state: &mut TerrainRegion) {
    if state.active {
        let mut tool = world.get_resource_or_insert_with(TerrainTool::default);
        state.tool_before.get_or_insert(tool.active);
        tool.active = true;
        if let Some(mut kit) = world.get_resource_mut::<crate::kit_paint::KitPaint>() {
            kit.active = false;
            kit.cancel_drag();
        }
    } else {
        state.cancel_drag();
        if let Some(was) = state.tool_before.take()
            && let Some(mut tool) = world.get_resource_mut::<TerrainTool>()
        {
            tool.active = was;
        }
    }
}

pub(crate) fn dragging(world: &World, pane: egui::Id) -> bool {
    world
        .get_resource::<TerrainRegion>()
        .is_some_and(|s| s.drag.is_some() && s.drag_pane == Some(pane))
}

/// Handles a terrain rectangle drag while Region mode is active. Called by the terrain tool.
pub fn viewport_input(
    ui: &egui::Ui,
    world: &mut World,
    response: &egui::Response,
    camera: &crate::viewport::EditorCamera,
    size: Vec2,
) {
    let state = world.resource::<TerrainRegion>();
    if state.drag.is_some() && state.drag_pane != Some(response.id) {
        return;
    }
    if !response.hovered() && world.resource::<TerrainRegion>().drag.is_none() {
        return;
    }
    let frame = ui.ctx().cumulative_frame_nr();
    if world.resource::<TerrainRegion>().input_frame == Some(frame) {
        return;
    }
    world.resource_mut::<TerrainRegion>().input_frame = Some(frame);
    let hit = response.hover_pos().and_then(|p| {
        let p = p - response.rect.left_top();
        let (origin, direction) = crate::viewport::pick_ray(camera, Vec2::new(p.x, p.y), size);
        crate::terrain_tool::pick_terrain(world, origin, direction)
    });
    let Some((entity, point)) = hit else {
        if ui.input(|i| i.pointer.primary_released()) {
            world.resource_mut::<TerrainRegion>().drag = None;
        }
        return;
    };
    let Some(terrain) = world.get::<Terrain>(entity) else {
        return;
    };
    let sample = regia_terrain::brush::world_to_sample(&terrain.map, point.x, point.z).round();
    let at = [
        sample.x.clamp(0.0, (terrain.map.width() - 1) as f32) as u32,
        sample.y.clamp(0.0, (terrain.map.height() - 1) as f32) as u32,
    ];
    let mut state = world.resource_mut::<TerrainRegion>();
    state.cursor = at;
    if ui.input(|i| i.pointer.primary_pressed() && !i.modifiers.alt) {
        state.drag = Some(at);
        state.drag_pane = Some(response.id);
    }
    if let Some(start) = state.drag {
        state.rect = [
            start[0].min(at[0]),
            start[1].min(at[1]),
            start[0].abs_diff(at[0]) + 1,
            start[1].abs_diff(at[1]) + 1,
        ];
    }
    if ui.input(|i| i.pointer.primary_released()) {
        state.drag = None;
    }
}

/// Draws the selected rectangle using the editor's selection token.
pub fn paint_overlay(ui: &egui::Ui, world: &World, camera: &crate::viewport::EditorCamera, rect: egui::Rect) {
    let _span = tracing::trace_span!("terrain_region_overlay").entered();
    let Some(state) = world.get_resource::<TerrainRegion>().filter(|s| s.active) else {
        return;
    };
    let terrain = world
        .get_resource::<crate::selection::Selection>()
        .and_then(|s| s.primary())
        .and_then(|e| world.get::<Terrain>(e))
        .or_else(|| world.iter_entities().find_map(|e| e.get::<Terrain>()));
    let Some(terrain) = terrain else {
        return;
    };
    if checked_rect(terrain, state.rect, 1).is_err() {
        return;
    }
    let [x, y, w, h] = state.rect;
    let corners = [
        [x, y],
        [x + w - 1, y],
        [x + w - 1, y + h - 1],
        [x, y + h - 1],
        [x, y],
    ];
    let screen: Vec<_> = corners
        .iter()
        .map(|&[col, row]| {
            let [x, z] = terrain.map.sample_to_world(col, row);
            crate::viewport::project_to_screen(
                camera,
                Vec3::new(x, terrain.map.height_at_sample(col.into(), row.into()), z),
                Vec2::new(rect.width(), rect.height()),
            )
            .map(|p| rect.left_top() + egui::vec2(p.x, p.y))
        })
        .collect();
    let painter = ui.painter_at(rect);
    for pair in screen.windows(2) {
        if let [Some(a), Some(b)] = pair {
            painter.line_segment(
                [*a, *b],
                egui::Stroke::new(2.0, crate::theme::tokens(world).overlay.selection_outline),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review sprint-w12-rev-g8 #3: Region mode on and off leaves the sculpt tool as it found it.
    #[test]
    fn region_mode_off_puts_the_sculpt_tool_back() {
        let mut world = World::new();
        world.insert_resource(TerrainTool::default());
        let mut state = TerrainRegion {
            active: true,
            ..TerrainRegion::default()
        };
        apply_mode_change(&mut world, &mut state);
        assert!(
            world.resource::<TerrainTool>().active,
            "Region drags run through the tool"
        );
        state.active = false;
        apply_mode_change(&mut world, &mut state);
        assert!(
            !world.resource::<TerrainTool>().active,
            "off again: the next drag picks, not sculpts"
        );
        // A tool that was on stays on.
        world.resource_mut::<TerrainTool>().active = true;
        state.active = true;
        apply_mode_change(&mut world, &mut state);
        state.active = false;
        apply_mode_change(&mut world, &mut state);
        assert!(world.resource::<TerrainTool>().active);
    }
    use regia_terrain::brush::StrokeTarget;

    fn fixture() -> (World, Entity) {
        let mut world = World::new();
        let hf = regia_asset::Heightfield {
            width: 9,
            height: 9,
            samples: (0..81).map(|i| i * 311).collect(),
        };
        let terrain = Terrain::new(
            &hf,
            8.0,
            100.0,
            -20.0,
            regia_terrain::CdlodConfig::default(),
            2,
            regia_terrain::TerrainLayers::default(),
        )
        .expect("terrain");
        let paint = TerrainPaint(regia_terrain::LayerWeightMap::new(9, 9, 2).expect("weights"));
        let entity = world.spawn((terrain, paint)).id();
        world.init_resource::<UndoStack>();
        (world, entity)
    }

    fn heights(world: &World, entity: Entity) -> Vec<u32> {
        world
            .get::<Terrain>(entity)
            .expect("terrain")
            .map
            .heights()
            .iter()
            .map(|h| h.to_bits())
            .collect()
    }

    #[test]
    fn terrain_region_non_square_rotations_copy_bits_and_all_weights_as_one_undo() {
        let indices = [
            vec![0, 1, 2, 3, 4, 5],
            vec![3, 0, 4, 1, 5, 2],
            vec![5, 4, 3, 2, 1, 0],
            vec![2, 5, 1, 4, 0, 3],
        ];
        let values = [-0.0, f32::from_bits(0x3f80_0001), 2.5, -3.25, 4.125, 5.75];
        for (turn, expected) in indices.into_iter().enumerate() {
            let (mut world, e) = fixture();
            for (i, &value) in values.iter().enumerate() {
                let (x, y) = (1 + i as u32 % 3, 1 + i as u32 / 3);
                world
                    .get_mut::<Terrain>(e)
                    .expect("terrain")
                    .map
                    .write(IVec2::new(x as i32, y as i32), value);
                for layer in 0..2 {
                    world.get_mut::<TerrainPaint>(e).expect("paint").0.set(
                        layer,
                        x,
                        y,
                        (i * 29 + layer as usize * 71) as u8,
                    );
                }
            }
            let before = heights(&world, e);
            let weights_before = world.get::<TerrainPaint>(e).expect("paint").clone();
            operate(&mut world, &json!({"op":"copy","rect":[1,1,3,2]})).expect("copy");
            assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
            // The 90-degree destination overlaps the source: it must use the clipboard snapshot.
            operate(&mut world, &json!({"op":"paste","at":[2,2],"rotation":turn*90})).expect("paste");
            let width = if turn.is_multiple_of(2) { 3 } else { 2 };
            for (i, index) in expected.into_iter().enumerate() {
                let (x, y) = (2 + i as u32 % width, 2 + i as u32 / width);
                assert_eq!(
                    world
                        .get::<Terrain>(e)
                        .expect("terrain")
                        .map
                        .height_at_sample(x.into(), y.into())
                        .to_bits(),
                    values[index].to_bits()
                );
                for layer in 0..2 {
                    assert_eq!(
                        world.get::<TerrainPaint>(e).expect("paint").0.get(layer, x, y),
                        (index * 29 + layer as usize * 71) as u8
                    );
                }
            }
            let after = heights(&world, e);
            let weights_after = world.get::<TerrainPaint>(e).expect("paint").clone();
            assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
            assert!(crate::command::undo_world(&mut world));
            assert_eq!(heights(&world, e), before);
            assert_eq!(world.get::<TerrainPaint>(e), Some(&weights_before));
            assert!(crate::command::redo_world(&mut world));
            assert_eq!(heights(&world, e), after);
            assert_eq!(world.get::<TerrainPaint>(e), Some(&weights_after));
            assert!(
                world
                    .get::<regia_terrain::TerrainEdits>(e)
                    .expect("edits")
                    .unsaved_paint()
            );
        }
    }

    #[test]
    fn terrain_region_offset_and_validation_are_atomic() {
        let (mut world, e) = fixture();
        operate(&mut world, &json!({"op":"copy","rect":[1,1,3,2]})).expect("copy");
        let before = heights(&world, e);
        for params in [
            json!({"op":"paste","at":[8,8]}),
            json!({"op":"paste","rotation":45}),
            json!({"op":"paste","height_offset":1e300}),
            json!({"op":"copy","rect":[0,0,4294967295u32,4]}),
            json!({"op":"copy","rect":[0,0,0,4]}),
        ] {
            assert!(operate(&mut world, &params).is_err(), "{params}");
            assert_eq!(heights(&world, e), before);
            assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
        }
        let source = world
            .get::<Terrain>(e)
            .expect("terrain")
            .map
            .height_at_sample(1, 1);
        operate(&mut world, &json!({"op":"paste","at":[4,4],"height_offset":2.25})).expect("paste");
        assert_eq!(
            world
                .get::<Terrain>(e)
                .expect("terrain")
                .map
                .height_at_sample(4, 4),
            source + 2.25
        );
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(heights(&world, e), before);
    }

    #[test]
    fn terrain_region_noop_and_layer_mismatch_do_not_change_history() {
        let (mut world, entity) = fixture();
        operate(&mut world, &json!({"op":"copy","rect":[1,1,3,2]})).expect("copy");
        let out = operate(&mut world, &json!({"op":"paste","at":[1,1]})).expect("no-op paste");
        assert_eq!(out["changed"], 0);
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
        let before = heights(&world, entity);
        world.entity_mut(entity).remove::<TerrainPaint>();
        let error = handler(&mut world, &json!({"op":"paste","at":[4,4]})).expect_err("missing weights");
        assert_eq!(error.data.expect("code")["code"], "terrain_region_invalid");
        assert_eq!(heights(&world, entity), before);
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
    }

    #[test]
    fn terrain_region_interchange_preserves_weights_and_has_exact_undo() {
        let dir = regia_test_utils::unique_temp_dir("terrain_region_interchange");
        std::fs::create_dir_all(&dir).expect("directory");
        for (ext, endian) in [("png", "little"), ("r16", "little"), ("raw", "big")] {
            let (mut world, e) = fixture();
            world.insert_resource(crate::agent_ext::EditorProject {
                root: Some(dir.clone()),
            });
            let path = format!("region.{ext}");
            operate(
                &mut world,
                &json!({"op":"export","rect":[1,1,3,2],"path":path,"endian":endian}),
            )
            .expect("export");
            let source = capture(&world, e, [1, 1, 3, 2]).expect("copy");
            let before = heights(&world, e);
            let weights = world.get::<TerrainPaint>(e).expect("paint").clone();
            operate(
                &mut world,
                &json!({"op":"import","rect":[4,4,3,2],"path":path,"endian":endian}),
            )
            .expect("import");
            assert_eq!(
                capture(&world, e, [4, 4, 3, 2]).expect("read").heights,
                source.heights
            );
            assert_eq!(world.get::<TerrainPaint>(e), Some(&weights));
            assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
            assert!(crate::command::undo_world(&mut world));
            assert_eq!(heights(&world, e), before);
            assert!(
                operate(
                    &mut world,
                    &json!({"op":"export","rect":[1,1,3,2],"path":"../escape.r16"})
                )
                .is_err()
            );
        }
        std::fs::remove_dir_all(dir).expect("cleanup");
    }
    #[test]
    fn terrain_region_panel_keyboard_copy_and_accessible_names() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        let (mut world, _) = fixture();
        let ctx = egui::Context::default();
        let mut draw = |ui: &mut egui::Ui| {
            egui::CentralPanel::default().show(ui, |ui| panel(ui, &mut world));
        };
        let tree = frame(&ctx, [1000.0, 1000.0], vec![], &mut draw);
        assert!(
            crate::a11y_audit::unnamed_widgets(&tree).is_empty(),
            "{:?}",
            crate::a11y_audit::widget_names(&tree)
        );
        tab_to(&ctx, [1000.0, 1000.0], "Copy region", false, 40, &mut draw);
        frame(
            &ctx,
            [1000.0, 1000.0],
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
        assert!(
            world.resource::<TerrainRegion>().clipboard.is_some(),
            "Enter copies the region"
        );
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
    }
}
