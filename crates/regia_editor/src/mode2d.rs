// SPDX-License-Identifier: MPL-2.0
//! The editor's 2D mode (`docs/specs/86-2d-toolkit.md` § 6, ADR-1889): paint tiles and
//! auto-tiled terrains onto a `.tilemap.ron` in the front orthographic view.
//!
//! The mode edits the map **document** (the `.tilemap.ron` source file, loaded into
//! [`Mode2d::doc`]), not the cooked copy a running game draws: [`save_tilemap`] writes it back
//! atomically and the asset watcher re-cooks it. A stroke (left button press → release, or one
//! `editor.tile_stroke` call) is **one** [`TilePaintCommand`] on the [`UndoStack`]; undo restores
//! the cells bit for bit. The brush is a raw tile, a terrain of the loaded `.autotile.ron` rule set
//! (painted cells and their neighbours re-resolve, `regia_2d::autotile`) or erase; Shift erases
//! while held. A tile brush or an erase paints every cell on the line between two pointer samples.
//! A stamp lands once on each new cell and does not fill that line.
//!
//! The front view looks down −Z (+X right, +Y up): the 2D world plane. The map's origin is its
//! bottom-left corner, taken from a `regia_2d::TileMap` entity that shows this file (or set with
//! `origin`). The viewport draws the document's tiles ([`tiles`]), the map bounds, the tile grid
//! (when a tile spans at least 4 px), the hover cell and a preview of the tile brush.
//!
//! Hotkeys with the viewport focused: `B` toggles the mode (and frames the map in the front
//! pane); with it on, `,` / `.` previous / next tile layer, `[` / `]` previous / next terrain. The
//! 2D tab ([`panel`]) opens maps and rule sets, and picks the layer and the brush.
//!
//! Agent methods (`book/agents/editor-2d.md`): `editor.mode2d_get`, `editor.mode2d_set`,
//! `editor.tile_stroke`, `editor.tilemap_save`; new maps and tileset binding in [`tilemap_new`],
//! the `.autotile.ron` rule editor in [`autotile_editor`] (ADR-7742). Errors carry
//! `data.mode2d_error`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_2d::autotile::{AutoTileSet, TerrainBrush, TileChange, apply_changes, revert_changes};
use regia_2d::stamp::{Stamp, StampOp};
use regia_2d::tilemap::{
    FLIP_DIAGONAL_BIT, FLIP_X_BIT, FLIP_Y_BIT, GID_MASK, MapLayer, TileMapAsset, split_gid,
};
use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::{Vec2, Vec3};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

pub use clock::AnimClock;

use crate::command::{EditorCommand, UndoStack};
use crate::overlays::OverlayShape;
use crate::viewport::EditorCamera;
use crate::viewport_agent::{Handler, str_param};
use crate::views::{ViewCamera, ViewKind, ViewportLayout};

pub mod autotile_editor;
pub mod clock;
pub mod guides;
pub mod layers;
pub mod object_drag;
pub mod panel;
pub mod patterns;
pub mod tilemap_new;
pub mod tiles;
pub mod tools;

pub use tools::{AutotileMode, RandomBrush, RandomTile, Tool2d};

/// Largest `.tilemap.ron` / `.autotile.ron` the mode loads (bytes).
const MAX_SOURCE_BYTES: u64 = 256 << 20;
/// Most cells one `editor.tile_stroke` call may paint.
const MAX_STROKE_CELLS: usize = 65_536;
/// Smallest on-screen tile (pixels) that still gets grid lines.
const GRID_MIN_TILE_PX: f32 = 4.0;
/// Most grid lines drawn per axis.
const MAX_GRID_LINES: u32 = 1024;
/// Most cells of an isometric, staggered or hexagonal map whose grid is outlined cell by cell.
const MAX_OUTLINE_CELLS: u64 = 16_384;
/// Most cells of a selection or stamp footprint outlined cell by cell.
const MAX_FOOTPRINT_CELLS: u64 = 1024;

/// 2D-mode errors; [`Mode2dError::code`] is the stable `data.mode2d_error` code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Mode2dError {
    /// A parameter is missing or malformed.
    #[error("{0}")]
    BadParams(String),
    /// No map is loaded.
    #[error("no map is loaded in the 2D mode")]
    NoMap,
    /// The brush is a terrain but no rule set is loaded.
    #[error("no auto-tiling rule set is loaded")]
    NoRules,
    /// The rule set has no terrain of that name.
    #[error("the rule set has no terrain `{0}`")]
    UnknownTerrain(String),
    /// The layer is not a tile layer.
    #[error("{0}")]
    BadLayer(String),
    /// A cell is outside the map.
    #[error("cell ({0}, {1}) is outside the map")]
    OutsideMap(u32, u32),
    /// No tileset holds the gid, or it carries flag bits.
    #[error("{0}")]
    BadGid(String),
    /// Only `.tilemap.ron` sources are editable.
    #[error("`{0}` is not a .tilemap.ron (Tiled sources are edited in Tiled)")]
    NotTilemapRon(String),
    /// The map could not be read or parsed.
    #[error("cannot load the map: {0}")]
    Load(String),
    /// The rule set could not be read, parsed or bound to the map.
    #[error("cannot use the rule set: {0}")]
    Rules(String),
    /// The map could not be written.
    #[error("cannot save the map: {0}")]
    Save(String),
    /// The loaded map has unsaved edits.
    #[error("the loaded map has unsaved edits (save it, or pass `discard: true`)")]
    Unsaved,
    /// The stamp rectangle, size or tiles are not usable.
    #[error("{0}")]
    Stamp(String),
    /// The stamp brush is selected (or a transform asked for) but nothing was copied yet.
    #[error("no stamp: copy a rectangle of cells first (editor.tile_stamp action copy)")]
    NoStamp,
    /// No object with that id on the layer.
    #[error("{0}")]
    NoObject(String),
    /// The tileset has no saved pattern of that name.
    #[error("no saved pattern `{0}`")]
    NoPattern(String),
    /// The tileset's `.tilepatterns.ron` could not be read, written or edited.
    #[error("tile patterns: {0}")]
    Patterns(String),
}

impl Mode2dError {
    /// The stable error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadParams(_) => "mode2d_bad_params",
            Self::NoMap => "mode2d_no_map",
            Self::NoRules => "mode2d_no_rules",
            Self::UnknownTerrain(_) => "mode2d_unknown_terrain",
            Self::BadLayer(_) => "mode2d_bad_layer",
            Self::OutsideMap(..) => "mode2d_outside_map",
            Self::BadGid(_) => "mode2d_bad_gid",
            Self::NotTilemapRon(_) => "mode2d_not_tilemap_ron",
            Self::Load(_) => "mode2d_load_failed",
            Self::Rules(_) => "mode2d_bad_rules",
            Self::Save(_) => "mode2d_save_failed",
            Self::Unsaved => "mode2d_unsaved",
            Self::Stamp(_) => "mode2d_bad_stamp",
            Self::NoStamp => "mode2d_no_stamp",
            Self::NoObject(_) => "mode2d_no_object",
            Self::NoPattern(_) => "mode2d_no_pattern",
            Self::Patterns(_) => "mode2d_bad_patterns",
        }
    }
}

/// Two cells: a line's ends or a rectangle's corners.
type CellPair = ((u32, u32), (u32, u32));

/// What a stroke paints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Brush2d {
    /// A raw GID (with flag bits).
    Tile(u32),
    /// A terrain of the loaded rule set, by name.
    Terrain(String),
    /// Empty cells (neighbours re-resolve when a rule set is loaded).
    Erase,
    /// The copied stamp ([`Mode2d::stamp`]), pasted centered on each painted cell.
    Stamp,
}

/// The map document being edited.
#[derive(Debug, Clone)]
pub struct TileDocument {
    /// The `.tilemap.ron` source file.
    pub path: PathBuf,
    /// The map.
    pub map: TileMapAsset,
    /// Edited since load or the last save.
    pub dirty: bool,
    /// The tileset images, one per tileset of the map.
    pub images: Vec<tiles::TilesetImage>,
}

/// The rule set the terrain brush uses.
#[derive(Debug, Clone)]
pub struct RulesDocument {
    /// The `.autotile.ron` file.
    pub path: PathBuf,
    /// The rules.
    pub set: AutoTileSet,
}

#[derive(Debug, Default)]
struct Stroke {
    /// `(y, x)` → `(before, after)`; the first `before` and the last `after` win.
    cells: BTreeMap<(u32, u32), (u32, u32)>,
    last: Option<(u32, u32)>,
    erase: bool,
    /// The layer it paints: a layer change during a drag does not split the stroke.
    layer: usize,
    /// The tool it was started with (a tool change during a drag does not change it).
    tool: Tool2d,
    /// The cells the stroke painted itself (path mode joins only these).
    painted: BTreeSet<(u32, u32)>,
    /// Line and rectangle: the pressed cell and the current end cell.
    anchor: Option<(u32, u32)>,
    end: Option<(u32, u32)>,
}

/// The 2D mode's state (a world resource).
#[derive(Resource, Debug)]
pub struct Mode2d {
    /// The mode is on: left-drag in the front pane paints instead of box-selecting.
    pub active: bool,
    /// The map being edited.
    pub doc: Option<TileDocument>,
    /// The rule set of the terrain brush.
    pub rules: Option<RulesDocument>,
    /// Index of the tile layer strokes paint into.
    pub layer: usize,
    /// The brush.
    pub brush: Brush2d,
    /// Draw the tile grid.
    pub grid: bool,
    /// World position of the map's bottom-left corner.
    pub origin: Vec3,
    /// The cell under the pointer.
    pub hover: Option<(u32, u32)>,
    /// Frame the map in the front pane on the next viewport frame.
    pub pending_frame: bool,
    /// Bring the 2D tab to the front on the next shell frame.
    pub focus_panel: bool,
    /// Moves when the document's images change; [`tiles::TileTextures`] follows it.
    pub image_generation: u64,
    /// Tiles the front pane drew last frame, and the cell stride of that view.
    pub view: (usize, u32),
    /// The copied stamp, if any ([`Brush2d::Stamp`] pastes it).
    pub stamp: Option<Stamp>,
    /// The rectangle being selected (Ctrl-drag): anchor and current cell.
    pub select: Option<((u32, u32), (u32, u32))>,
    /// The animation clock of animated tiles (play/pause per pane).
    pub clock: AnimClock,
    /// The paint tool (freehand, line, rectangle, bucket, pick).
    pub tool: Tool2d,
    /// The rectangle tool fills (else it draws the outline).
    pub rect_filled: bool,
    /// The bucket fills only the connected region (else every matching cell of the layer).
    pub bucket_contiguous: bool,
    /// The random scatter brush: when set, a tile brush paints one of its tiles per cell.
    pub random: Option<RandomBrush>,
    /// How a terrain brush joins its neighbors (connect or path).
    pub autotile_mode: AutotileMode,
    /// The object layer the layer tree edits (`None`: a tile layer is the target).
    pub object_layer: Option<usize>,
    /// The selected object of [`Self::object_layer`], by id.
    pub selected_object: Option<u32>,
    /// The 2D view's rulers and guides (editor view state).
    pub guides: guides::Guides2d,
    /// A guide being dragged out of a ruler or along the pane: its index.
    pub dragging_guide: Option<usize>,
    /// The saved patterns of the tileset last asked for (its `.tilepatterns.ron`).
    pub patterns: Option<patterns::PatternDoc>,
    /// An object being dragged in the pane.
    pub object_drag: Option<object_drag::ObjectDrag>,
    stroke: Option<Stroke>,
}

impl Default for Mode2d {
    fn default() -> Self {
        Self {
            active: false,
            doc: None,
            rules: None,
            layer: 0,
            brush: Brush2d::Tile(1),
            grid: true,
            origin: Vec3::ZERO,
            hover: None,
            pending_frame: false,
            focus_panel: false,
            image_generation: 0,
            view: (0, 1),
            stamp: None,
            select: None,
            clock: AnimClock::default(),
            tool: Tool2d::Paint,
            rect_filled: true,
            bucket_contiguous: true,
            random: None,
            autotile_mode: AutotileMode::Connect,
            object_layer: None,
            selected_object: None,
            guides: guides::Guides2d::default(),
            dragging_guide: None,
            patterns: None,
            object_drag: None,
            stroke: None,
        }
    }
}

fn read_limited(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.len() > MAX_SOURCE_BYTES {
        return Err(format!(
            "{} is larger than {MAX_SOURCE_BYTES} bytes",
            path.display()
        ));
    }
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn is_tilemap_ron(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_ascii_lowercase().ends_with(".tilemap.ron"))
}

impl Mode2d {
    /// Loads `path` (a `.tilemap.ron`) as the document. Refuses to drop unsaved edits unless
    /// `discard`.
    pub fn load_map(&mut self, path: &Path, discard: bool) -> Result<(), Mode2dError> {
        if !is_tilemap_ron(path) {
            return Err(Mode2dError::NotTilemapRon(path.display().to_string()));
        }
        if self.doc.as_ref().is_some_and(|d| d.dirty) && !discard {
            return Err(Mode2dError::Unsaved);
        }
        let text = read_limited(path).map_err(Mode2dError::Load)?;
        let map = TileMapAsset::from_ron(&text).map_err(|e| Mode2dError::Load(e.to_string()))?;
        self.layer = map
            .layers
            .iter()
            .position(|l| matches!(l, MapLayer::Tiles(_)))
            .unwrap_or(0);
        let images = tiles::load_images(&map, path);
        self.doc = Some(TileDocument {
            path: path.to_path_buf(),
            map,
            dirty: false,
            images,
        });
        self.image_generation += 1;
        self.stroke = None;
        self.object_layer = None;
        self.selected_object = None;
        self.patterns = None;
        self.object_drag = None;
        if let Some(rules) = &self.rules
            && let Some(doc) = &self.doc
            && let Err(e) = rules.set.bind(&doc.map)
        {
            tracing::warn!(target: "regia_editor::mode2d", "rule set {} does not fit the map: {e}", rules.path.display());
        }
        Ok(())
    }

    /// Decodes the document's tileset images again (they changed on disk).
    pub fn reload_images(&mut self) {
        if let Some(doc) = self.doc.as_mut() {
            doc.images = tiles::load_images(&doc.map, &doc.path);
            self.image_generation += 1;
        }
    }

    /// Loads `path` (an `.autotile.ron`) as the rule set; it must fit the loaded map.
    pub fn load_rules(&mut self, path: &Path) -> Result<(), Mode2dError> {
        let text = read_limited(path).map_err(Mode2dError::Rules)?;
        let set = AutoTileSet::from_ron(&text).map_err(|e| Mode2dError::Rules(e.to_string()))?;
        if let Some(doc) = &self.doc {
            set.bind(&doc.map)
                .map_err(|e| Mode2dError::Rules(e.to_string()))?;
        }
        if let Brush2d::Terrain(name) = &self.brush
            && set.terrain_index(name).is_none()
        {
            self.brush = Brush2d::Terrain(set.terrains[0].name.clone());
        }
        self.rules = Some(RulesDocument {
            path: path.to_path_buf(),
            set,
        });
        Ok(())
    }

    /// Selects tile layer `layer` (index) of the document.
    pub fn set_layer(&mut self, layer: usize) -> Result<(), Mode2dError> {
        let doc = self.doc.as_ref().ok_or(Mode2dError::NoMap)?;
        match doc.map.layers.get(layer) {
            Some(MapLayer::Tiles(_)) => {
                self.layer = layer;
                Ok(())
            }
            _ => Err(Mode2dError::BadLayer(format!(
                "layer {layer} is not a tile layer"
            ))),
        }
    }

    /// Steps to the previous (`-1`) or next (`+1`) tile layer, wrapping.
    pub fn cycle_layer(&mut self, step: i64) {
        let Some(doc) = &self.doc else { return };
        let tiles: Vec<usize> = (0..doc.map.layers.len())
            .filter(|&l| matches!(doc.map.layers[l], MapLayer::Tiles(_)))
            .collect();
        if tiles.is_empty() {
            return;
        }
        let at = tiles.iter().position(|&l| l == self.layer).unwrap_or(0) as i64;
        let next = (at + step).rem_euclid(tiles.len() as i64) as usize;
        self.layer = tiles[next];
    }

    /// Steps the terrain brush to the previous / next terrain of the rule set, wrapping.
    pub fn cycle_terrain(&mut self, step: i64) {
        let Some(rules) = &self.rules else { return };
        let count = rules.set.terrains.len() as i64;
        let at = match &self.brush {
            Brush2d::Terrain(name) => rules.set.terrain_index(name).map_or(0, |i| i as i64 + step),
            _ => 0,
        };
        self.brush = Brush2d::Terrain(rules.set.terrains[at.rem_euclid(count) as usize].name.clone());
    }

    /// Paints `cells` with the brush (or erase) into the document, without undo bookkeeping.
    /// Every cell is checked first. Returns the changed cells.
    pub fn paint_now(&mut self, cells: &[(u32, u32)], erase: bool) -> Result<Vec<TileChange>, Mode2dError> {
        self.paint_layer(self.layer, cells, erase)
    }

    fn paint_layer(
        &mut self,
        layer: usize,
        cells: &[(u32, u32)],
        erase: bool,
    ) -> Result<Vec<TileChange>, Mode2dError> {
        self.paint_with(layer, cells, erase, false, None)
    }

    /// [`Self::paint_now`] with the tool context: `pattern` tiles the stamp over the cells (line,
    /// rectangle and bucket) instead of pasting it centered on each; `joined` is the path-mode
    /// stroke so far (`None`: just `cells`).
    fn paint_with(
        &mut self,
        layer: usize,
        cells: &[(u32, u32)],
        erase: bool,
        pattern: bool,
        joined: Option<&BTreeSet<(u32, u32)>>,
    ) -> Result<Vec<TileChange>, Mode2dError> {
        let doc = self.doc.as_mut().ok_or(Mode2dError::NoMap)?;
        let map = &mut doc.map;
        if !matches!(map.layers.get(layer), Some(MapLayer::Tiles(_))) {
            return Err(Mode2dError::BadLayer(format!(
                "layer {layer} is not a tile layer"
            )));
        }
        if let Some(&(x, y)) = cells.iter().find(|(x, y)| *x >= map.width || *y >= map.height) {
            return Err(Mode2dError::OutsideMap(x, y));
        }
        let brush = if erase { Brush2d::Erase } else { self.brush.clone() };
        let changes = match (&brush, &self.rules) {
            (Brush2d::Terrain(name), Some(rules)) if self.autotile_mode == AutotileMode::Path => {
                let index = rules
                    .set
                    .terrain_index(name)
                    .ok_or_else(|| Mode2dError::UnknownTerrain(name.clone()))?;
                let bound = rules
                    .set
                    .bind(map)
                    .map_err(|e| Mode2dError::Rules(e.to_string()))?;
                let own: BTreeSet<(u32, u32)>;
                let joined = match joined {
                    Some(set) => set,
                    None => {
                        own = cells.iter().copied().collect();
                        &own
                    }
                };
                tools::paint_path(&bound, map, layer, index, cells, joined).map_err(Mode2dError::Rules)?
            }
            (Brush2d::Terrain(name), Some(rules)) => {
                let index = rules
                    .set
                    .terrain_index(name)
                    .ok_or_else(|| Mode2dError::UnknownTerrain(name.clone()))?;
                let bound = rules
                    .set
                    .bind(map)
                    .map_err(|e| Mode2dError::Rules(e.to_string()))?;
                bound
                    .paint(map, layer, TerrainBrush::Terrain(index), cells)
                    .map_err(|e| Mode2dError::Rules(e.to_string()))?
            }
            (Brush2d::Terrain(_), None) => return Err(Mode2dError::NoRules),
            (Brush2d::Erase, Some(rules)) => {
                let bound = rules
                    .set
                    .bind(map)
                    .map_err(|e| Mode2dError::Rules(e.to_string()))?;
                bound
                    .paint(map, layer, TerrainBrush::Erase, cells)
                    .map_err(|e| Mode2dError::Rules(e.to_string()))?
            }
            (Brush2d::Erase, None) => write_raw(map, layer, cells, 0),
            (Brush2d::Stamp, _) if pattern => {
                // Line, rectangle and bucket tile the stamp over the cells, aligned to the map.
                let stamp = self.stamp.as_ref().ok_or(Mode2dError::NoStamp)?;
                stamp
                    .check(map, layer)
                    .map_err(|e| Mode2dError::Stamp(e.to_string()))?;
                let values: Vec<((u32, u32), u32)> = cells
                    .iter()
                    .filter_map(|&(x, y)| {
                        stamp
                            .get(x % stamp.width, y % stamp.height)
                            .filter(|&raw| raw & GID_MASK != 0)
                            .map(|raw| ((x, y), raw))
                    })
                    .collect();
                tools::write_values(map, layer, &values)
            }
            (Brush2d::Stamp, _) => {
                let stamp = self.stamp.as_ref().ok_or(Mode2dError::NoStamp)?;
                // Each painted cell is the stamp's center; later pastes win where they overlap.
                let mut merged: BTreeMap<(u32, u32), TileChange> = BTreeMap::new();
                for &(x, y) in cells {
                    let at = (
                        i64::from(x) - i64::from(stamp.width / 2),
                        i64::from(y) - i64::from(stamp.height / 2),
                    );
                    for c in stamp
                        .paste(map, layer, at, true)
                        .map_err(|e| Mode2dError::Stamp(e.to_string()))?
                    {
                        merged
                            .entry((c.y, c.x))
                            .and_modify(|e| e.after = c.after)
                            .or_insert(c);
                    }
                }
                merged.into_values().filter(|c| c.before != c.after).collect()
            }
            (Brush2d::Tile(_), _) if self.random.is_some() => {
                let random = self.random.as_ref().ok_or(Mode2dError::NoMap)?;
                random.validate().map_err(Mode2dError::BadParams)?;
                if let Some(t) = random
                    .tiles
                    .iter()
                    .find(|t| map.tileset_for(t.raw & GID_MASK).is_none())
                {
                    return Err(Mode2dError::BadGid(format!(
                        "no tileset holds gid {}",
                        t.raw & GID_MASK
                    )));
                }
                let values: Vec<((u32, u32), u32)> = cells
                    .iter()
                    .filter_map(|&(x, y)| random.pick(x, y).map(|raw| ((x, y), raw)))
                    .collect();
                tools::write_values(map, layer, &values)
            }
            (Brush2d::Tile(raw), _) => {
                let gid = raw & GID_MASK;
                if gid != 0 && map.tileset_for(gid).is_none() {
                    return Err(Mode2dError::BadGid(format!("no tileset holds gid {gid}")));
                }
                write_raw(map, layer, cells, *raw)
            }
        };
        if !changes.is_empty() {
            doc.dirty = true;
        }
        Ok(changes)
    }

    /// Copies the cells between `a` and `b` (corners in any order) of the active layer into the
    /// stamp and selects the stamp brush.
    pub fn copy_stamp(&mut self, a: (u32, u32), b: (u32, u32)) -> Result<(), Mode2dError> {
        let doc = self.doc.as_ref().ok_or(Mode2dError::NoMap)?;
        let stamp = Stamp::copy(&doc.map, self.layer, a, b).map_err(|e| Mode2dError::Stamp(e.to_string()))?;
        self.stamp = Some(stamp);
        self.brush = Brush2d::Stamp;
        Ok(())
    }

    /// Rotates or mirrors the stamp (its tiles' flip bits turn with it).
    pub fn transform_stamp(&mut self, op: StampOp) -> Result<(), Mode2dError> {
        let stamp = self.stamp.as_ref().ok_or(Mode2dError::NoStamp)?;
        self.stamp = Some(stamp.transformed(op));
        Ok(())
    }

    /// Drops the stamp; the stamp brush falls back to tile 1.
    pub fn clear_stamp(&mut self) {
        self.stamp = None;
        if self.brush == Brush2d::Stamp {
            self.brush = Brush2d::Tile(1);
        }
    }

    /// Starts a stroke (pointer pressed) with the current tool.
    pub fn begin_stroke(&mut self, erase: bool) {
        self.begin_stroke_as(erase, self.tool);
    }

    /// Starts a stroke with `tool` (a right-click erase uses freehand or bucket).
    pub fn begin_stroke_as(&mut self, erase: bool, tool: Tool2d) {
        self.stroke = Some(Stroke {
            erase,
            layer: self.layer,
            tool,
            ..Stroke::default()
        });
    }

    /// Picks the tile of `cell` on the active layer as the brush (an empty cell picks erase).
    /// The random brush turns off.
    pub fn pick_cell(&mut self, cell: (u32, u32)) -> Result<(), Mode2dError> {
        let doc = self.doc.as_ref().ok_or(Mode2dError::NoMap)?;
        let map = &doc.map;
        if cell.0 >= map.width || cell.1 >= map.height {
            return Err(Mode2dError::OutsideMap(cell.0, cell.1));
        }
        let Some(MapLayer::Tiles(tiles)) = map.layers.get(self.layer) else {
            return Err(Mode2dError::BadLayer(format!(
                "layer {} is not a tile layer",
                self.layer
            )));
        };
        let raw = tiles
            .data
            .get(cell.1 as usize * map.width as usize + cell.0 as usize)
            .copied()
            .unwrap_or(0);
        self.brush = if raw & GID_MASK == 0 {
            Brush2d::Erase
        } else {
            Brush2d::Tile(raw)
        };
        self.random = None;
        Ok(())
    }

    /// The cells a one-shot `tool` call covers: `paint` takes `cells` as they are, `line` and
    /// `rect` take two cells (the ends or corners), `bucket` one cell (the region of it). `pick`
    /// paints nothing.
    pub fn tool_cells(&self, tool: Tool2d, cells: &[(u32, u32)]) -> Result<Vec<(u32, u32)>, Mode2dError> {
        let doc = self.doc.as_ref().ok_or(Mode2dError::NoMap)?;
        let map = &doc.map;
        if let Some(&(x, y)) = cells.iter().find(|(x, y)| *x >= map.width || *y >= map.height) {
            return Err(Mode2dError::OutsideMap(x, y));
        }
        let two = |what: &str| -> Result<CellPair, Mode2dError> {
            match cells {
                [a, b] => Ok((*a, *b)),
                _ => Err(Mode2dError::BadParams(format!("`{what}` takes two cells"))),
            }
        };
        match tool {
            Tool2d::Paint => Ok(cells.to_vec()),
            Tool2d::Line => {
                let (a, b) = two("line")?;
                Ok(line_cells(a, b))
            }
            Tool2d::Rect => {
                let (a, b) = two("rect")?;
                tools::rect_cells(a, b, self.rect_filled).ok_or_else(|| {
                    Mode2dError::BadParams(format!(
                        "the rectangle is larger than {} cells",
                        tools::MAX_TOOL_CELLS
                    ))
                })
            }
            Tool2d::Bucket => match cells {
                [start] => {
                    tools::bucket_cells(map, self.layer, *start, self.bucket_contiguous).ok_or_else(|| {
                        Mode2dError::BadParams(format!(
                            "the bucket region is larger than {} cells or the layer is not a tile layer",
                            tools::MAX_TOOL_CELLS
                        ))
                    })
                }
                _ => Err(Mode2dError::BadParams("`bucket` takes one cell".into())),
            },
            Tool2d::Pick => Ok(Vec::new()),
        }
    }

    /// Paints `cells` (already expanded by [`Self::tool_cells`]) as one stroke of `tool`.
    pub fn paint_tool_now(
        &mut self,
        tool: Tool2d,
        cells: &[(u32, u32)],
        erase: bool,
    ) -> Result<Vec<TileChange>, Mode2dError> {
        let pattern = tool != Tool2d::Paint;
        self.paint_with(self.layer, cells, erase, pattern, None)
    }

    /// Extends the stroke to `cell`. A tile brush or an erase paints every cell on the line from
    /// the previous sample. A stamp paints only `cell`.
    pub fn stroke_to(&mut self, cell: (u32, u32)) -> Result<usize, Mode2dError> {
        let Some(stroke) = self.stroke.as_ref() else {
            return Ok(0);
        };
        if stroke.last == Some(cell) {
            return Ok(0);
        }
        let (erase, layer, tool) = (stroke.erase, stroke.layer, stroke.tool);
        if tool.is_shape() || tool == Tool2d::Pick {
            // The shape lands on release; until then the pointer only moves its end.
            if let Some(stroke) = self.stroke.as_mut() {
                stroke.anchor.get_or_insert(cell);
                stroke.end = Some(cell);
                stroke.last = Some(cell);
            }
            return Ok(0);
        }
        let cells = match (tool, stroke.last) {
            // The bucket fills once, at the pressed cell.
            (Tool2d::Bucket, Some(_)) => return Ok(0),
            (Tool2d::Bucket, None) => self.tool_cells(Tool2d::Bucket, &[cell])?,
            // A stamp lands once per new cell (not on every cell of the line between samples).
            (_, Some(from)) if self.brush != Brush2d::Stamp || stroke.erase => line_cells(from, cell),
            _ => vec![cell],
        };
        let mut joined = stroke.painted.clone();
        joined.extend(cells.iter().copied());
        let changes = self.paint_with(layer, &cells, erase, tool == Tool2d::Bucket, Some(&joined))?;
        if let Some(stroke) = self.stroke.as_mut() {
            stroke.painted = joined;
            stroke.last = Some(cell);
            for c in &changes {
                stroke
                    .cells
                    .entry((c.y, c.x))
                    .and_modify(|e| e.1 = c.after)
                    .or_insert((c.before, c.after));
            }
        }
        Ok(changes.len())
    }

    /// Ends the stroke; the command to push when anything changed. A line or rectangle paints
    /// its cells here, from the pressed cell to the last one.
    pub fn end_stroke(&mut self) -> Option<TilePaintCommand> {
        let mut stroke = self.stroke.take()?;
        if let (true, Some(a), Some(b)) = (stroke.tool.is_shape(), stroke.anchor, stroke.end) {
            let cells = if stroke.tool == Tool2d::Line {
                line_cells(a, b)
            } else {
                tools::rect_cells(a, b, self.rect_filled).unwrap_or_else(|| {
                    tracing::warn!(
                        target: "regia_editor::mode2d",
                        "tile rect: larger than {} cells; nothing painted",
                        tools::MAX_TOOL_CELLS
                    );
                    Vec::new()
                })
            };
            let joined: BTreeSet<(u32, u32)> = cells.iter().copied().collect();
            match self.paint_with(stroke.layer, &cells, stroke.erase, true, Some(&joined)) {
                Ok(changes) => {
                    for c in changes {
                        stroke
                            .cells
                            .entry((c.y, c.x))
                            .and_modify(|e| e.1 = c.after)
                            .or_insert((c.before, c.after));
                    }
                }
                Err(e) => tracing::warn!(target: "regia_editor::mode2d", "tile {}: {e}", stroke.tool.name()),
            }
        }
        // The next stroke of the random brush scatters differently.
        if let Some(random) = self.random.as_mut()
            && !stroke.cells.is_empty()
        {
            random.seed = random.seed.wrapping_add(1);
        }
        let doc = self.doc.as_ref()?;
        let changes: Vec<TileChange> = stroke
            .cells
            .into_iter()
            .filter(|(_, (before, after))| before != after)
            .map(|((y, x), (before, after))| TileChange { x, y, before, after })
            .collect();
        (!changes.is_empty()).then(|| TilePaintCommand {
            path: doc.path.clone(),
            layer: stroke.layer,
            changes,
            erase: stroke.erase,
        })
    }

    /// Whether a stroke is in progress.
    #[must_use]
    pub fn stroking(&self) -> bool {
        self.stroke.is_some()
    }

    /// The cells a line or rectangle in progress would paint (the pane outlines them).
    #[must_use]
    pub fn shape_preview(&self) -> Vec<(u32, u32)> {
        match self.stroke.as_ref() {
            Some(Stroke {
                tool: Tool2d::Line,
                anchor: Some(a),
                end: Some(b),
                ..
            }) => line_cells(*a, *b),
            Some(Stroke {
                tool: Tool2d::Rect,
                anchor: Some(a),
                end: Some(b),
                ..
            }) => tools::rect_cells(*a, *b, self.rect_filled).unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// The document cell under a viewport pixel of a front pane.
    #[must_use]
    pub fn cell_at_pixel(&self, view: &ViewCamera, pixel: Vec2, size: Vec2) -> Option<(u32, u32)> {
        let doc = self.doc.as_ref()?;
        let (origin, dir) = view.ray(pixel, size);
        if dir.z.abs() < 1e-6 {
            return None;
        }
        let t = (self.origin.z - origin.z) / dir.z;
        let hit = origin + dir * t;
        doc.map
            .cell_at_world([hit.x - self.origin.x, hit.y - self.origin.y])
    }

    /// The level point (units from the map's bottom-left corner, +Y up) under a pane pixel.
    #[must_use]
    pub fn level_at_pixel(&self, view: &ViewCamera, pixel: Vec2, size: Vec2) -> Option<[f32; 2]> {
        let (origin, dir) = view.ray(pixel, size);
        if dir.z.abs() < 1e-6 {
            return None;
        }
        let t = (self.origin.z - origin.z) / dir.z;
        let hit = origin + dir * t;
        Some([hit.x - self.origin.x, hit.y - self.origin.y])
    }

    /// Points a front-pane camera at the whole map.
    pub fn frame(&self, camera: &mut EditorCamera) {
        let Some(doc) = &self.doc else { return };
        let [w, h] = doc.map.pixel_size();
        let (w, h) = (w as f32, h as f32);
        camera.target = self.origin + Vec3::new(w * 0.5, h * 0.5, 0.0);
        let half = w.max(h) * 0.55;
        camera.distance = (half / (camera.fov_deg.to_radians() * 0.5).tan()).max(1.0);
    }

    /// One-line status for the viewport.
    #[must_use]
    pub fn status(&self) -> String {
        let Some(doc) = &self.doc else {
            return "2D mode: no map (open one in the 2D tab, or B to leave)".into();
        };
        let layer = doc.map.layers.get(self.layer).map_or("?", MapLayer::name);
        let file = doc.path.file_name().and_then(|n| n.to_str()).unwrap_or("map");
        let brush = match &self.brush {
            Brush2d::Tile(_) if self.random.is_some() => format!(
                "random {} tiles",
                self.random.as_ref().map_or(0, |r| r.tiles.len())
            ),
            Brush2d::Tile(raw) => format!("tile {}", raw & GID_MASK),
            Brush2d::Terrain(name) if self.autotile_mode == AutotileMode::Path => {
                format!("terrain {name} (path)")
            }
            Brush2d::Terrain(name) => format!("terrain {name}"),
            Brush2d::Erase => "erase".into(),
            Brush2d::Stamp => self.stamp.as_ref().map_or_else(
                || "stamp (none)".into(),
                |s| format!("stamp {}x{}", s.width, s.height),
            ),
        };
        format!(
            "2D · {file}{} · layer {layer} · {} · {brush} · Shift or right-click erases",
            if doc.dirty { " *" } else { "" },
            self.tool.label().to_lowercase(),
        )
    }
}

/// Writes `raw` into `cells` of `layer`; the changed cells sorted by `(y, x)`, each once.
fn write_raw(map: &mut TileMapAsset, layer: usize, cells: &[(u32, u32)], raw: u32) -> Vec<TileChange> {
    let width = map.width as usize;
    let Some(MapLayer::Tiles(tiles)) = map.layers.get_mut(layer) else {
        return Vec::new();
    };
    let mut changed = BTreeMap::new();
    for &(x, y) in cells {
        if let Some(cell) = tiles.data.get_mut(y as usize * width + x as usize)
            && *cell != raw
        {
            changed.entry((y, x)).or_insert(*cell);
            *cell = raw;
        }
    }
    changed
        .into_iter()
        .map(|((y, x), before)| TileChange {
            x,
            y,
            before,
            after: raw,
        })
        .collect()
}

/// The cells on the line from `a` to `b` (both included), Bresenham's integer walk.
#[must_use]
pub fn line_cells(a: (u32, u32), b: (u32, u32)) -> Vec<(u32, u32)> {
    let (mut x, mut y) = (i64::from(a.0), i64::from(a.1));
    let (x1, y1) = (i64::from(b.0), i64::from(b.1));
    let (dx, dy) = ((x1 - x).abs(), -(y1 - y).abs());
    let (sx, sy) = (if x < x1 { 1 } else { -1 }, if y < y1 { 1 } else { -1 });
    let mut err = dx + dy;
    let mut out = Vec::with_capacity((dx - dy) as usize + 1);
    loop {
        out.push((x as u32, y as u32));
        if x == x1 && y == y1 {
            return out;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// One stroke on the undo stack: the changed cells of one layer of one map document.
#[derive(Debug, Clone)]
pub struct TilePaintCommand {
    /// The document's path (the command does nothing once another map is loaded).
    pub path: PathBuf,
    /// The layer.
    pub layer: usize,
    /// The changed cells.
    pub changes: Vec<TileChange>,
    /// The stroke erased.
    pub erase: bool,
}

impl TilePaintCommand {
    fn write(&self, world: &mut World, undo: bool) {
        let Some(mut mode) = world.get_resource_mut::<Mode2d>() else {
            return;
        };
        let Some(doc) = mode.doc.as_mut().filter(|d| d.path == self.path) else {
            tracing::warn!(target: "regia_editor::mode2d", "tile stroke on {} skipped: another map is loaded", self.path.display());
            return;
        };
        let result = if undo {
            revert_changes(&mut doc.map, self.layer, &self.changes)
        } else {
            apply_changes(&mut doc.map, self.layer, &self.changes)
        };
        match result {
            Ok(()) => doc.dirty = true,
            Err(e) => tracing::warn!(target: "regia_editor::mode2d", "tile stroke not applied: {e}"),
        }
    }
}

impl EditorCommand for TilePaintCommand {
    fn apply(&mut self, world: &mut World) {
        self.write(world, false);
    }

    fn revert(&mut self, world: &mut World) {
        self.write(world, true);
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        let n = self.changes.len();
        if self.erase {
            format!("Erase {n} tiles")
        } else {
            format!("Paint {n} tiles")
        }
    }
}

/// Pushes a finished stroke onto the world's undo stack (already applied) and seals it.
/// True the first time a panel asks with `id` in this egui frame, false on a later pass of the
/// same frame. egui runs a frame's UI again when a pass asks for a discard, and that pass sees the
/// same clicks and keys, so the 2D tab's action lists (palette, map cursor, tileset binding, rule
/// editor) apply once per frame: a second pass must not step a pattern cell twice, add two rules or
/// swap a tileset back (the `abilities_panel` guard, review finding 7 of ADR-7742).
pub(crate) fn first_apply_this_frame(ctx: &egui::Context, id: egui::Id) -> bool {
    let frame = ctx.cumulative_frame_nr();
    if ctx.data(|d| d.get_temp::<u64>(id)) == Some(frame) {
        return false;
    }
    ctx.data_mut(|d| d.insert_temp(id, frame));
    true
}

fn push_stroke(world: &mut World, command: TilePaintCommand) {
    push_command(world, Box::new(command));
}

/// Pushes an already applied 2D edit onto the world's undo stack and seals it.
fn push_command(world: &mut World, command: Box<dyn EditorCommand>) {
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    stack.push(command);
    stack.seal();
}

/// Writes the document to `path` (default: its own) atomically: a temporary file next to it,
/// then a rename. Clears `dirty` when it writes the document's own path. Returns the bytes written.
pub fn save_tilemap(mode: &mut Mode2d, path: Option<&Path>) -> Result<(PathBuf, usize), Mode2dError> {
    let doc = mode.doc.as_mut().ok_or(Mode2dError::NoMap)?;
    let target = path.map_or_else(|| doc.path.clone(), Path::to_path_buf);
    if !is_tilemap_ron(&target) {
        return Err(Mode2dError::NotTilemapRon(target.display().to_string()));
    }
    let text = doc.map.to_ron().map_err(|e| Mode2dError::Save(e.to_string()))?;
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Mode2dError::Save("the path has no file name".into()))?;
    let tmp = target.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text.as_bytes())
        .map_err(|e| Mode2dError::Save(format!("{}: {e}", tmp.display())))?;
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(Mode2dError::Save(format!("{}: {e}", target.display())));
    }
    if crate::path_norm::same_file(&target, &doc.path, crate::path_norm::FOLD_CASE) {
        doc.dirty = false;
    }
    Ok((target, text.len()))
}

/// Saves the 2D mode's map to its own `.tilemap.ron` when it has unsaved edits: the editor's
/// Save level (menu, save-on-close) and `editor.save_level` call it, so a level save never leaves
/// tile strokes behind. `Ok(None)` when there is no map or nothing to save.
pub fn save_dirty_tilemap(world: &mut World) -> Result<Option<(PathBuf, usize)>, Mode2dError> {
    let Some(mut mode) = world.get_resource_mut::<Mode2d>() else {
        return Ok(None);
    };
    if !mode.doc.as_ref().is_some_and(|d| d.dirty) {
        return Ok(None);
    }
    save_tilemap(&mut mode, None).map(Some)
}

/// [`save_dirty_tilemap`] for the editor's save toasts: empty when there was nothing to save,
/// else `" and <map>"` or `"; tile map not saved: <error>"`.
#[must_use]
pub fn save_dirty_tilemap_note(world: &mut World) -> String {
    match save_dirty_tilemap(world) {
        Ok(None) => String::new(),
        Ok(Some((path, _))) => format!(" and {}", path.display()),
        Err(e) => format!("; tile map not saved: {e}"),
    }
}

/// The end of a windowed level save, after the level file is written: writes a dirty tile map
/// ([`save_dirty_tilemap`]) and only then marks the undo stack saved. The tile strokes are on the
/// same undo stack, so a failed tile-map write keeps the session dirty (the close prompt and
/// autosave still see the edits), as the headless and agent saves do (ADR-7412). Returns the
/// toast note of [`save_dirty_tilemap_note`].
#[must_use]
pub fn save_dirty_tilemap_and_mark_saved(world: &mut World) -> String {
    let result = save_dirty_tilemap(world);
    if result.is_ok() {
        world
            .get_resource_or_insert_with::<crate::command::UndoStack>(crate::command::UndoStack::new)
            .mark_saved();
    }
    match result {
        Ok(None) => String::new(),
        Ok(Some((path, _))) => format!(" and {}", path.display()),
        Err(e) => format!("; tile map not saved: {e}"),
    }
}

/// The overlay of a front pane: map bounds, grid and hover cell, in pane pixels.
#[must_use]
pub fn overlay_shapes(
    mode: &Mode2d,
    view: &ViewCamera,
    size: Vec2,
    colors: &crate::theme::OverlayTokens,
) -> Vec<OverlayShape> {
    let mut out = Vec::new();
    let Some(doc) = mode.doc.as_ref().filter(|_| mode.active) else {
        return out;
    };
    let map = &doc.map;
    let (tw, th) = (map.tile_width as f32, map.tile_height as f32);
    let at = |x: f32, y: f32| view.project(mode.origin + Vec3::new(x, y, 0.0), size);
    let line = |a: Option<Vec2>, b: Option<Vec2>, color, thickness, out: &mut Vec<OverlayShape>| {
        if let (Some(a), Some(b)) = (a, b) {
            out.push(OverlayShape::Line {
                a,
                b,
                color,
                thickness,
            });
        }
    };
    let [pw, ph] = map.pixel_size().map(|v| v as f32);
    let map_h = map.orientation.pixel_size(map.grid_size())[1];
    let stroke = |x: u32, y: u32, color, thickness: i32, out: &mut Vec<OverlayShape>| {
        let outline = map.tile_world_outline_points(map_h, x, y);
        let points = &outline.points[..outline.len];
        for (index, a) in points.iter().enumerate() {
            let b = points[(index + 1) % points.len()];
            line(at(a[0], a[1]), at(b[0], b[1]), color, thickness, out);
        }
    };
    let (w, h) = if map.orientation.is_orthogonal() {
        (map.width as f32 * tw, map.height as f32 * th)
    } else {
        (pw, ph)
    };
    let tile_px = tw.min(th) / view.units_per_pixel(size.y).max(1e-6);
    let grid_ok = mode.grid && tile_px >= GRID_MIN_TILE_PX;
    if grid_ok
        && map.orientation.is_orthogonal()
        && map.width <= MAX_GRID_LINES
        && map.height <= MAX_GRID_LINES
    {
        for x in 1..map.width {
            let wx = x as f32 * tw;
            line(at(wx, 0.0), at(wx, h), colors.grid_minor, 0, &mut out);
        }
        for y in 1..map.height {
            let wy = y as f32 * th;
            line(at(0.0, wy), at(w, wy), colors.grid_minor, 0, &mut out);
        }
    } else if grid_ok
        && !map.orientation.is_orthogonal()
        && u64::from(map.width) * u64::from(map.height) <= MAX_OUTLINE_CELLS
    {
        // Diamonds and hexagons do not share straight grid lines: outline each cell.
        for y in 0..map.height {
            for x in 0..map.width {
                stroke(x, y, colors.grid_minor, 0, &mut out);
            }
        }
    }
    let corners = [(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)];
    for (&(ax, ay), &(bx, by)) in corners.iter().zip(corners.iter().cycle().skip(1)) {
        line(at(ax, ay), at(bx, by), colors.bounds, 1, &mut out);
    }
    if let Some((x, y)) = mode.hover {
        if map.orientation.is_orthogonal() {
            let (min, max) = map.tile_world_rect(x, y);
            let [min, max] = [min, max].map(|p| p.map(|v| v as f32));
            let cell = [min, [max[0], min[1]], max, [min[0], max[1]]];
            for (index, a) in cell.iter().enumerate() {
                let b = cell[(index + 1) % cell.len()];
                line(
                    at(a[0], a[1]),
                    at(b[0], b[1]),
                    colors.selection_outline,
                    1,
                    &mut out,
                );
            }
        } else {
            stroke(x, y, colors.selection_outline, 1, &mut out);
        }
    }
    // The cells a line or rectangle in progress lands on.
    let preview = mode.shape_preview();
    if preview.len() as u64 <= MAX_FOOTPRINT_CELLS {
        for (x, y) in preview {
            stroke(x, y, colors.selection_outline, 0, &mut out);
        }
    }
    // The cells being selected (Ctrl-drag), and the footprint the stamp brush would paste.
    let mut footprint = |x0: u32, y0: u32, x1: u32, y1: u32| {
        let (x1, y1) = (
            x1.min(map.width.saturating_sub(1)),
            y1.min(map.height.saturating_sub(1)),
        );
        if x0 > x1 || y0 > y1 || u64::from(x1 - x0 + 1) * u64::from(y1 - y0 + 1) > MAX_FOOTPRINT_CELLS {
            return;
        }
        for y in y0..=y1 {
            for x in x0..=x1 {
                stroke(x, y, colors.selection_outline, 0, &mut out);
            }
        }
    };
    if let Some((a, b)) = mode.select {
        footprint(a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1));
    }
    if let (Tool2d::Paint, Brush2d::Stamp, Some(stamp), Some((x, y))) =
        (mode.tool, &mode.brush, &mode.stamp, mode.hover)
    {
        let x0 = x.saturating_sub(stamp.width / 2);
        let y0 = y.saturating_sub(stamp.height / 2);
        footprint(
            x0,
            y0,
            x0.saturating_add(stamp.width - 1),
            y0.saturating_add(stamp.height - 1),
        );
    }
    out
}

/// Whether the 2D mode is on.
#[must_use]
pub fn is_active(world: &World) -> bool {
    world.get_resource::<Mode2d>().is_some_and(|m| m.active)
}

/// A half-transparent preview of the tile brush over the hover cell.
fn brush_preview(mode: &Mode2d) -> Option<tiles::TileQuad> {
    let Brush2d::Tile(raw) = mode.brush else {
        return None;
    };
    let doc = mode.doc.as_ref()?;
    let (gid, _) = split_gid(raw);
    let (tileset, local) = doc.map.tileset_for(gid)?;
    let uv = tiles::corner_uvs(&doc.map.tilesets[tileset], local, raw)?;
    Some(tiles::TileQuad {
        layer: mode.layer,
        tileset,
        local,
        min: Vec2::ZERO,
        max: Vec2::ZERO,
        uv,
        opacity: 0.6,
    })
}

/// Under the status line. Opaque: the backdrop and the tiles behind it can be any color, and a
/// translucent black over the light backdrop composites to a gray the text cannot be read on.
const STATUS_BACKING: egui::Color32 = egui::Color32::from_gray(24);

/// Paints the front pane's tiles, 2D overlay and status line.
pub fn paint_overlay(ui: &egui::Ui, world: &mut World, view: &ViewCamera, rect: egui::Rect) {
    tiles::sync_textures(ui.ctx(), world);
    let Some(mut mode) = world.get_resource_mut::<Mode2d>().filter(|m| m.active) else {
        return;
    };
    let _span = tracing::info_span!(target: "regia_editor", "mode2d.paint_overlay").entered();
    let size = Vec2::new(rect.width(), rect.height());
    if mode.clock.playing {
        // The pane clock follows the frame time while playing; a paused pane never repaints itself.
        // Once per egui frame: egui runs the UI a second time on a frame that asks for a discard,
        // and a layout can show two Front panes; each call used to step the one clock again.
        let frame = ui.ctx().cumulative_frame_nr();
        let ticked_id = egui::Id::new("regia_mode2d_clock_ticked_frame");
        if ui.ctx().data(|d| d.get_temp::<u64>(ticked_id)) != Some(frame) {
            ui.ctx().data_mut(|d| d.insert_temp(ticked_id, frame));
            let dt = std::time::Duration::from_secs_f32(ui.input(|i| i.stable_dt).clamp(0.0, 0.25));
            mode.clock.tick(dt);
        }
        ui.ctx().request_repaint();
    }
    let drawn = tiles::view_quads(&mode, view, size);
    mode.view = (drawn.quads.len(), drawn.stride);
    let world = &*world;
    let Some(mode) = world.get_resource::<Mode2d>() else {
        return;
    };
    let textures = world.get_resource::<tiles::TileTextures>();
    let painter = ui.painter_at(rect);
    tiles::paint_quads(&painter, rect, &drawn.quads, textures);
    if let (Some(mut quad), Some((x, y)), Some(doc)) = (brush_preview(mode), mode.hover, mode.doc.as_ref()) {
        let (min, max) = doc.map.tile_world_rect(x, y);
        let [min, max] = [min, max].map(|p| p.map(|v| v as f32));
        let at = |x: f32, y: f32| view.project(mode.origin + Vec3::new(x, y, 0.0), size);
        if let (Some(top_left), Some(bottom_right)) = (at(min[0], max[1]), at(max[0], min[1])) {
            quad.min = top_left;
            quad.max = bottom_right;
            tiles::paint_quads(&painter, rect, &[quad], textures);
        }
    }
    let shapes = overlay_shapes(mode, view, size, &crate::theme::tokens(world).overlay);
    crate::overlays::paint_shapes_egui(&painter, rect, &shapes);
    let to_pixel = |p: [f32; 2]| {
        view.project(mode.origin + Vec3::new(p[0], p[1], 0.0), size)
            .map(|v| rect.min + egui::vec2(v.x, v.y))
    };
    let from_pixel = |p: egui::Pos2| {
        let local = p - rect.min;
        mode.level_at_pixel(view, Vec2::new(local.x, local.y), size)
    };
    guides::paint_rulers(
        &painter,
        rect,
        &mode.guides,
        to_pixel,
        from_pixel,
        view.units_per_pixel(size.y),
        crate::theme::tokens(world),
    );
    object_drag::paint_objects(&painter, rect, mode, view, size, crate::theme::tokens(world));
    let color = crate::theme::tokens(world).overlay.selection_outline;
    let galley = ui
        .painter()
        .layout_no_wrap(mode.status(), egui::FontId::proportional(12.0), color);
    let at = rect.left_bottom() + egui::vec2(8.0, -8.0 - galley.size().y);
    let backing = egui::Rect::from_min_size(at - egui::vec2(4.0, 3.0), galley.size() + egui::vec2(8.0, 6.0));
    painter.rect_filled(backing, 3.0, STATUS_BACKING);
    painter.galley(at, galley, color);
}

/// The front pane's pointer input while the mode is on: hover cell and strokes.
pub fn viewport_input(
    ui: &egui::Ui,
    world: &mut World,
    response: &egui::Response,
    view: &ViewCamera,
    size: Vec2,
) {
    let Some(mut mode) = world.remove_resource::<Mode2d>() else {
        return;
    };
    if mode.active {
        let pixel = response.hover_pos().map(|p| {
            let local = p - response.rect.left_top();
            Vec2::new(local.x, local.y)
        });
        mode.hover = pixel.and_then(|p| mode.cell_at_pixel(view, p, size));
        // A second pass of this frame (egui reruns the UI when a pass asks for a discard) sees the
        // same press and release: strokes, guides and object drags apply once per frame, so one
        // random-brush click is one stroke and one guide drag-out is one guide (review g17 row 5).
        if !first_apply_this_frame(ui.ctx(), response.id.with("mode2d_viewport_input")) {
            world.insert_resource(mode);
            return;
        }
        let primary = egui::PointerButton::Primary;
        if guide_input(ui, &mut mode, response, view, size)
            || object_drag::object_input(ui, world, &mut mode, response, view, size)
        {
            world.insert_resource(mode);
            return;
        }
        let erase = ui.input(|i| i.modifiers.shift);
        let alt = ui.input(|i| i.modifiers.alt);
        let ctrl = ui.input(|i| i.modifiers.command);
        // Ctrl (or the pick tool): a click picks the cell's tile as the brush, a drag selects a
        // rectangle of cells and releasing copies it into the stamp.
        let picking = ctrl || mode.tool == Tool2d::Pick;
        if picking && !alt && (response.drag_started_by(primary) || response.clicked_by(primary)) {
            let pressed = ui
                .input(|i| i.pointer.press_origin())
                .and_then(|p| {
                    let local = p - response.rect.left_top();
                    mode.cell_at_pixel(view, Vec2::new(local.x, local.y), size)
                })
                .or(mode.hover);
            mode.select = pressed.map(|c| (c, c));
        }
        if let (Some(sel), Some(cell)) = (mode.select.as_mut(), mode.hover) {
            sel.1 = cell;
        }
        if (response.drag_stopped_by(primary) || response.clicked_by(primary))
            && let Some((a, b)) = mode.select.take()
        {
            let picked = if a == b {
                mode.pick_cell(a)
            } else {
                mode.copy_stamp(a, b)
            };
            if let Err(e) = picked {
                tracing::warn!(target: "regia_editor::mode2d", "pick: {e}");
            }
        }
        // A right-click erases in every tool: the cell, or the bucket's region (a right drag
        // pans the 2D view, as in every orthographic pane).
        let secondary = egui::PointerButton::Secondary;
        if !alt
            && !mode.stroking()
            && response.clicked_by(secondary)
            && let Some(cell) = mode.hover
        {
            let tool = if mode.tool == Tool2d::Bucket {
                Tool2d::Bucket
            } else {
                Tool2d::Paint
            };
            mode.begin_stroke_as(true, tool);
            if let Err(e) = mode.stroke_to(cell) {
                tracing::warn!(target: "regia_editor::mode2d", "tile erase: {e}");
            }
            if let Some(command) = mode.end_stroke() {
                world.insert_resource(mode);
                push_stroke(world, command);
                return;
            }
        }
        if !alt
            && !picking
            && mode.select.is_none()
            && (response.drag_started_by(primary) || response.clicked_by(primary))
        {
            mode.begin_stroke(erase);
            // egui reports a drag only once the pointer has moved: start where the button went down.
            let pressed = ui.input(|i| i.pointer.press_origin()).and_then(|p| {
                let local = p - response.rect.left_top();
                mode.cell_at_pixel(view, Vec2::new(local.x, local.y), size)
            });
            if let Some(cell) = pressed
                && let Err(e) = mode.stroke_to(cell)
            {
                tracing::warn!(target: "regia_editor::mode2d", "tile stroke: {e}");
            }
        }
        if mode.stroking()
            && (response.dragged_by(primary)
                || response.clicked_by(primary)
                || response.drag_started_by(primary))
            && let Some(cell) = mode.hover
            && let Err(e) = mode.stroke_to(cell)
        {
            tracing::warn!(target: "regia_editor::mode2d", "tile stroke: {e}");
        }
        if (response.drag_stopped_by(primary) || response.clicked_by(primary))
            && let Some(command) = mode.end_stroke()
        {
            world.insert_resource(mode);
            push_stroke(world, command);
            return;
        }
    }
    world.insert_resource(mode);
}

/// Rulers and guides in the pane: a press on a ruler band pulls out a new guide (the top ruler a
/// horizontal one, the left ruler a vertical one), a press on a guide line picks it up; dragging
/// moves it, and releasing it over a ruler removes it. True while a guide has the pointer (no
/// paint, pick or stroke runs then).
fn guide_input(
    ui: &egui::Ui,
    mode: &mut Mode2d,
    response: &egui::Response,
    view: &ViewCamera,
    size: Vec2,
) -> bool {
    let primary = egui::PointerButton::Primary;
    let local = |p: egui::Pos2| {
        let l = p - response.rect.left_top();
        Vec2::new(l.x, l.y)
    };
    let in_top = |p: Vec2| p.y >= 0.0 && p.y < guides::RULER_PX;
    let in_left = |p: Vec2| p.x >= 0.0 && p.x < guides::RULER_PX;
    if mode.dragging_guide.is_none()
        && !mode.stroking()
        && mode.guides.rulers
        && (response.drag_started_by(primary) || response.clicked_by(primary))
        && let Some(press) = ui.input(|i| i.pointer.press_origin()).map(local)
    {
        let at = mode.level_at_pixel(view, press, size);
        let picked = if in_top(press) || in_left(press) {
            at.and_then(|at| {
                let (axis, value) = if in_top(press) {
                    (guides::GuideAxis::Horizontal, at[1])
                } else {
                    (guides::GuideAxis::Vertical, at[0])
                };
                mode.guides.add(axis, value).ok()
            })
        } else {
            // An existing guide within the snap distance of the press.
            let tolerance = view.units_per_pixel(size.y) * guides::SNAP_PX;
            at.and_then(|at| {
                mode.guides.guides.iter().position(|g| match g.axis {
                    guides::GuideAxis::Vertical => (g.at - at[0]).abs() <= tolerance,
                    guides::GuideAxis::Horizontal => (g.at - at[1]).abs() <= tolerance,
                })
            })
        };
        mode.dragging_guide = picked;
    }
    let Some(index) = mode.dragging_guide else {
        return false;
    };
    if let Some(at) = response
        .hover_pos()
        .or_else(|| ui.input(|i| i.pointer.latest_pos()))
        .map(local)
        .and_then(|p| mode.level_at_pixel(view, p, size))
        && let Some(guide) = mode.guides.guides.get_mut(index)
    {
        guide.at = match guide.axis {
            guides::GuideAxis::Vertical => at[0],
            guides::GuideAxis::Horizontal => at[1],
        };
    }
    if response.drag_stopped_by(primary)
        || response.clicked_by(primary)
        || !ui.input(|i| i.pointer.primary_down())
    {
        mode.dragging_guide = None;
        let over_ruler = ui
            .input(|i| i.pointer.latest_pos())
            .map(local)
            .is_some_and(|p| in_top(p) || in_left(p));
        if over_ruler && index < mode.guides.guides.len() {
            // Dropped back on a ruler: the guide goes away.
            mode.guides.guides.remove(index);
        }
    }
    true
}

/// Per-pane frame hook: `B` toggles the mode, `,` `.` `[` `]` step layer and terrain, and a
/// pending frame request switches the layout to its front pane and frames the map there.
///
/// The keys run when `has_keys` (the pane is the active pane of the panel that has the hotkeys)
/// and no other widget holds keyboard focus. `pane_focused` is the pane's own keyboard focus
/// (Tab into the viewport, FU-9): it does not count as egui wanting the keyboard, so a keyboard
/// user's keys reach the pane while a focused text field still keeps them (ADR-8870).
pub fn viewport_frame(
    ui: &egui::Ui,
    world: &mut World,
    layout: &mut ViewportLayout,
    index: usize,
    camera: &mut EditorCamera,
    has_keys: bool,
    pane_focused: bool,
) {
    if has_keys && (pane_focused || !ui.ctx().egui_wants_keyboard_input()) {
        let pressed = |k: egui::Key| ui.input(|i| i.key_pressed(k));
        if pressed(egui::Key::B) {
            let mut mode = world.get_resource_or_insert_with::<Mode2d>(Mode2d::default);
            mode.active = !mode.active;
            mode.pending_frame = mode.active;
            if mode.active && mode.doc.is_none() {
                open_level_map(world);
            }
        }
        if let Some(mut mode) = world.get_resource_mut::<Mode2d>().filter(|m| m.active) {
            if pressed(egui::Key::Comma) {
                mode.cycle_layer(-1);
            }
            if pressed(egui::Key::Period) {
                mode.cycle_layer(1);
            }
            if pressed(egui::Key::OpenBracket) {
                mode.cycle_terrain(-1);
            }
            if pressed(egui::Key::CloseBracket) {
                mode.cycle_terrain(1);
            }
        }
    }
    // A stroke never outlives the button: leaving the mode, or Play taking the pane, mid-drag ends
    // it here, so its cells keep their undo entry.
    let primary_down = ui.input(|i| i.pointer.primary_down());
    let ended = world
        .get_resource_mut::<Mode2d>()
        .filter(|m| m.stroking() && !(m.active && primary_down))
        .and_then(|mut m| m.end_stroke());
    if let Some(command) = ended {
        push_stroke(world, command);
    }
    // The same for an object drag and a guide drag.
    let dropped = world
        .get_resource_mut::<Mode2d>()
        .filter(|m| (m.object_drag.is_some() || m.dragging_guide.is_some()) && !(m.active && primary_down))
        .and_then(|mut m| {
            m.dragging_guide = None;
            m.end_object_drag()
        });
    if let Some(command) = dropped {
        push_command(world, Box::new(command));
    }
    let Some(mut mode) = world.get_resource_mut::<Mode2d>().filter(|m| m.pending_frame) else {
        return;
    };
    mode.pending_frame = false;
    let Some(front) = layout.panes.iter().position(|p| p.kind == ViewKind::Front) else {
        return;
    };
    layout.active = front;
    if front == index {
        mode.frame(camera);
    } else {
        let mut framed = layout.panes[front].camera.clone();
        mode.frame(&mut framed);
        layout.panes[front].camera = framed;
    }
}

// ---- Agent methods (`book/agents/editor-2d.md`) -------------------------------------------

fn mode2d_error(error: &Mode2dError) -> RpcError {
    let mut rpc = RpcError::invalid_params(error.to_string());
    if let Some(data) = rpc.data.as_mut() {
        data["mode2d_error"] = json!(error.code());
    }
    rpc
}

fn bad(message: impl Into<String>) -> RpcError {
    mode2d_error(&Mode2dError::BadParams(message.into()))
}

/// A path parameter of an agent request, through the editor's one document-path rule (ADR-7388,
/// audit rc1-sec-paths-z finding 3): relative to the project, or absolute under the project or the
/// temp directory; `..`, drive letters, UNC forms, NUL and symlinks out are `path_outside_project`.
fn resolve_path(world: &World, key: &str, path: &str) -> Result<PathBuf, RpcError> {
    crate::agent_ext::document_request_path(world, key, path)
}

/// A `regia_2d::TileMap` asset path from level content, relative to `base` (the project or its
/// `assets` folder): `None` when it breaks a rule of `regia_core::project_path` (absolute, `..`,
/// a drive or UNC form, a symlink leading out) or names no file (audit rc1-sec-paths-z finding 3).
fn content_map_path(base: &Path, asset: &str) -> Option<PathBuf> {
    regia_core::project_path::resolve_read(base, asset).ok()
}

/// The world origin of a `regia_2d::TileMap` entity that shows `path` (its asset path is a
/// component-boundary suffix of the file path, case-folded on Windows where the filesystem
/// folds — audit G finding 4).
fn map_origin(world: &mut World, path: &Path) -> Option<Vec3> {
    let mut query = world.query::<(Entity, &regia_2d::TileMap)>();
    let mut found: Vec<(Entity, String)> = query.iter(world).map(|(e, m)| (e, m.map.clone())).collect();
    found.sort_by_key(|(e, _)| *e);
    let shown = path.to_string_lossy().replace('\\', "/");
    let shown = Path::new(&shown);
    let entity = found
        .into_iter()
        .find(|(_, m)| {
            !m.is_empty()
                && crate::path_norm::ends_with_components(
                    shown,
                    Path::new(&m.replace('\\', "/")),
                    crate::path_norm::FOLD_CASE,
                )
        })?
        .0;
    crate::mesh_source::world_matrix(world, entity).map(|m| m.w_axis.truncate())
}

/// Loads `path` as the document, as `editor.mode2d_set map` and the 2D tab do: the origin comes
/// from a `regia_2d::TileMap` entity that shows the file, and the map gets framed.
pub fn open_map(world: &mut World, path: &Path, discard: bool) -> Result<(), Mode2dError> {
    let origin = map_origin(world, path).unwrap_or(Vec3::ZERO);
    let mut mode = world.remove_resource::<Mode2d>().unwrap_or_default();
    let result = mode.load_map(path, discard);
    if result.is_ok() {
        mode.origin = origin;
        mode.pending_frame = true;
    }
    world.insert_resource(mode);
    result
}

/// The `.tilemap.ron` files the level's `regia_2d::TileMap` entities show, sorted by path (entity
/// ids depend on the load order). An asset path is relative to the project or to its `assets`
/// folder.
#[must_use]
pub fn level_maps(world: &mut World) -> Vec<PathBuf> {
    let mut query = world.query::<&regia_2d::TileMap>();
    let shown: Vec<String> = query.iter(world).map(|m| m.map.clone()).collect();
    let root = crate::mesh_source::project_root(world).unwrap_or_else(|| crate::agent_ext::agent_root(world));
    let assets = root.join("assets");
    let found: BTreeSet<PathBuf> = shown
        .iter()
        .filter_map(|asset| {
            [&root, &assets]
                .into_iter()
                .filter_map(|base| content_map_path(base, asset))
                .find(|p| is_tilemap_ron(p) && p.is_file())
        })
        .collect();
    found.into_iter().collect()
}

/// The mode came on without a document: opens the level's map, or asks for the 2D tab.
fn open_level_map(world: &mut World) {
    let opened = match level_maps(world).first() {
        Some(path) => open_map(world, path, false)
            .inspect_err(|e| tracing::warn!(target: "regia_editor::mode2d", "{}: {e}", path.display()))
            .is_ok(),
        None => false,
    };
    if !opened {
        world.get_resource_or_insert_with(Mode2d::default).focus_panel = true;
    }
}

fn brush_json(brush: &Brush2d) -> Value {
    match brush {
        Brush2d::Tile(raw) => json!({
            "tile": raw & GID_MASK,
            "flip": { "x": raw & FLIP_X_BIT != 0, "y": raw & FLIP_Y_BIT != 0, "diagonal": raw & FLIP_DIAGONAL_BIT != 0 },
        }),
        Brush2d::Terrain(name) => json!({ "terrain": name }),
        Brush2d::Erase => json!({ "erase": true }),
        Brush2d::Stamp => json!({ "stamp": true }),
    }
}

fn random_json(random: Option<&RandomBrush>) -> Value {
    random.map_or(Value::Null, |r| {
        json!({
            "tiles": r.tiles.iter().map(|t| json!({ "tile": t.raw & GID_MASK, "raw": t.raw, "weight": t.weight })).collect::<Vec<_>>(),
            "nothing": r.nothing,
            "seed": r.seed,
        })
    })
}

/// Parses a `random` parameter: null, or `{tiles: [{tile, weight?, flip?}], nothing?, seed?}`.
fn parse_random(value: &Value) -> Result<Option<RandomBrush>, Mode2dError> {
    if value.is_null() {
        return Ok(None);
    }
    let shape = "`random` is null or {tiles: [{tile, weight?, flip?}], nothing?: 0..1, seed?}";
    let bad = || Mode2dError::BadParams(shape.into());
    let object = value.as_object().ok_or_else(bad)?;
    let tiles = object
        .get("tiles")
        .and_then(Value::as_array)
        .ok_or_else(bad)?
        .iter()
        .map(|t| {
            let raw = parse_brush(&json!({ "brush": t })).ok().and_then(|b| match b {
                Brush2d::Tile(raw) => Some(raw),
                _ => None,
            })?;
            let weight = match t.get("weight") {
                None => 1,
                Some(w) => u32::try_from(w.as_u64()?).ok()?,
            };
            Some(RandomTile { raw, weight })
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(bad)?;
    let nothing = match object.get("nothing") {
        None => 0.0,
        Some(v) => v.as_f64().ok_or_else(bad)? as f32,
    };
    let seed = match object.get("seed") {
        None => 0,
        Some(v) => v.as_u64().ok_or_else(bad)?,
    };
    let brush = RandomBrush { tiles, nothing, seed };
    brush.validate().map_err(Mode2dError::BadParams)?;
    Ok(Some(brush))
}

fn parse_tool(value: &Value) -> Result<Tool2d, Mode2dError> {
    value
        .as_str()
        .and_then(Tool2d::parse)
        .ok_or_else(|| Mode2dError::BadParams("`tool` is paint, line, rect, bucket or pick".into()))
}

fn parse_autotile_mode(value: &Value) -> Result<AutotileMode, Mode2dError> {
    value
        .as_str()
        .and_then(AutotileMode::parse)
        .ok_or_else(|| Mode2dError::BadParams("`mode` is connect or path".into()))
}

fn bool_param(params: &Value, key: &str) -> Result<Option<bool>, Mode2dError> {
    match params.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| Mode2dError::BadParams(format!("`{key}` is a boolean"))),
    }
}

fn state_json(world: &World) -> Value {
    let Some(mode) = world.get_resource::<Mode2d>() else {
        return json!({ "active": false, "map": null, "rules": null, "layer": null, "brush": null, "grid": true, "hover": null, "origin": [0.0, 0.0, 0.0], "view": { "tiles": 0, "stride": 1 }, "undo_depth": 0 });
    };
    let map = mode.doc.as_ref().map(|d| {
        json!({
            "path": d.path.display().to_string(),
            "size": [d.map.width, d.map.height],
            "tile_size": [d.map.tile_width, d.map.tile_height],
            "orientation": d.map.orientation.name(),
            "dirty": d.dirty,
            "layers": d.map.layers.iter().enumerate().map(|(i, l)| json!({
                "index": i,
                "name": l.name(),
                "kind": if matches!(l, MapLayer::Tiles(_)) { "tiles" } else { "objects" },
            })).collect::<Vec<_>>(),
            "tilesets": d.map.tilesets.iter().zip(&d.images).map(|(set, image)| json!({
                "name": set.name,
                "first_gid": set.first_gid,
                "tile_count": set.tile_count,
                "columns": set.columns,
                "image": image.path.display().to_string(),
                "loaded": image.pixels.is_some(),
                "error": image.error,
            })).collect::<Vec<_>>(),
        })
    });
    let layer = mode
        .doc
        .as_ref()
        .and_then(|d| d.map.layers.get(mode.layer))
        .map(|l| json!({ "index": mode.layer, "name": l.name() }));
    let rules = mode.rules.as_ref().map(|r| {
        json!({
            "path": r.path.display().to_string(),
            "tileset": r.set.tileset,
            "terrains": r.set.terrains.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
        })
    });
    let undo_depth = world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth);
    json!({
        "active": mode.active,
        "map": map,
        "rules": rules,
        "layer": layer,
        "brush": brush_json(&mode.brush),
        "grid": mode.grid,
        "hover": mode.hover.map(|(x, y)| [x, y]),
        "origin": [mode.origin.x, mode.origin.y, mode.origin.z],
        "view": { "tiles": mode.view.0, "stride": mode.view.1 },
        "stamp": mode.stamp.as_ref().map(|s| json!({ "width": s.width, "height": s.height })),
        "clock": { "playing": mode.clock.playing, "time_ms": mode.clock.time_ms() },
        "tool": mode.tool.name(),
        "rect_filled": mode.rect_filled,
        "bucket_contiguous": mode.bucket_contiguous,
        "random": random_json(mode.random.as_ref()),
        "autotile_mode": mode.autotile_mode.name(),
        "undo_depth": undo_depth,
    })
}

/// `editor.mode2d_get`.
fn mode2d_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(state_json(world))
}

fn parse_brush(params: &Value) -> Result<Brush2d, RpcError> {
    let brush = &params["brush"];
    if let Some(tile) = brush.get("tile") {
        let gid = tile
            .as_u64()
            .and_then(|g| u32::try_from(g).ok())
            .filter(|g| g & !GID_MASK == 0)
            .ok_or_else(|| bad("`brush.tile` is a gid without flag bits (flips go in `brush.flip`)"))?;
        let flip = brush.get("flip");
        let bit = |key: &str, bit: u32| {
            if flip
                .and_then(|f| f.get(key))
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                bit
            } else {
                0
            }
        };
        return Ok(Brush2d::Tile(
            gid | bit("x", FLIP_X_BIT) | bit("y", FLIP_Y_BIT) | bit("diagonal", FLIP_DIAGONAL_BIT),
        ));
    }
    if let Some(name) = brush.get("terrain").and_then(Value::as_str) {
        return Ok(Brush2d::Terrain(name.to_string()));
    }
    if brush.get("erase").and_then(Value::as_bool) == Some(true) {
        return Ok(Brush2d::Erase);
    }
    if brush.get("stamp").and_then(Value::as_bool) == Some(true) {
        return Ok(Brush2d::Stamp);
    }
    Err(bad(
        "`brush` is {tile: gid, flip?}, {terrain: name}, {erase: true} or {stamp: true}",
    ))
}

/// The parsed parameters of `editor.mode2d_set`.
struct SetRequest {
    map: Option<PathBuf>,
    rules: Option<Option<PathBuf>>,
    origin: Option<Vec3>,
    found_origin: Option<Vec3>,
    discard: bool,
    brush: Option<Brush2d>,
}

/// Applies the `clock` parameter of `editor.mode2d_set`: `{ play?, reset?, step_ms? }`, in that
/// order (`reset`, then `step_ms`, then `play`).
fn apply_clock(clock: &mut AnimClock, params: &Value) -> Result<(), Mode2dError> {
    let bad = |m: &str| Mode2dError::BadParams(m.into());
    let object = params
        .as_object()
        .ok_or_else(|| bad("`clock` is {play?, reset?, step_ms?}"))?;
    if let Some(key) = object
        .keys()
        .find(|k| !matches!(k.as_str(), "play" | "reset" | "step_ms"))
    {
        return Err(Mode2dError::BadParams(format!("unknown `clock.{key}`")));
    }
    let flag = |key: &str| -> Result<Option<bool>, Mode2dError> {
        match object.get(key) {
            None => Ok(None),
            Some(v) => v
                .as_bool()
                .map(Some)
                .ok_or_else(|| Mode2dError::BadParams(format!("`clock.{key}` is a boolean"))),
        }
    };
    let (play, reset) = (flag("play")?, flag("reset")?);
    let step =
        match object.get("step_ms") {
            None => None,
            Some(v) => Some(v.as_u64().filter(|ms| *ms <= MAX_CLOCK_STEP_MS).ok_or_else(|| {
                Mode2dError::BadParams(format!("`clock.step_ms` is 0..={MAX_CLOCK_STEP_MS}"))
            })?),
        };
    if reset == Some(true) {
        clock.reset();
    }
    if let Some(ms) = step {
        clock.step(std::time::Duration::from_millis(ms));
    }
    if let Some(play) = play {
        clock.playing = play;
    }
    Ok(())
}

/// Most one `clock.step_ms` may advance the pane clock (one hour).
const MAX_CLOCK_STEP_MS: u64 = 3_600_000;

/// Applies an `editor.mode2d_set` request in order: map, rules, origin, layer, brush, grid, active.
fn apply_set(mode: &mut Mode2d, request: &SetRequest, params: &Value) -> Result<(), Mode2dError> {
    if let Some(path) = &request.map {
        mode.load_map(path, request.discard)?;
        mode.origin = request.found_origin.unwrap_or(Vec3::ZERO);
        mode.pending_frame = true;
    }
    match &request.rules {
        Some(Some(path)) => mode.load_rules(path)?,
        Some(None) => mode.rules = None,
        None => {}
    }
    if let Some(origin) = request.origin {
        mode.origin = origin;
    }
    if let Some(layer) = params.get("layer") {
        let doc = mode.doc.as_ref().ok_or(Mode2dError::NoMap)?;
        let index = match layer {
            Value::String(name) => doc
                .map
                .layer_index(name)
                .ok_or_else(|| Mode2dError::BadLayer(format!("no layer named `{name}`")))?,
            other => other
                .as_u64()
                .and_then(|i| usize::try_from(i).ok())
                .ok_or_else(|| Mode2dError::BadParams("`layer` is a name or an index".into()))?,
        };
        mode.set_layer(index)?;
    }
    if let Some(brush) = &request.brush {
        if let Brush2d::Terrain(name) = brush {
            let rules = mode.rules.as_ref().ok_or(Mode2dError::NoRules)?;
            if rules.set.terrain_index(name).is_none() {
                return Err(Mode2dError::UnknownTerrain(name.clone()));
            }
        }
        mode.brush = brush.clone();
    }
    if let Some(grid) = params.get("grid").and_then(Value::as_bool) {
        mode.grid = grid;
    }
    if let Some(tool) = params.get("tool") {
        mode.tool = parse_tool(tool)?;
    }
    if let Some(filled) = bool_param(params, "rect_filled")? {
        mode.rect_filled = filled;
    }
    if let Some(contiguous) = bool_param(params, "bucket_contiguous")? {
        mode.bucket_contiguous = contiguous;
    }
    if let Some(random) = params.get("random") {
        mode.random = parse_random(random)?;
    }
    if let Some(autotile) = params.get("autotile_mode") {
        mode.autotile_mode = parse_autotile_mode(autotile)?;
    }
    if let Some(clock) = params.get("clock") {
        apply_clock(&mut mode.clock, clock)?;
    }
    if params.get("reload_images").and_then(Value::as_bool) == Some(true) {
        mode.reload_images();
    }
    if let Some(active) = params.get("active").and_then(Value::as_bool) {
        if active && !mode.active {
            mode.pending_frame = true;
        }
        mode.active = active;
    }
    Ok(())
}

/// `editor.mode2d_set`.
fn mode2d_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let map_path = str_param(params, "map")
        .map(|p| resolve_path(world, "map", p))
        .transpose()?;
    let rules_path = match params.get("rules") {
        Some(Value::Null) => Some(None),
        Some(Value::String(p)) => Some(Some(resolve_path(world, "rules", p)?)),
        Some(_) => return Err(bad("`rules` is a path or null")),
        None => None,
    };
    let origin = match params.get("origin") {
        Some(v) => {
            let coords: Option<Vec<f32>> = v
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_f64).map(|f| f as f32).collect());
            match coords.as_deref() {
                Some([x, y, z]) if x.is_finite() && y.is_finite() && z.is_finite() => {
                    Some(Vec3::new(*x, *y, *z))
                }
                _ => return Err(bad("`origin` is [x, y, z]")),
            }
        }
        None => None,
    };
    // Refuse a bad `clock` before anything changes (a `discard` load would already have dropped
    // the unsaved map): validate it on a scratch clock first.
    if let Some(clock) = params.get("clock") {
        apply_clock(&mut AnimClock::default(), clock).map_err(|e| mode2d_error(&e))?;
    }
    // The same for the tool parameters.
    let checks = [
        params.get("tool").map(|v| parse_tool(v).map(|_| ())),
        params.get("random").map(|v| parse_random(v).map(|_| ())),
        params
            .get("autotile_mode")
            .map(|v| parse_autotile_mode(v).map(|_| ())),
        Some(bool_param(params, "rect_filled").map(|_| ())),
        Some(bool_param(params, "bucket_contiguous").map(|_| ())),
    ];
    for check in checks.into_iter().flatten() {
        check.map_err(|e| mode2d_error(&e))?;
    }
    let discard = params.get("discard").and_then(Value::as_bool).unwrap_or(false);
    let brush = if params.get("brush").is_some() {
        Some(parse_brush(params)?)
    } else {
        None
    };
    let found_origin = match (&map_path, origin) {
        (Some(path), None) => map_origin(world, path),
        _ => None,
    };
    let request = SetRequest {
        map: map_path,
        rules: rules_path,
        origin,
        found_origin,
        discard,
        brush,
    };
    let mut mode = world.remove_resource::<Mode2d>().unwrap_or_default();
    let result = apply_set(&mut mode, &request, params);
    world.insert_resource(mode);
    result.map_err(|e| mode2d_error(&e))?;
    Ok(state_json(world))
}

/// `editor.tile_stroke`: one stroke over `cells` with the current brush (or `erase: true`).
fn tile_stroke(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let cells = params
        .get("cells")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("`cells: [[x, y], …]` is required"))?;
    if cells.len() > MAX_STROKE_CELLS {
        return Err(bad(format!("at most {MAX_STROKE_CELLS} cells per stroke")));
    }
    let cells: Vec<(u32, u32)> = cells
        .iter()
        .map(|c| match c.as_array().map(Vec::as_slice) {
            Some([x, y]) => x.as_u64().zip(y.as_u64()).map(|(x, y)| {
                (
                    u32::try_from(x).unwrap_or(u32::MAX),
                    u32::try_from(y).unwrap_or(u32::MAX),
                )
            }),
            _ => None,
        })
        .collect::<Option<_>>()
        .ok_or_else(|| bad("every cell is [x, y]"))?;
    let erase = params.get("erase").and_then(Value::as_bool).unwrap_or(false);
    let options = StrokeOptions::parse(params).map_err(|e| mode2d_error(&e))?;
    let mut mode = world.remove_resource::<Mode2d>().unwrap_or_default();
    // The call's own tool settings apply to this stroke only.
    let saved = options.swap_into(&mut mode);
    let tool = mode.tool;
    let result = if tool == Tool2d::Pick {
        let picked = match cells.as_slice() {
            [cell] => mode.pick_cell(*cell),
            [a, b] => mode.copy_stamp(*a, *b),
            _ => Err(Mode2dError::BadParams(
                "`pick` takes one cell (a tile) or two (a stamp)".into(),
            )),
        };
        picked.map(|()| Vec::new())
    } else {
        mode.tool_cells(tool, &cells)
            .and_then(|expanded| mode.paint_tool_now(tool, &expanded, erase))
    };
    // A pick keeps its brush; everything else the call set goes back.
    saved.restore(&mut mode, tool == Tool2d::Pick);
    let command = match &result {
        Ok(changes) if !changes.is_empty() => mode.doc.as_ref().map(|doc| TilePaintCommand {
            path: doc.path.clone(),
            layer: mode.layer,
            changes: changes.clone(),
            erase,
        }),
        _ => None,
    };
    world.insert_resource(mode);
    let changes = result.map_err(|e| mode2d_error(&e))?;
    if let Some(command) = command {
        push_stroke(world, command);
    }
    let undo_depth = world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth);
    let brush = world
        .get_resource::<Mode2d>()
        .map_or(Value::Null, |m| brush_json(&m.brush));
    Ok(json!({ "changed": changes.len(), "undo_depth": undo_depth, "tool": tool.name(), "brush": brush }))
}

/// The per-call tool settings of `editor.tile_stroke`.
struct StrokeOptions {
    tool: Option<Tool2d>,
    random: Option<Option<RandomBrush>>,
    mode: Option<AutotileMode>,
    filled: Option<bool>,
    contiguous: Option<bool>,
}

/// The mode's own settings while a call's options are swapped in.
struct SavedOptions {
    brush: Brush2d,
    tool: Tool2d,
    random: Option<RandomBrush>,
    mode: AutotileMode,
    filled: bool,
    contiguous: bool,
}

impl StrokeOptions {
    fn parse(params: &Value) -> Result<Self, Mode2dError> {
        Ok(Self {
            tool: params.get("tool").map(parse_tool).transpose()?,
            random: params.get("random").map(parse_random).transpose()?,
            mode: params.get("mode").map(parse_autotile_mode).transpose()?,
            filled: bool_param(params, "filled")?,
            contiguous: bool_param(params, "contiguous")?,
        })
    }

    fn swap_into(self, mode: &mut Mode2d) -> SavedOptions {
        let saved = SavedOptions {
            brush: mode.brush.clone(),
            tool: mode.tool,
            random: mode.random.clone(),
            mode: mode.autotile_mode,
            filled: mode.rect_filled,
            contiguous: mode.bucket_contiguous,
        };
        mode.tool = self.tool.unwrap_or(mode.tool);
        if let Some(random) = self.random {
            // A call's own random brush paints random tiles whatever the brush is.
            if let Some(first) = random.as_ref().and_then(|r| r.tiles.first())
                && !matches!(mode.brush, Brush2d::Tile(_))
            {
                mode.brush = Brush2d::Tile(first.raw);
            }
            mode.random = random;
        }
        mode.autotile_mode = self.mode.unwrap_or(mode.autotile_mode);
        mode.rect_filled = self.filled.unwrap_or(mode.rect_filled);
        mode.bucket_contiguous = self.contiguous.unwrap_or(mode.bucket_contiguous);
        saved
    }
}

impl SavedOptions {
    fn restore(self, mode: &mut Mode2d, picked: bool) {
        mode.tool = self.tool;
        if !picked {
            // A pick keeps the brush it picked (and its random brush stays off).
            mode.brush = self.brush;
            mode.random = self.random;
        }
        mode.autotile_mode = self.mode;
        mode.rect_filled = self.filled;
        mode.bucket_contiguous = self.contiguous;
    }
}

/// `editor.tile_stamp`: copy a rectangle of the active layer into the stamp, rotate or mirror
/// it, or drop it.
fn tile_stamp(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = str_param(params, "action").ok_or_else(|| bad("`action` is copy, transform or clear"))?;
    let mut mode = world.remove_resource::<Mode2d>().unwrap_or_default();
    let result = (|| -> Result<(), Mode2dError> {
        match action {
            "copy" => {
                let rect: Vec<u32> = params
                    .get("rect")
                    .and_then(Value::as_array)
                    .and_then(|a| {
                        a.iter()
                            .map(|v| v.as_u64().map(|v| u32::try_from(v).unwrap_or(u32::MAX)))
                            .collect::<Option<Vec<u32>>>()
                    })
                    .filter(|v| v.len() == 4)
                    .ok_or_else(|| {
                        Mode2dError::BadParams("`rect` is [x0, y0, x1, y1] (cells, inclusive)".into())
                    })?;
                mode.copy_stamp((rect[0], rect[1]), (rect[2], rect[3]))
            }
            "transform" => {
                let op = str_param(params, "op").and_then(StampOp::parse).ok_or_else(|| {
                    Mode2dError::BadParams(
                        "`op` is rotate_cw, rotate_ccw, rotate_180, flip_x or flip_y".into(),
                    )
                })?;
                mode.transform_stamp(op)
            }
            "clear" => {
                mode.clear_stamp();
                Ok(())
            }
            other => Err(Mode2dError::BadParams(format!(
                "unknown action `{other}` (copy, transform, clear)"
            ))),
        }
    })();
    let stamp = mode
        .stamp
        .as_ref()
        .map(|s| json!({ "width": s.width, "height": s.height, "cells": s.cells }));
    let brush = brush_json(&mode.brush);
    world.insert_resource(mode);
    result.map_err(|e| mode2d_error(&e))?;
    Ok(json!({ "stamp": stamp, "brush": brush }))
}

/// `editor.tilemap_save`.
fn tilemap_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")
        .map(|p| resolve_path(world, "path", p))
        .transpose()?;
    let mut mode = world.remove_resource::<Mode2d>().unwrap_or_default();
    let result = save_tilemap(&mut mode, path.as_deref());
    world.insert_resource(mode);
    let (path, bytes) = result.map_err(|e| mode2d_error(&e))?;
    Ok(json!({ "path": path.display().to_string(), "bytes": bytes }))
}

const fn param(
    name: &'static str,
    kind: &'static str,
    required: bool,
    description: &'static str,
) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required,
        description,
    }
}

/// `editor.mode2d_set` parameters.
const SET_PARAMS: &[ParamSchema] = &[
    param("active", "boolean", false, "Turn the mode on or off."),
    param(
        "map",
        "string",
        false,
        "A .tilemap.ron, absolute or project-relative.",
    ),
    param(
        "discard",
        "boolean",
        false,
        "Drop unsaved edits of the loaded map.",
    ),
    param(
        "rules",
        "string",
        false,
        "An .autotile.ron path, or null to clear.",
    ),
    param("layer", "string", false, "Tile layer name or index."),
    param(
        "brush",
        "object",
        false,
        "{tile: gid, flip?}, {terrain: name}, {erase: true} or {stamp: true}.",
    ),
    param("grid", "boolean", false, "Draw the tile grid."),
    param(
        "clock",
        "object",
        false,
        "The pane's tile animation clock: {play?: bool, reset?: bool, step_ms?: 0..=3600000}.",
    ),
    param("tool", "string", false, "paint, line, rect, bucket or pick."),
    param(
        "rect_filled",
        "boolean",
        false,
        "The rectangle tool fills (else outlines).",
    ),
    param(
        "bucket_contiguous",
        "boolean",
        false,
        "The bucket fills the connected region (else every matching cell).",
    ),
    param(
        "random",
        "object",
        false,
        "The random scatter brush {tiles: [{tile, weight?, flip?}], nothing?: 0..1, seed?}, or null to turn it off.",
    ),
    param("autotile_mode", "string", false, "connect or path."),
    param(
        "reload_images",
        "boolean",
        false,
        "Decode the map's tileset images again.",
    ),
    param(
        "origin",
        "array",
        false,
        "World [x, y, z] of the map's bottom-left corner.",
    ),
];

/// `editor.tile_stroke` parameters.
const STROKE_PARAMS: &[ParamSchema] = &[
    param("cells", "array", true, "[[x, y], …], row 0 = top; at most 65536."),
    param("erase", "boolean", false, "Erase instead of painting the brush."),
    param(
        "tool",
        "string",
        false,
        "paint (cells as given), line or rect (two cells), bucket (one cell), pick (one cell: its tile \
         becomes the brush; two: the rectangle becomes the stamp). Default: the mode's tool.",
    ),
    param(
        "filled",
        "boolean",
        false,
        "rect: fill (true) or outline (false).",
    ),
    param(
        "contiguous",
        "boolean",
        false,
        "bucket: the connected region (true) or every matching cell of the layer (false).",
    ),
    param(
        "random",
        "object",
        false,
        "A random scatter brush for this stroke: {tiles: [{tile, weight?, flip?}], nothing?: 0..1, seed?}, or null.",
    ),
    param(
        "mode",
        "string",
        false,
        "Terrain brush: connect (joins every same-terrain neighbor) or path (joins only this stroke's cells).",
    ),
];

/// `editor.tile_stamp` parameters.
const STAMP_PARAMS: &[ParamSchema] = &[
    param("action", "string", true, "copy, transform or clear."),
    param(
        "rect",
        "array",
        false,
        "copy: [x0, y0, x1, y1], cells of the active layer, inclusive, row 0 = top.",
    ),
    param(
        "op",
        "string",
        false,
        "transform: rotate_cw, rotate_ccw, rotate_180, flip_x or flip_y.",
    ),
];

/// `editor.tilemap_save` parameters.
const SAVE_PARAMS: &[ParamSchema] = &[param(
    "path",
    "string",
    false,
    "Another .tilemap.ron to write instead.",
)];

/// The 2D mode's agent methods (with the new-map, tileset-binding and rule-editor methods of
/// [`tilemap_new`] and [`autotile_editor`]).
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    let mut methods = core_methods();
    methods.extend(tilemap_new::methods());
    methods.extend(autotile_editor::methods());
    methods.extend(layers::methods());
    methods.extend(guides::methods());
    methods.extend(patterns::methods());
    methods
}

fn core_methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.mode2d_get",
                tool: "mode2d_get",
                description: "The editor's 2D mode: on/off, the map document (path, size, tile size, layers, \
                    tilesets with their first gid and image state, unsaved edits), the rule set (path, terrains), \
                    layer, brush, grid, hover cell, map origin, the tiles the front pane drew last frame and the \
                    undo depth.",
                params: &[],
                result: "{ active, map, rules, layer, brush, grid, hover, origin, view, stamp, clock, tool, rect_filled, bucket_contiguous, random, autotile_mode, undo_depth }",
                mutating: false,
                permission: Permission::Read,
            },
            mode2d_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.mode2d_set",
                tool: "mode2d_set",
                description: "Changes the 2D mode: load a .tilemap.ron (`map`) and an .autotile.ron (`rules`, null \
                    clears), pick the `layer`, the `brush`, the paint `tool` (with `rect_filled`, \
                    `bucket_contiguous`), the `random` scatter brush, the terrain `autotile_mode` (connect or path), \
                    the grid, the map `origin`, and turn it on or off \
                    (turning it on, or loading a map, frames the map in the front pane). Errors in \
                    data.mode2d_error: mode2d_bad_params, mode2d_not_tilemap_ron, mode2d_load_failed, \
                    mode2d_bad_rules, mode2d_unsaved, mode2d_no_map, mode2d_no_rules, mode2d_bad_layer, \
                    mode2d_unknown_terrain.",
                params: SET_PARAMS,
                result: "the new state (as editor.mode2d_get)",
                mutating: true,
                permission: Permission::EditScene,
            },
            mode2d_set as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tile_stroke",
                tool: "tile_stroke",
                description: "Paints the brush (or erases) over cells of the 2D mode's map as one undo entry \
                    (editor.undo restores it exactly), with a tool (paint, line, rect, bucket, pick), an optional \
                    random scatter brush and the terrain mode (connect re-resolves the painted cells' neighbours, \
                    path joins only the stroke's own cells). \
                    Errors in data.mode2d_error: mode2d_bad_params, mode2d_no_map, mode2d_no_rules, \
                    mode2d_bad_layer, mode2d_outside_map, mode2d_bad_gid, mode2d_unknown_terrain, mode2d_bad_rules.",
                params: STROKE_PARAMS,
                result: "{ changed, undo_depth, tool, brush }",
                mutating: true,
                permission: Permission::EditScene,
            },
            tile_stroke as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tile_stamp",
                tool: "tile_stamp",
                description: "Multi-cell stamps for the 2D mode: copy a rectangle of the active layer (and select \
                    the stamp brush, which pastes it centered on every painted cell), rotate or mirror it (the \
                    tiles' flip bits turn with it), or clear it. Paste with editor.tile_stroke; one stroke is one \
                    undo entry. Errors in data.mode2d_error: mode2d_bad_params, mode2d_no_map, mode2d_bad_stamp, \
                    mode2d_no_stamp.",
                params: STAMP_PARAMS,
                result: "{ stamp: { width, height, cells } | null, brush }",
                mutating: true,
                permission: Permission::EditScene,
            },
            tile_stamp as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tilemap_save",
                tool: "tilemap_save",
                description: "Writes the 2D mode's map to its .tilemap.ron (or `path`) atomically; the asset watcher \
                    re-cooks it. Errors in data.mode2d_error: mode2d_no_map, mode2d_not_tilemap_ron, \
                    mode2d_save_failed.",
                params: SAVE_PARAMS,
                result: "{ path, bytes }",
                mutating: true,
                permission: Permission::EditScene,
            },
            tilemap_save as Handler,
        ),
    ]
}

#[cfg(test)]
mod input_tests;
#[cfg(test)]
mod keys_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tools_tests;
