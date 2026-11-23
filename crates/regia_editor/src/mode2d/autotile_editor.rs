// SPDX-License-Identifier: MPL-2.0
//! The `.autotile.ron` rule editor of the 2D tab (`docs/specs/86-2d-toolkit.md` § 6 "As built",
//! ADR-7742).
//!
//! [`AutotileEditor`] (a world resource) holds one rule-set document: its path, the
//! [`AutoTileSet`], a dirty flag, the selected terrain and rule, and a scratch preview. Every edit
//! is an [`AutotileOp`]: [`apply_op`] returns the edited set, validated by the format's own
//! `AutoTileSet::validate`, or refuses it and changes nothing. One op is one
//! [`AutotileEditCommand`] on the editor's undo stack (the set before and after). Saving writes the
//! format's canonical text (`AutoTileSet::to_ron`) atomically.
//!
//! When the 2D mode's terrain brush uses the same file, each edit reaches the brush at once, so
//! the next stroke paints with the edited rules.
//!
//! **Preview.** [`preview`] paints a terrain over a set of cells of a scratch map with
//! `regia_2d::autotile::BoundAutoTiles::paint`, the function the runtime, the agent's
//! `tilemap.paint` and the 2D brush call. The scratch map's tileset is the loaded map's tileset of
//! the rule set's name, or a placeholder with as many tiles as the rules name.
//!
//! Agent methods (`book/agents/editor-2d.md`): `editor.autotile_get`, `editor.autotile_open`,
//! `editor.autotile_new`, `editor.autotile_edit`, `editor.autotile_save`,
//! `editor.autotile_preview`. Errors are F0002 with `data.mode2d_error` (codes on the page).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use regia_2d::autotile::{
    AUTOTILE_FORMAT_VERSION, AutoTileSet, Border, Layout, PatternRule, Terrain, TerrainBrush,
};
use regia_2d::tilemap::{
    GID_MASK, MapLayer, Orientation, TILEMAP_FORMAT_VERSION, TileLayer, TileMapAsset, TilesetDef,
};
use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::{Mode2d, param, read_limited, resolve_path};
use crate::command::{EditorCommand, UndoStack};
use crate::viewport_agent::{Handler, str_param};

/// Default preview grid (cells).
pub const PREVIEW_SIZE: [u32; 2] = [8, 6];
/// Largest preview side (cells).
pub const MAX_PREVIEW_SIDE: u32 = 64;
/// The pattern a new rule starts with: the center belongs, every neighbor is either.
pub const NEW_RULE_PATTERN: &str = "???/?#?/???";

/// Rule-editor errors; [`AutotileEditError::code`] is the stable `data.mode2d_error` code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AutotileEditError {
    /// A parameter is missing or malformed.
    #[error("{0}")]
    BadParams(String),
    /// No rule set is open in the rule editor.
    #[error("no rule set is open in the rule editor (editor.autotile_open or editor.autotile_new)")]
    NoDocument,
    /// The edit would make an invalid rule set (the format's validation message).
    #[error("the edit makes an invalid rule set: {0}")]
    Invalid(String),
    /// The file could not be read or parsed.
    #[error("cannot open the rule set: {0}")]
    Load(String),
    /// The file could not be written.
    #[error("cannot save the rule set: {0}")]
    Save(String),
    /// The open rule set has unsaved edits.
    #[error("the open rule set has unsaved edits (save it, or pass `discard: true`)")]
    Unsaved,
    /// The preview could not bind or paint.
    #[error("cannot preview: {0}")]
    Preview(String),
}

impl AutotileEditError {
    /// The stable error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadParams(_) => "autotile_bad_params",
            Self::NoDocument => "autotile_no_document",
            Self::Invalid(_) => "autotile_invalid",
            Self::Load(_) => "autotile_load_failed",
            Self::Save(_) => "autotile_save_failed",
            Self::Unsaved => "autotile_unsaved",
            Self::Preview(_) => "autotile_preview_failed",
        }
    }
}

/// The open rule set.
#[derive(Debug, Clone, PartialEq)]
pub struct AutotileDoc {
    /// The `.autotile.ron` file (written on save; a New document has no file yet).
    pub path: PathBuf,
    /// The rules.
    pub set: AutoTileSet,
    /// Edited since open or the last save.
    pub dirty: bool,
}

/// The scratch preview: a grid and the cells painted with the selected terrain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewGrid {
    /// Grid size in cells.
    pub size: [u32; 2],
    /// Painted cells `(x, y)`, row 0 at the top.
    pub cells: BTreeSet<(u32, u32)>,
}

impl Default for PreviewGrid {
    fn default() -> Self {
        Self {
            size: PREVIEW_SIZE,
            cells: BTreeSet::new(),
        }
    }
}

/// The rule editor's state (a world resource).
#[derive(Resource, Debug, Default)]
pub struct AutotileEditor {
    /// The open rule set.
    pub doc: Option<AutotileDoc>,
    /// The selected terrain (index).
    pub terrain: usize,
    /// The selected rule of a `Rules` terrain, or entry of a preset layout (index).
    pub rule: usize,
    /// The scratch preview.
    pub preview: PreviewGrid,
}

/// The kind of layout a new terrain gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutKind {
    /// 3×3 pattern rules (one rule to start).
    Rules,
    /// 16 edge-mask tiles.
    Edge16,
    /// 47 blob tiles.
    Blob47,
    /// 16 corner tiles.
    WangCorner,
}

impl LayoutKind {
    /// Parses `rules`, `edge16`, `blob47` or `wang_corner`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "rules" => Some(Self::Rules),
            "edge16" => Some(Self::Edge16),
            "blob47" => Some(Self::Blob47),
            "wang_corner" => Some(Self::WangCorner),
            _ => None,
        }
    }
}

/// One edit of a rule set. `terrain`, `rule` and `index` are indices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutotileOp {
    /// Sets cell `cell` (0..9, row by row, top row first) of a rule's pattern to `#`, `.` or `?`.
    SetPatternCell {
        /// Terrain.
        terrain: usize,
        /// Rule.
        rule: usize,
        /// Cell 0..9.
        cell: usize,
        /// `#`, `.` or `?`.
        value: char,
    },
    /// Replaces a rule's whole pattern.
    SetPattern {
        /// Terrain.
        terrain: usize,
        /// Rule.
        rule: usize,
        /// Nine cells (whitespace and `/` ignored).
        pattern: String,
    },
    /// Sets a rule's output tile and flips.
    SetRuleTile {
        /// Terrain.
        terrain: usize,
        /// Rule.
        rule: usize,
        /// Local tile id.
        tile: u32,
        /// `[flip_x, flip_y, diagonal]`.
        flips: [bool; 3],
    },
    /// Inserts a rule at `at` (default: the end).
    AddRule {
        /// Terrain.
        terrain: usize,
        /// Position.
        at: Option<usize>,
        /// Pattern.
        pattern: String,
        /// Output tile.
        tile: u32,
    },
    /// Removes a rule (a terrain keeps at least one).
    RemoveRule {
        /// Terrain.
        terrain: usize,
        /// Rule.
        rule: usize,
    },
    /// Moves a rule to another position (the first match wins, so order matters).
    MoveRule {
        /// Terrain.
        terrain: usize,
        /// Current index.
        from: usize,
        /// New index.
        to: usize,
    },
    /// Sets or clears a `Rules` terrain's fallback tile.
    SetFallback {
        /// Terrain.
        terrain: usize,
        /// Local tile id, or none.
        tile: Option<u32>,
    },
    /// Sets entry `index` of an `Edge16`, `Blob47` or `WangCorner` layout.
    SetLayoutTile {
        /// Terrain.
        terrain: usize,
        /// Entry (the mask index).
        index: usize,
        /// Local tile id.
        tile: u32,
    },
    /// Appends a terrain; its tiles start at the lowest local ids no other terrain uses.
    AddTerrain {
        /// Name (unique).
        name: String,
        /// Layout.
        layout: LayoutKind,
    },
    /// Removes a terrain (a set keeps at least one).
    RemoveTerrain {
        /// Terrain.
        terrain: usize,
    },
    /// Renames a terrain.
    RenameTerrain {
        /// Terrain.
        terrain: usize,
        /// New name.
        name: String,
    },
    /// Sets how cells off the map count.
    SetBorder(Border),
    /// Sets the variant seed.
    SetSeed(u32),
    /// Sets the tileset name the rules bind to.
    SetTileset(String),
}

/// The nine cells of `pattern` (whitespace and `/` dropped), or `None` when it does not hold nine.
#[must_use]
pub fn pattern_cells(pattern: &str) -> Option<[char; 9]> {
    let cells: Vec<char> = pattern
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '/')
        .collect();
    cells.try_into().ok()
}

/// The canonical text of nine pattern cells: three rows joined by ` / `
/// (`"?#? / ### / ?.?"`, spec 86 § 2.1).
#[must_use]
pub fn pattern_text(cells: [char; 9]) -> String {
    cells
        .chunks(3)
        .map(|row| row.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// The next pattern value a click on a grid cell gives: `?` → `#` → `.` → `?`. The center has no
/// `.` (the format refuses it): `?` ↔ `#`.
#[must_use]
pub fn next_cell_value(value: char, center: bool) -> char {
    match (value, center) {
        ('?', _) => '#',
        ('#', false) => '.',
        _ => '?',
    }
}

/// The lowest `count` local ids that no terrain of `set` uses.
fn free_tiles(set: &AutoTileSet, count: usize) -> Vec<u32> {
    let used: BTreeSet<u32> = set.terrains.iter().flat_map(Terrain::member_tiles).collect();
    (0u32..).filter(|t| !used.contains(t)).take(count).collect()
}

fn check_terrain(set: &AutoTileSet, i: usize) -> Result<usize, AutotileEditError> {
    if i < set.terrains.len() {
        Ok(i)
    } else {
        Err(AutotileEditError::BadParams(format!(
            "terrain {i} does not exist ({} in the set)",
            set.terrains.len()
        )))
    }
}

fn rules_mut(set: &mut AutoTileSet, t: usize) -> Result<&mut Vec<PatternRule>, AutotileEditError> {
    let t = check_terrain(set, t)?;
    let terrain = &mut set.terrains[t];
    match &mut terrain.layout {
        Layout::Rules(rules) => Ok(rules),
        other => Err(AutotileEditError::BadParams(format!(
            "terrain `{}` is a {} layout, not rules",
            terrain.name,
            other.name()
        ))),
    }
}

fn check_rule(rules: &[PatternRule], r: usize) -> Result<usize, AutotileEditError> {
    if r < rules.len() {
        Ok(r)
    } else {
        Err(AutotileEditError::BadParams(format!(
            "rule {r} does not exist ({} in the terrain)",
            rules.len()
        )))
    }
}

/// `set` with `op` applied and validated (`AutoTileSet::validate`); the input is not changed.
///
/// # Errors
///
/// [`AutotileEditError::BadParams`] for an index out of range or an op that does not fit the
/// terrain's layout; [`AutotileEditError::Invalid`] when the result fails validation.
pub fn apply_op(set: &AutoTileSet, op: &AutotileOp) -> Result<AutoTileSet, AutotileEditError> {
    let bad = AutotileEditError::BadParams;
    let mut out = set.clone();
    let terrain_count = out.terrains.len();
    let check_terrain = |i: usize| check_terrain(set, i);
    let rules_of = rules_mut;
    let rule_index = check_rule;
    match op {
        AutotileOp::SetPatternCell {
            terrain,
            rule,
            cell,
            value,
        } => {
            let rules = rules_of(&mut out, *terrain)?;
            let r = rule_index(rules, *rule)?;
            if *cell >= 9 || !matches!(value, '#' | '.' | '?') {
                return Err(bad("`cell` is 0..9 and `value` is #, . or ?".into()));
            }
            let mut cells = pattern_cells(&rules[r].pattern)
                .ok_or_else(|| bad(format!("rule {r} has no nine-cell pattern")))?;
            cells[*cell] = *value;
            rules[r].pattern = pattern_text(cells);
        }
        AutotileOp::SetPattern {
            terrain,
            rule,
            pattern,
        } => {
            let rules = rules_of(&mut out, *terrain)?;
            let r = rule_index(rules, *rule)?;
            rules[r].pattern = pattern_cells(pattern).map_or_else(|| pattern.clone(), pattern_text);
        }
        AutotileOp::SetRuleTile {
            terrain,
            rule,
            tile,
            flips,
        } => {
            let rules = rules_of(&mut out, *terrain)?;
            let r = rule_index(rules, *rule)?;
            let rule = &mut rules[r];
            rule.tile = *tile;
            [rule.flip_x, rule.flip_y, rule.diagonal] = *flips;
        }
        AutotileOp::AddRule {
            terrain,
            at,
            pattern,
            tile,
        } => {
            let rules = rules_of(&mut out, *terrain)?;
            let at = at.unwrap_or(rules.len());
            if at > rules.len() {
                return Err(bad(format!("`at` is 0..={}", rules.len())));
            }
            rules.insert(
                at,
                PatternRule {
                    pattern: pattern_cells(pattern).map_or_else(|| pattern.clone(), pattern_text),
                    tile: *tile,
                    flip_x: false,
                    flip_y: false,
                    diagonal: false,
                },
            );
        }
        AutotileOp::RemoveRule { terrain, rule } => {
            let rules = rules_of(&mut out, *terrain)?;
            let r = rule_index(rules, *rule)?;
            if rules.len() == 1 {
                return Err(bad("a rules terrain keeps at least one rule".into()));
            }
            rules.remove(r);
        }
        AutotileOp::MoveRule { terrain, from, to } => {
            let rules = rules_of(&mut out, *terrain)?;
            let from = rule_index(rules, *from)?;
            let to = rule_index(rules, *to)?;
            let rule = rules.remove(from);
            rules.insert(to, rule);
        }
        AutotileOp::SetFallback { terrain, tile } => {
            let t = check_terrain(*terrain)?;
            if !matches!(out.terrains[t].layout, Layout::Rules(_)) {
                return Err(bad("only a rules terrain has a fallback".into()));
            }
            out.terrains[t].fallback = *tile;
        }
        AutotileOp::SetLayoutTile { terrain, index, tile } => {
            let t = check_terrain(*terrain)?;
            match &mut out.terrains[t].layout {
                Layout::Edge16(tiles) | Layout::Blob47(tiles) | Layout::WangCorner(tiles) => {
                    let len = tiles.len();
                    *tiles.get_mut(*index).ok_or_else(|| {
                        bad(format!("entry {index} does not exist ({len} in the layout)"))
                    })? = *tile;
                }
                Layout::Rules(_) => return Err(bad("a rules terrain has rules, not entries".into())),
            }
        }
        AutotileOp::AddTerrain { name, layout } => {
            let count = match layout {
                LayoutKind::Rules => 1,
                LayoutKind::Edge16 | LayoutKind::WangCorner => 16,
                LayoutKind::Blob47 => 47,
            };
            let tiles = free_tiles(&out, count);
            let layout = match layout {
                LayoutKind::Rules => Layout::Rules(vec![PatternRule {
                    pattern: pattern_cells(NEW_RULE_PATTERN)
                        .map_or_else(|| NEW_RULE_PATTERN.into(), pattern_text),
                    tile: tiles[0],
                    flip_x: false,
                    flip_y: false,
                    diagonal: false,
                }]),
                LayoutKind::Edge16 => Layout::Edge16(tiles),
                LayoutKind::Blob47 => Layout::Blob47(tiles),
                LayoutKind::WangCorner => Layout::WangCorner(tiles),
            };
            out.terrains.push(Terrain {
                name: name.clone(),
                layout,
                fallback: None,
                members: Vec::new(),
                variants: Default::default(),
            });
        }
        AutotileOp::RemoveTerrain { terrain } => {
            let t = check_terrain(*terrain)?;
            if terrain_count == 1 {
                return Err(bad("a rule set keeps at least one terrain".into()));
            }
            out.terrains.remove(t);
        }
        AutotileOp::RenameTerrain { terrain, name } => {
            let t = check_terrain(*terrain)?;
            out.terrains[t].name = name.clone();
        }
        AutotileOp::SetBorder(border) => out.border = *border,
        AutotileOp::SetSeed(seed) => out.seed = *seed,
        AutotileOp::SetTileset(name) => out.tileset = name.clone(),
    }
    out.validate()
        .map_err(|e| AutotileEditError::Invalid(e.to_string()))?;
    Ok(out)
}

/// A new rule set for tileset `tileset`: the current format version, one rules terrain named
/// `terrain` whose one rule writes tile 0.
#[must_use]
pub fn new_rule_set(tileset: &str) -> AutoTileSet {
    AutoTileSet {
        version: AUTOTILE_FORMAT_VERSION,
        tileset: tileset.to_string(),
        seed: 0,
        border: Border::Same,
        terrains: vec![Terrain {
            name: "terrain".into(),
            layout: Layout::Rules(vec![PatternRule {
                pattern: pattern_cells(NEW_RULE_PATTERN)
                    .map_or_else(|| NEW_RULE_PATTERN.into(), pattern_text),
                tile: 0,
                flip_x: false,
                flip_y: false,
                diagonal: false,
            }]),
            fallback: None,
            members: Vec::new(),
            variants: Default::default(),
        }],
    }
}

/// The highest local id `set` names, plus one (at least 1).
fn tiles_named(set: &AutoTileSet) -> u32 {
    set.terrains
        .iter()
        .flat_map(Terrain::member_tiles)
        .max()
        .map_or(1, |t| t.saturating_add(1))
}

/// The scratch map of a preview: `size` cells of 16 px, one tile layer, the tileset `tileset`
/// (renamed to the rules' tileset, first GID 1) or a placeholder with as many tiles as the rules
/// name.
#[must_use]
pub fn scratch_map(set: &AutoTileSet, tileset: Option<&TilesetDef>, size: [u32; 2]) -> TileMapAsset {
    let tileset = tileset.cloned().map_or_else(
        || {
            let count = tiles_named(set);
            TilesetDef {
                name: set.tileset.clone(),
                first_gid: 1,
                image: String::new(),
                image_width: 16 * count.min(16),
                image_height: 16 * count.div_ceil(16),
                tile_width: 16,
                tile_height: 16,
                columns: 16,
                tile_count: count,
                spacing: 0,
                margin: 0,
                tiles: Vec::new(),
            }
        },
        |mut t| {
            t.first_gid = 1;
            t.name = set.tileset.clone();
            t
        },
    );
    TileMapAsset {
        version: TILEMAP_FORMAT_VERSION,
        orientation: Orientation::Orthogonal,
        width: size[0],
        height: size[1],
        tile_width: 16,
        tile_height: 16,
        background: None,
        tilesets: vec![tileset],
        layers: vec![MapLayer::Tiles(TileLayer {
            name: "preview".into(),
            visible: true,
            opacity: 1.0,
            offset: [0.0, 0.0],
            data: vec![0; size[0] as usize * size[1] as usize],
            properties: Default::default(),
        })],
        properties: Default::default(),
    }
}

/// Paints terrain `terrain` of `set` over `cells` of a [`scratch_map`] with the runtime's
/// `BoundAutoTiles::paint`; returns the map.
///
/// # Errors
///
/// [`AutotileEditError::BadParams`] for a size outside 1..=[`MAX_PREVIEW_SIDE`] or a cell off the
/// grid, [`AutotileEditError::Preview`] when the rules do not bind to the tileset.
pub fn preview(
    set: &AutoTileSet,
    tileset: Option<&TilesetDef>,
    terrain: usize,
    size: [u32; 2],
    cells: &[(u32, u32)],
) -> Result<TileMapAsset, AutotileEditError> {
    if !size.iter().all(|s| (1..=MAX_PREVIEW_SIDE).contains(s)) {
        return Err(AutotileEditError::BadParams(format!(
            "the preview is 1..={MAX_PREVIEW_SIDE} cells per side"
        )));
    }
    if let Some((x, y)) = cells.iter().find(|(x, y)| *x >= size[0] || *y >= size[1]) {
        return Err(AutotileEditError::BadParams(format!(
            "cell ({x}, {y}) is outside the {}x{} preview",
            size[0], size[1]
        )));
    }
    if terrain >= set.terrains.len() {
        return Err(AutotileEditError::BadParams(format!(
            "terrain {terrain} does not exist"
        )));
    }
    let mut map = scratch_map(set, tileset, size);
    let bound = set
        .bind(&map)
        .map_err(|e| AutotileEditError::Preview(e.to_string()))?;
    bound
        .paint(&mut map, 0, TerrainBrush::Terrain(terrain), cells)
        .map_err(|e| AutotileEditError::Preview(e.to_string()))?;
    Ok(map)
}

/// The tileset of the 2D mode's map that `set` binds to, if a map is loaded and has it.
#[must_use]
pub fn map_tileset(world: &World, set: &AutoTileSet) -> Option<TilesetDef> {
    world
        .get_resource::<Mode2d>()?
        .doc
        .as_ref()?
        .map
        .tilesets
        .iter()
        .find(|t| t.name == set.tileset)
        .cloned()
}

/// One rule-set edit on the undo stack: the set before and after.
#[derive(Debug, Clone)]
pub struct AutotileEditCommand {
    /// The document's path (the command does nothing once another rule set is open).
    pub path: PathBuf,
    /// Before.
    pub before: AutoTileSet,
    /// After.
    pub after: AutoTileSet,
    /// What it did.
    pub what: String,
}

impl AutotileEditCommand {
    fn write(&self, world: &mut World, undo: bool) {
        let set = if undo { &self.before } else { &self.after };
        {
            let Some(mut editor) = world.get_resource_mut::<AutotileEditor>() else {
                return;
            };
            let Some(doc) = editor.doc.as_mut().filter(|d| d.path == self.path) else {
                tracing::warn!(target: "regia_editor::mode2d", "rule edit on {} skipped: another rule set is open", self.path.display());
                return;
            };
            doc.set = set.clone();
            doc.dirty = true;
            let terrains = set.terrains.len();
            if editor.terrain >= terrains {
                editor.terrain = terrains.saturating_sub(1);
            }
        }
        sync_brush_rules(world, &self.path, set);
    }
}

impl EditorCommand for AutotileEditCommand {
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
        self.what.clone()
    }
}

/// The 2D mode's terrain brush follows an edited rule set it uses (same file), when the set still
/// fits the loaded map; a brush terrain that no longer exists falls back to the first terrain.
fn sync_brush_rules(world: &mut World, path: &Path, set: &AutoTileSet) {
    let Some(mut mode) = world.get_resource_mut::<Mode2d>() else {
        return;
    };
    let same = mode
        .rules
        .as_ref()
        .is_some_and(|r| crate::path_norm::same_file(&r.path, path, crate::path_norm::FOLD_CASE));
    if !same {
        return;
    }
    if let Some(doc) = &mode.doc
        && set.bind(&doc.map).is_err()
    {
        return;
    }
    if let Some(rules) = mode.rules.as_mut() {
        rules.set = set.clone();
    }
    if let super::Brush2d::Terrain(name) = &mode.brush
        && set.terrain_index(name).is_none()
    {
        mode.brush = super::Brush2d::Terrain(set.terrains[0].name.clone());
    }
}

fn describe(op: &AutotileOp) -> String {
    match op {
        AutotileOp::SetPatternCell { .. } | AutotileOp::SetPattern { .. } => "Edit rule pattern".into(),
        AutotileOp::SetRuleTile { .. } => "Set rule tile".into(),
        AutotileOp::AddRule { .. } => "Add rule".into(),
        AutotileOp::RemoveRule { .. } => "Remove rule".into(),
        AutotileOp::MoveRule { .. } => "Move rule".into(),
        AutotileOp::SetFallback { .. } => "Set fallback tile".into(),
        AutotileOp::SetLayoutTile { .. } => "Set layout tile".into(),
        AutotileOp::AddTerrain { name, .. } => format!("Add terrain {name}"),
        AutotileOp::RemoveTerrain { .. } => "Remove terrain".into(),
        AutotileOp::RenameTerrain { name, .. } => format!("Rename terrain to {name}"),
        AutotileOp::SetBorder(_) => "Set border".into(),
        AutotileOp::SetSeed(_) => "Set variant seed".into(),
        AutotileOp::SetTileset(_) => "Set rules tileset".into(),
    }
}

/// Applies `op` to the open rule set as one undo entry (applied when pushed); selects what it
/// made (a new rule or terrain). Returns the description.
///
/// # Errors
///
/// [`AutotileEditError::NoDocument`], or the errors of [`apply_op`] (nothing changes).
pub fn edit(world: &mut World, op: &AutotileOp) -> Result<String, AutotileEditError> {
    let editor = world
        .get_resource::<AutotileEditor>()
        .ok_or(AutotileEditError::NoDocument)?;
    let doc = editor.doc.as_ref().ok_or(AutotileEditError::NoDocument)?;
    let after = apply_op(&doc.set, op)?;
    let what = describe(op);
    let command = AutotileEditCommand {
        path: doc.path.clone(),
        before: doc.set.clone(),
        after,
        what: what.clone(),
    };
    let mut boxed: Box<dyn EditorCommand> = Box::new(command);
    boxed.apply(world);
    if let Some(mut editor) = world.get_resource_mut::<AutotileEditor>() {
        match op {
            AutotileOp::AddRule { terrain, at, .. } => {
                editor.terrain = *terrain;
                let len = editor
                    .doc
                    .as_ref()
                    .and_then(|d| d.set.terrains.get(*terrain))
                    .map_or(1, |t| match &t.layout {
                        Layout::Rules(r) => r.len(),
                        _ => 1,
                    });
                editor.rule = at.unwrap_or(len - 1);
            }
            AutotileOp::MoveRule { terrain, to, .. } => {
                editor.terrain = *terrain;
                editor.rule = *to;
            }
            AutotileOp::RemoveRule { rule, .. } => editor.rule = rule.saturating_sub(1),
            AutotileOp::AddTerrain { .. } => {
                editor.terrain = editor.doc.as_ref().map_or(0, |d| d.set.terrains.len() - 1);
                editor.rule = 0;
            }
            _ => {}
        }
    }
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    stack.push(boxed);
    stack.seal();
    Ok(what)
}

fn is_autotile_ron(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_ascii_lowercase().ends_with(".autotile.ron"))
}

fn refuse_unsaved(world: &World, discard: bool) -> Result<(), AutotileEditError> {
    let dirty = world
        .get_resource::<AutotileEditor>()
        .and_then(|e| e.doc.as_ref())
        .is_some_and(|d| d.dirty);
    if dirty && !discard {
        return Err(AutotileEditError::Unsaved);
    }
    Ok(())
}

fn install_doc(world: &mut World, doc: AutotileDoc) {
    let mut editor = world.get_resource_or_insert_with(AutotileEditor::default);
    editor.doc = Some(doc);
    editor.terrain = 0;
    editor.rule = 0;
}

/// Opens `path` (an `.autotile.ron`) in the rule editor (refusing unsaved edits unless `discard`).
///
/// # Errors
///
/// [`AutotileEditError::Unsaved`], [`AutotileEditError::BadParams`] (not an `.autotile.ron`) or
/// [`AutotileEditError::Load`].
pub fn open(world: &mut World, path: &Path, discard: bool) -> Result<(), AutotileEditError> {
    refuse_unsaved(world, discard)?;
    if !is_autotile_ron(path) {
        return Err(AutotileEditError::BadParams(format!(
            "`{}` is not an .autotile.ron",
            path.display()
        )));
    }
    let text = read_limited(path).map_err(AutotileEditError::Load)?;
    let set = AutoTileSet::from_ron(&text).map_err(|e| AutotileEditError::Load(e.to_string()))?;
    install_doc(
        world,
        AutotileDoc {
            path: path.to_path_buf(),
            set,
            dirty: false,
        },
    );
    Ok(())
}

/// Starts a new rule set for `tileset` that saves to `path` (not written until saved).
///
/// # Errors
///
/// [`AutotileEditError::Unsaved`], or [`AutotileEditError::BadParams`] for a path that is not an
/// `.autotile.ron`, an existing file, or an empty tileset name.
pub fn new_document(
    world: &mut World,
    path: &Path,
    tileset: &str,
    discard: bool,
) -> Result<(), AutotileEditError> {
    refuse_unsaved(world, discard)?;
    if !is_autotile_ron(path) {
        return Err(AutotileEditError::BadParams(format!(
            "`{}` is not an .autotile.ron",
            path.display()
        )));
    }
    if path.exists() {
        return Err(AutotileEditError::BadParams(format!(
            "{} already exists (open it instead)",
            path.display()
        )));
    }
    if tileset.is_empty() {
        return Err(AutotileEditError::BadParams(
            "the rules need a tileset name".into(),
        ));
    }
    install_doc(
        world,
        AutotileDoc {
            path: path.to_path_buf(),
            set: new_rule_set(tileset),
            dirty: true,
        },
    );
    Ok(())
}

/// Writes the open rule set to `path` (default: its own) atomically through
/// `AutoTileSet::to_ron`; clears `dirty` when it writes its own path. Returns the path and bytes.
///
/// # Errors
///
/// [`AutotileEditError::NoDocument`], [`AutotileEditError::BadParams`] (not an `.autotile.ron`)
/// or [`AutotileEditError::Save`].
pub fn save(world: &mut World, path: Option<&Path>) -> Result<(PathBuf, usize), AutotileEditError> {
    let mut editor = world
        .get_resource_mut::<AutotileEditor>()
        .ok_or(AutotileEditError::NoDocument)?;
    let doc = editor.doc.as_mut().ok_or(AutotileEditError::NoDocument)?;
    let target = path.map_or_else(|| doc.path.clone(), Path::to_path_buf);
    if !is_autotile_ron(&target) {
        return Err(AutotileEditError::BadParams(format!(
            "`{}` is not an .autotile.ron",
            target.display()
        )));
    }
    let text = doc
        .set
        .to_ron()
        .map_err(|e| AutotileEditError::Save(e.to_string()))?;
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| AutotileEditError::Save("the path has no file name".into()))?;
    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| AutotileEditError::Save(format!("{}: {e}", parent.display())))?;
    }
    let tmp = target.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text.as_bytes())
        .map_err(|e| AutotileEditError::Save(format!("{}: {e}", tmp.display())))?;
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(AutotileEditError::Save(format!("{}: {e}", target.display())));
    }
    if crate::path_norm::same_file(&target, &doc.path, crate::path_norm::FOLD_CASE) {
        doc.dirty = false;
    }
    Ok((target, text.len()))
}

/// Paints the editor's preview cells with the selected terrain ([`preview`]).
///
/// # Errors
///
/// [`AutotileEditError::NoDocument`] or the errors of [`preview`].
pub fn editor_preview(world: &World) -> Result<TileMapAsset, AutotileEditError> {
    let editor = world
        .get_resource::<AutotileEditor>()
        .ok_or(AutotileEditError::NoDocument)?;
    let doc = editor.doc.as_ref().ok_or(AutotileEditError::NoDocument)?;
    let cells: Vec<(u32, u32)> = editor.preview.cells.iter().copied().collect();
    preview(
        &doc.set,
        map_tileset(world, &doc.set).as_ref(),
        editor.terrain,
        editor.preview.size,
        &cells,
    )
}

// ---- Agent methods ------------------------------------------------------------------------

fn rpc(error: &AutotileEditError) -> RpcError {
    let mut rpc = RpcError::invalid_params(error.to_string());
    if let Some(data) = rpc.data.as_mut() {
        data["mode2d_error"] = json!(error.code());
    }
    rpc
}

fn bad(message: impl Into<String>) -> RpcError {
    rpc(&AutotileEditError::BadParams(message.into()))
}

fn rule_json(rule: &PatternRule) -> Value {
    json!({
        "pattern": rule.pattern,
        "tile": rule.tile,
        "flip": { "x": rule.flip_x, "y": rule.flip_y, "diagonal": rule.diagonal },
    })
}

fn state_json(world: &World) -> Value {
    let editor = world.get_resource::<AutotileEditor>();
    let doc = editor.and_then(|e| e.doc.as_ref()).map(|d| {
        json!({
            "path": d.path.display().to_string(),
            "dirty": d.dirty,
            "version": d.set.version,
            "tileset": d.set.tileset,
            "seed": d.set.seed,
            "border": match d.set.border { Border::Same => "same", Border::Empty => "empty" },
            "terrains": d.set.terrains.iter().map(|t| {
                let (rules, tiles) = match &t.layout {
                    Layout::Rules(r) => (r.iter().map(rule_json).collect::<Vec<_>>(), Vec::new()),
                    Layout::Edge16(v) | Layout::Blob47(v) | Layout::WangCorner(v) => (Vec::new(), v.clone()),
                };
                json!({ "name": t.name, "layout": t.layout.name(), "rules": rules, "tiles": tiles, "fallback": t.fallback })
            }).collect::<Vec<_>>(),
        })
    });
    let undo_depth = world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth);
    json!({
        "doc": doc,
        "terrain": editor.map_or(0, |e| e.terrain),
        "rule": editor.map_or(0, |e| e.rule),
        "preview": editor.map(|e| json!({
            "size": e.preview.size,
            "cells": e.preview.cells.iter().map(|(x, y)| [*x, *y]).collect::<Vec<_>>(),
        })),
        "undo_depth": undo_depth,
    })
}

/// `editor.autotile_get`.
fn autotile_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(state_json(world))
}

/// `editor.autotile_open`.
fn autotile_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path").ok_or_else(|| bad("`path` is required"))?;
    let path = resolve_path(world, "path", path)?;
    let discard = params.get("discard").and_then(Value::as_bool).unwrap_or(false);
    open(world, &path, discard).map_err(|e| rpc(&e))?;
    Ok(state_json(world))
}

/// `editor.autotile_new`.
fn autotile_new(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path").ok_or_else(|| bad("`path` is required"))?;
    let path = resolve_path(world, "path", path)?;
    // Default tileset: the first one of the 2D mode's map.
    let tileset = match str_param(params, "tileset") {
        Some(name) => name.to_string(),
        None => world
            .get_resource::<Mode2d>()
            .and_then(|m| m.doc.as_ref())
            .and_then(|d| d.map.tilesets.first())
            .map(|t| t.name.clone())
            .ok_or_else(|| bad("`tileset` is required when no map with a tileset is loaded"))?,
    };
    let discard = params.get("discard").and_then(Value::as_bool).unwrap_or(false);
    new_document(world, &path, &tileset, discard).map_err(|e| rpc(&e))?;
    Ok(state_json(world))
}

/// A terrain of the open set by name or index.
fn terrain_param(world: &World, params: &Value) -> Result<usize, RpcError> {
    let editor = world.get_resource::<AutotileEditor>();
    let set = editor
        .and_then(|e| e.doc.as_ref())
        .map(|d| &d.set)
        .ok_or_else(|| rpc(&AutotileEditError::NoDocument))?;
    match params.get("terrain") {
        None => Ok(editor.map_or(0, |e| e.terrain)),
        Some(Value::String(name)) => set
            .terrain_index(name)
            .ok_or_else(|| bad(format!("the rule set has no terrain `{name}`"))),
        Some(v) => v
            .as_u64()
            .and_then(|i| usize::try_from(i).ok())
            .ok_or_else(|| bad("`terrain` is a name or an index")),
    }
}

fn usize_param(params: &Value, key: &str) -> Result<usize, RpcError> {
    params
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|i| usize::try_from(i).ok())
        .ok_or_else(|| bad(format!("`{key}` is an index")))
}

fn u32_param(params: &Value, key: &str) -> Result<u32, RpcError> {
    params
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|i| u32::try_from(i).ok())
        .filter(|t| t & !GID_MASK == 0)
        .ok_or_else(|| bad(format!("`{key}` is a local tile id")))
}

fn string_param(params: &Value, key: &str) -> Result<String, RpcError> {
    str_param(params, key)
        .map(str::to_string)
        .ok_or_else(|| bad(format!("`{key}` is required")))
}

/// Parses the op of an `editor.autotile_edit` request.
fn parse_op(world: &World, params: &Value) -> Result<AutotileOp, RpcError> {
    let op = str_param(params, "op").ok_or_else(|| bad("`op` is required"))?;
    let terrain = || terrain_param(world, params);
    let rule = || usize_param(params, "rule");
    let flag = |key: &str| {
        params
            .get("flip")
            .and_then(|f| f.get(key))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    Ok(match op {
        "set_cell" => AutotileOp::SetPatternCell {
            terrain: terrain()?,
            rule: rule()?,
            cell: usize_param(params, "cell")?,
            value: str_param(params, "value")
                .and_then(|v| {
                    let mut chars = v.chars();
                    chars.next().filter(|_| chars.next().is_none())
                })
                .ok_or_else(|| bad("`value` is #, . or ?"))?,
        },
        "set_pattern" => AutotileOp::SetPattern {
            terrain: terrain()?,
            rule: rule()?,
            pattern: string_param(params, "pattern")?,
        },
        "set_tile" => AutotileOp::SetRuleTile {
            terrain: terrain()?,
            rule: rule()?,
            tile: u32_param(params, "tile")?,
            flips: [flag("x"), flag("y"), flag("diagonal")],
        },
        "add_rule" => AutotileOp::AddRule {
            terrain: terrain()?,
            at: params.get("at").map(|_| usize_param(params, "at")).transpose()?,
            pattern: str_param(params, "pattern")
                .unwrap_or(NEW_RULE_PATTERN)
                .to_string(),
            tile: u32_param(params, "tile")?,
        },
        "remove_rule" => AutotileOp::RemoveRule {
            terrain: terrain()?,
            rule: rule()?,
        },
        "move_rule" => AutotileOp::MoveRule {
            terrain: terrain()?,
            from: rule()?,
            to: usize_param(params, "to")?,
        },
        "set_fallback" => AutotileOp::SetFallback {
            terrain: terrain()?,
            tile: match params.get("tile") {
                None | Some(Value::Null) => None,
                Some(_) => Some(u32_param(params, "tile")?),
            },
        },
        "set_entry" => AutotileOp::SetLayoutTile {
            terrain: terrain()?,
            index: usize_param(params, "index")?,
            tile: u32_param(params, "tile")?,
        },
        "add_terrain" => AutotileOp::AddTerrain {
            name: string_param(params, "name")?,
            layout: LayoutKind::parse(str_param(params, "layout").unwrap_or("rules"))
                .ok_or_else(|| bad("`layout` is rules, edge16, blob47 or wang_corner"))?,
        },
        "remove_terrain" => AutotileOp::RemoveTerrain { terrain: terrain()? },
        "rename_terrain" => AutotileOp::RenameTerrain {
            terrain: terrain()?,
            name: string_param(params, "name")?,
        },
        "set_border" => AutotileOp::SetBorder(match str_param(params, "border") {
            Some("same") => Border::Same,
            Some("empty") => Border::Empty,
            _ => return Err(bad("`border` is same or empty")),
        }),
        "set_seed" => AutotileOp::SetSeed(
            params
                .get("seed")
                .and_then(Value::as_u64)
                .and_then(|s| u32::try_from(s).ok())
                .ok_or_else(|| bad("`seed` is a u32"))?,
        ),
        "set_tileset" => AutotileOp::SetTileset(string_param(params, "tileset")?),
        other => {
            return Err(bad(format!(
                "unknown op `{other}` (set_cell, set_pattern, set_tile, add_rule, remove_rule, move_rule, \
                 set_fallback, set_entry, add_terrain, remove_terrain, rename_terrain, set_border, set_seed, set_tileset)"
            )));
        }
    })
}

/// `editor.autotile_edit`.
fn autotile_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = parse_op(world, params)?;
    let what = edit(world, &op).map_err(|e| rpc(&e))?;
    let mut out = state_json(world);
    out["done"] = json!(what);
    Ok(out)
}

/// `editor.autotile_save`.
fn autotile_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")
        .map(|p| resolve_path(world, "path", p))
        .transpose()?;
    let (path, bytes) = save(world, path.as_deref()).map_err(|e| rpc(&e))?;
    Ok(json!({ "path": path.display().to_string(), "bytes": bytes }))
}

fn cells_param(params: &Value) -> Result<Option<Vec<(u32, u32)>>, RpcError> {
    if let Some(rect) = params.get("rect") {
        let v: Vec<u32> = rect
            .as_array()
            .and_then(|a| {
                a.iter()
                    .map(|v| v.as_u64().and_then(|v| u32::try_from(v).ok()))
                    .collect()
            })
            .filter(|v: &Vec<u32>| v.len() == 4)
            .ok_or_else(|| bad("`rect` is [x0, y0, x1, y1]"))?;
        let (x0, x1) = (v[0].min(v[2]), v[0].max(v[2]).min(MAX_PREVIEW_SIDE));
        let (y0, y1) = (v[1].min(v[3]), v[1].max(v[3]).min(MAX_PREVIEW_SIDE));
        return Ok(Some(
            (y0..=y1).flat_map(|y| (x0..=x1).map(move |x| (x, y))).collect(),
        ));
    }
    match params.get("cells") {
        None => Ok(None),
        Some(cells) => cells
            .as_array()
            .and_then(|a| {
                a.iter()
                    .map(|c| match c.as_array().map(Vec::as_slice) {
                        Some([x, y]) => {
                            Some((u32::try_from(x.as_u64()?).ok()?, u32::try_from(y.as_u64()?).ok()?))
                        }
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()
            })
            .filter(|c| c.len() <= (MAX_PREVIEW_SIDE * MAX_PREVIEW_SIDE) as usize)
            .map(Some)
            .ok_or_else(|| bad("`cells` is [[x, y], …]")),
    }
}

/// `editor.autotile_preview`: paints `cells` (or `rect`, or the editor's preview cells) of a
/// scratch map with a terrain; nothing changes.
fn autotile_preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let terrain = terrain_param(world, params)?;
    let editor = world
        .get_resource::<AutotileEditor>()
        .ok_or_else(|| rpc(&AutotileEditError::NoDocument))?;
    let size = match params.get("size") {
        None => editor.preview.size,
        Some(v) => v
            .as_array()
            .and_then(|a| match a.as_slice() {
                [w, h] => Some([u32::try_from(w.as_u64()?).ok()?, u32::try_from(h.as_u64()?).ok()?]),
                _ => None,
            })
            .ok_or_else(|| bad("`size` is [width, height]"))?,
    };
    let cells = cells_param(params)?.unwrap_or_else(|| editor.preview.cells.iter().copied().collect());
    let doc = editor
        .doc
        .as_ref()
        .ok_or_else(|| rpc(&AutotileEditError::NoDocument))?;
    let map = preview(
        &doc.set,
        map_tileset(world, &doc.set).as_ref(),
        terrain,
        size,
        &cells,
    )
    .map_err(|e| rpc(&e))?;
    let data = map.tile_layer(0).map(|l| l.data.clone()).unwrap_or_default();
    let rows: Vec<Vec<Value>> = data
        .chunks(size[0] as usize)
        .map(|row| {
            row.iter()
                .map(|&raw| {
                    if raw & GID_MASK == 0 {
                        Value::Null
                    } else {
                        json!((raw & GID_MASK) - 1)
                    }
                })
                .collect()
        })
        .collect();
    Ok(json!({ "size": size, "tiles": rows, "gids": data }))
}

/// `editor.autotile_select`: the selected terrain and rule, and the 2D tab's preview grid (view
/// state, not undone).
fn autotile_select(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let terrain = params
        .get("terrain")
        .map(|_| terrain_param(world, params))
        .transpose()?;
    let rule = params
        .get("rule")
        .map(|_| usize_param(params, "rule"))
        .transpose()?;
    let size = match params.get("size") {
        None => None,
        Some(v) => Some(
            v.as_array()
                .and_then(|a| match a.as_slice() {
                    [w, h] => Some([u32::try_from(w.as_u64()?).ok()?, u32::try_from(h.as_u64()?).ok()?]),
                    _ => None,
                })
                .filter(|s| s.iter().all(|s| (1..=MAX_PREVIEW_SIDE).contains(s)))
                .ok_or_else(|| bad(format!("`size` is [width, height], 1..={MAX_PREVIEW_SIDE}")))?,
        ),
    };
    let cells = cells_param(params)?;
    let mut editor = world.get_resource_or_insert_with(AutotileEditor::default);
    let terrains = editor.doc.as_ref().map_or(0, |d| d.set.terrains.len());
    if let Some(t) = terrain {
        if t >= terrains {
            return Err(bad(format!("terrain {t} does not exist")));
        }
        editor.terrain = t;
    }
    if let Some(r) = rule {
        editor.rule = r;
    }
    if let Some(size) = size {
        editor.preview.size = size;
        editor.preview.cells.retain(|(x, y)| *x < size[0] && *y < size[1]);
    }
    if let Some(cells) = cells {
        let [w, h] = editor.preview.size;
        if let Some((x, y)) = cells.iter().find(|(x, y)| *x >= w || *y >= h) {
            return Err(bad(format!("cell ({x}, {y}) is outside the {w}x{h} preview")));
        }
        editor.preview.cells = cells.into_iter().collect();
    }
    Ok(state_json(world))
}

const SELECT_PARAMS: &[ParamSchema] = &[
    param("terrain", "string", false, "Select a terrain (name or index)."),
    param("rule", "integer", false, "Select a rule (or layout entry) index."),
    param(
        "size",
        "array",
        false,
        "The preview grid [width, height], 1..=64.",
    ),
    param(
        "cells",
        "array",
        false,
        "The preview's painted cells [[x, y], …] (replaces them).",
    ),
    param(
        "rect",
        "array",
        false,
        "The preview's painted cells as [x0, y0, x1, y1] instead of cells.",
    ),
];

const OPEN_PARAMS: &[ParamSchema] = &[
    param("path", "string", true, "An .autotile.ron, project-relative."),
    param(
        "discard",
        "boolean",
        false,
        "Drop unsaved edits of the open rule set.",
    ),
];

const NEW_PARAMS: &[ParamSchema] = &[
    param(
        "path",
        "string",
        true,
        "Where the new .autotile.ron saves (must not exist).",
    ),
    param(
        "tileset",
        "string",
        false,
        "The tileset name it binds to (default: the 2D map's first tileset).",
    ),
    param(
        "discard",
        "boolean",
        false,
        "Drop unsaved edits of the open rule set.",
    ),
];

const EDIT_PARAMS: &[ParamSchema] = &[
    param(
        "op",
        "string",
        true,
        "set_cell, set_pattern, set_tile, add_rule, remove_rule, move_rule, set_fallback, set_entry, add_terrain, \
         remove_terrain, rename_terrain, set_border, set_seed or set_tileset.",
    ),
    param(
        "terrain",
        "string",
        false,
        "The terrain's name or index (default: the selected one).",
    ),
    param(
        "rule",
        "integer",
        false,
        "The rule index (set_cell, set_pattern, set_tile, remove_rule, move_rule).",
    ),
    param(
        "cell",
        "integer",
        false,
        "set_cell: 0..9, row by row, top row first.",
    ),
    param(
        "value",
        "string",
        false,
        "set_cell: # (belongs), . (does not) or ? (either).",
    ),
    param(
        "pattern",
        "string",
        false,
        "set_pattern, add_rule: nine cells, e.g. \"?#? / ### / ?.?\".",
    ),
    param(
        "tile",
        "integer",
        false,
        "set_tile, add_rule, set_fallback (null clears), set_entry: a local tile id.",
    ),
    param("flip", "object", false, "set_tile: {x?, y?, diagonal?}."),
    param(
        "at",
        "integer",
        false,
        "add_rule: the position (default: the end).",
    ),
    param("to", "integer", false, "move_rule: the new index."),
    param(
        "index",
        "integer",
        false,
        "set_entry: the layout entry (mask index).",
    ),
    param(
        "name",
        "string",
        false,
        "add_terrain, rename_terrain: the terrain name.",
    ),
    param(
        "layout",
        "string",
        false,
        "add_terrain: rules, edge16, blob47 or wang_corner.",
    ),
    param("border", "string", false, "set_border: same or empty."),
    param("seed", "integer", false, "set_seed: the variant seed."),
    param("tileset", "string", false, "set_tileset: the tileset name."),
];

const SAVE_PARAMS: &[ParamSchema] = &[param(
    "path",
    "string",
    false,
    "Another .autotile.ron to write instead.",
)];

const PREVIEW_PARAMS: &[ParamSchema] = &[
    param(
        "terrain",
        "string",
        false,
        "The terrain's name or index (default: the selected one).",
    ),
    param(
        "cells",
        "array",
        false,
        "[[x, y], …] to paint (default: the 2D tab's preview cells).",
    ),
    param(
        "rect",
        "array",
        false,
        "[x0, y0, x1, y1] to paint instead of cells (inclusive).",
    ),
    param(
        "size",
        "array",
        false,
        "[width, height] of the scratch map, 1..=64 (default: the tab's).",
    ),
];

/// The rule editor's agent methods.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.autotile_get",
                tool: "autotile_get",
                description: "The 2D tab's auto-tile rule editor: the open .autotile.ron (path, unsaved edits, \
                    tileset, border, seed, terrains with their rules or layout tiles), the selected terrain and rule, \
                    the preview cells and the undo depth.",
                params: &[],
                result: "{ doc, terrain, rule, preview, undo_depth }",
                mutating: false,
                permission: Permission::Read,
            },
            autotile_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.autotile_open",
                tool: "autotile_open",
                description: "Opens an .autotile.ron in the rule editor. Errors in data.mode2d_error: \
                    autotile_bad_params, autotile_load_failed, autotile_unsaved.",
                params: OPEN_PARAMS,
                result: "the editor state (as editor.autotile_get)",
                mutating: true,
                permission: Permission::EditScene,
            },
            autotile_open as Handler,
        ),
        (
            MethodSchema {
                name: "editor.autotile_new",
                tool: "autotile_new",
                description: "Starts a new rule set (current format version, one rules terrain) that saves to `path`. \
                    Errors in data.mode2d_error: autotile_bad_params, autotile_unsaved.",
                params: NEW_PARAMS,
                result: "the editor state (as editor.autotile_get)",
                mutating: true,
                permission: Permission::EditScene,
            },
            autotile_new as Handler,
        ),
        (
            MethodSchema {
                name: "editor.autotile_edit",
                tool: "autotile_edit",
                description: "Edits the open rule set as one undo entry: neighbor masks of a rule's 3x3 pattern, its \
                    output tile and flips, add, remove and reorder rules, a preset layout's entries, terrains, border, \
                    seed, tileset. The result is validated by the format; an invalid edit changes nothing. A 2D brush \
                    using the same file follows. Errors in data.mode2d_error: autotile_bad_params, \
                    autotile_no_document, autotile_invalid.",
                params: EDIT_PARAMS,
                result: "the editor state (as editor.autotile_get) and { done }",
                mutating: true,
                permission: Permission::EditScene,
            },
            autotile_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.autotile_save",
                tool: "autotile_save",
                description: "Writes the open rule set (or to `path`) atomically through the format's writer. Errors \
                    in data.mode2d_error: autotile_no_document, autotile_bad_params, autotile_save_failed.",
                params: SAVE_PARAMS,
                result: "{ path, bytes }",
                mutating: true,
                permission: Permission::EditScene,
            },
            autotile_save as Handler,
        ),
        (
            MethodSchema {
                name: "editor.autotile_preview",
                tool: "autotile_preview",
                description: "Paints a terrain of the open rule set over cells of a scratch map with the runtime's \
                    auto-tiler and returns the tiles (local ids, null = empty, row 0 at the top). The scratch map \
                    uses the 2D map's tileset of that name, or a placeholder. Errors in data.mode2d_error: \
                    autotile_bad_params, autotile_no_document, autotile_preview_failed.",
                params: PREVIEW_PARAMS,
                result: "{ size, tiles, gids }",
                mutating: false,
                permission: Permission::Read,
            },
            autotile_preview as Handler,
        ),
        (
            MethodSchema {
                name: "editor.autotile_select",
                tool: "autotile_select",
                description: "The rule editor's view state, as the 2D tab's clicks set it: the selected terrain and \
                    rule, and the preview grid's size and painted cells (not undone). Errors in data.mode2d_error: \
                    autotile_bad_params, autotile_no_document.",
                params: SELECT_PARAMS,
                result: "the editor state (as editor.autotile_get)",
                mutating: true,
                permission: Permission::EditScene,
            },
            autotile_select as Handler,
        ),
    ]
}

// ---- The 2D tab's section -----------------------------------------------------------------

/// Side of a preview cell and of a pattern-grid cell, points.
const GRID_CELL: f32 = 22.0;

enum RuleAction {
    New,
    Open,
    EditBrushRules,
    Save,
    Op(AutotileOp),
    Terrain(usize),
    Rule(usize),
    TogglePreview(u32, u32),
    ClearPreview,
}

/// The pattern grid's accessible name of cell `i` (`NW`, `N`, … `SE`).
const SLOT_NAMES: [&str; 9] = ["NW", "N", "NE", "W", "center", "E", "SW", "S", "SE"];

fn rules_list(
    ui: &mut egui::Ui,
    editor: &AutotileEditor,
    terrain: usize,
    rules: &[PatternRule],
    actions: &mut Vec<RuleAction>,
) {
    for (i, rule) in rules.iter().enumerate() {
        let label = format!("{}: {} -> {}", i + 1, rule.pattern, rule.tile);
        if ui.selectable_label(editor.rule == i, label).clicked() {
            actions.push(RuleAction::Rule(i));
        }
    }
    let r = editor.rule.min(rules.len().saturating_sub(1));
    ui.horizontal(|ui| {
        if ui.small_button("Add rule").clicked() {
            actions.push(RuleAction::Op(AutotileOp::AddRule {
                terrain,
                at: Some(r + 1),
                pattern: NEW_RULE_PATTERN.into(),
                tile: rules.get(r).map_or(0, |rule| rule.tile),
            }));
        }
        if ui
            .add_enabled(r > 0, egui::Button::new("Move up").small())
            .clicked()
        {
            actions.push(RuleAction::Op(AutotileOp::MoveRule {
                terrain,
                from: r,
                to: r - 1,
            }));
        }
        if ui
            .add_enabled(r + 1 < rules.len(), egui::Button::new("Move down").small())
            .clicked()
        {
            actions.push(RuleAction::Op(AutotileOp::MoveRule {
                terrain,
                from: r,
                to: r + 1,
            }));
        }
        if ui
            .add_enabled(rules.len() > 1, egui::Button::new("Remove rule").small())
            .clicked()
        {
            actions.push(RuleAction::Op(AutotileOp::RemoveRule { terrain, rule: r }));
        }
    });
    let Some(rule) = rules.get(r) else { return };
    let Some(cells) = pattern_cells(&rule.pattern) else {
        return;
    };
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(2.0, 2.0);
            for row in 0..3 {
                ui.horizontal(|ui| {
                    for col in 0..3 {
                        let i = row * 3 + col;
                        let text = cells[i].to_string();
                        let response = ui.add_sized([GRID_CELL, GRID_CELL], egui::Button::new(text));
                        response.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                true,
                                format!("Neighbor {} is {}", SLOT_NAMES[i], cells[i]),
                            )
                        });
                        if response.clicked() {
                            actions.push(RuleAction::Op(AutotileOp::SetPatternCell {
                                terrain,
                                rule: r,
                                cell: i,
                                value: next_cell_value(cells[i], i == 4),
                            }));
                        }
                    }
                });
            }
        });
        ui.vertical(|ui| {
            ui.weak("# belongs, . does not, ? either");
            let mut tile = rule.tile;
            let mut flips = [rule.flip_x, rule.flip_y, rule.diagonal];
            let before = (tile, flips);
            ui.horizontal(|ui| {
                ui.label("Output tile");
                ui.add(egui::DragValue::new(&mut tile).range(0..=GID_MASK));
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut flips[0], "Rule flip X");
                ui.checkbox(&mut flips[1], "Rule flip Y");
                ui.checkbox(&mut flips[2], "Rule diagonal");
            });
            if (tile, flips) != before {
                actions.push(RuleAction::Op(AutotileOp::SetRuleTile {
                    terrain,
                    rule: r,
                    tile,
                    flips,
                }));
            }
        });
    });
}

fn preset_entries(
    ui: &mut egui::Ui,
    editor: &AutotileEditor,
    terrain: usize,
    tiles: &[u32],
    actions: &mut Vec<RuleAction>,
) {
    let mut index = editor.rule.min(tiles.len().saturating_sub(1));
    let before = index;
    ui.horizontal(|ui| {
        ui.label("Entry (mask index)");
        ui.add(egui::DragValue::new(&mut index).range(0..=tiles.len().saturating_sub(1)));
    });
    if index != before {
        actions.push(RuleAction::Rule(index));
    }
    let Some(&current) = tiles.get(index) else { return };
    let mut tile = current;
    ui.horizontal(|ui| {
        ui.label("Tile");
        ui.add(egui::DragValue::new(&mut tile).range(0..=GID_MASK));
    });
    if tile != current {
        actions.push(RuleAction::Op(AutotileOp::SetLayoutTile { terrain, index, tile }));
    }
}

fn preview_grid(ui: &mut egui::Ui, world: &World, editor: &AutotileEditor, actions: &mut Vec<RuleAction>) {
    let [w, h] = editor.preview.size;
    let painted = editor_preview(world);
    let data = painted
        .as_ref()
        .ok()
        .and_then(|m| m.tile_layer(0).map(|l| l.data.clone()))
        .unwrap_or_default();
    ui.horizontal(|ui| {
        ui.weak("Preview: click cells to paint the terrain");
        if ui.small_button("Clear preview").clicked() {
            actions.push(RuleAction::ClearPreview);
        }
    });
    if let Err(e) = &painted {
        ui.colored_label(ui.visuals().warn_fg_color, e.to_string());
    }
    ui.spacing_mut().item_spacing = egui::vec2(1.0, 1.0);
    for y in 0..h {
        ui.horizontal(|ui| {
            for x in 0..w {
                let (rect, response) =
                    ui.allocate_exact_size(egui::vec2(GRID_CELL, GRID_CELL), egui::Sense::click());
                let raw = data.get((y * w + x) as usize).copied().unwrap_or(0);
                let on = editor.preview.cells.contains(&(x, y));
                let label = if raw & GID_MASK == 0 {
                    String::new()
                } else {
                    ((raw & GID_MASK) - 1).to_string()
                };
                response.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::WidgetType::Checkbox,
                        true,
                        on,
                        format!(
                            "Preview cell {x}, {y}{}",
                            if label.is_empty() {
                                String::new()
                            } else {
                                format!(": tile {label}")
                            }
                        ),
                    )
                });
                if ui.is_rect_visible(rect) {
                    let fill = if on {
                        ui.visuals().selection.bg_fill
                    } else {
                        ui.visuals().faint_bg_color
                    };
                    ui.painter().rect_filled(rect, 0.0, fill);
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        label,
                        egui::FontId::proportional(9.0),
                        ui.visuals().text_color(),
                    );
                }
                if response.clicked() {
                    actions.push(RuleAction::TogglePreview(x, y));
                }
            }
        });
    }
}

/// Draws the **Auto-tile rules** section of the 2D tab (collapsed by default) and applies its
/// clicks: New, Open, the 2D brush's rules, Save, the terrain list, the selected terrain's rules
/// (the 3x3 neighbor grid, output tile, flips, add, move, remove) or layout entries, and the
/// preview.
pub fn section_ui(ui: &mut egui::Ui, world: &mut World) {
    world.get_resource_or_insert_with(AutotileEditor::default);
    let mut actions = Vec::new();
    let error_id = egui::Id::new("mode2d-autotile-error");
    let world_ref: &World = world;
    egui::CollapsingHeader::new("Auto-tile rules")
        .id_salt("mode2d-autotile")
        .show(ui, |ui| {
            let world = world_ref;
            let editor = world.resource::<AutotileEditor>();
            ui.horizontal(|ui| {
                if ui.small_button("New rules…").clicked() {
                    actions.push(RuleAction::New);
                }
                if ui.small_button("Open rules file…").clicked() {
                    actions.push(RuleAction::Open);
                }
                let brush_rules = world.get_resource::<Mode2d>().is_some_and(|m| m.rules.is_some());
                if ui
                    .add_enabled(brush_rules, egui::Button::new("Edit brush rules").small())
                    .clicked()
                {
                    actions.push(RuleAction::EditBrushRules);
                }
                let dirty = editor.doc.as_ref().is_some_and(|d| d.dirty);
                if ui
                    .add_enabled(dirty, egui::Button::new("Save rules").small())
                    .clicked()
                {
                    actions.push(RuleAction::Save);
                }
            });
            let Some(doc) = &editor.doc else {
                ui.weak("No rule set open.");
                return;
            };
            let name = doc.path.file_name().and_then(|n| n.to_str()).unwrap_or("rules");
            ui.label(format!(
                "{name}{} (tileset {})",
                if doc.dirty { " *" } else { "" },
                doc.set.tileset
            ));
            ui.horizontal_wrapped(|ui| {
                for (i, t) in doc.set.terrains.iter().enumerate() {
                    if ui
                        .selectable_label(editor.terrain == i, t.name.as_str())
                        .clicked()
                    {
                        actions.push(RuleAction::Terrain(i));
                    }
                }
                if ui.small_button("Add terrain").clicked() {
                    let mut n = doc.set.terrains.len() + 1;
                    while doc.set.terrain_index(&format!("terrain {n}")).is_some() {
                        n += 1;
                    }
                    actions.push(RuleAction::Op(AutotileOp::AddTerrain {
                        name: format!("terrain {n}"),
                        layout: LayoutKind::Rules,
                    }));
                }
            });
            let t = editor.terrain.min(doc.set.terrains.len().saturating_sub(1));
            if let Some(terrain) = doc.set.terrains.get(t) {
                ui.weak(format!("{}: {} layout", terrain.name, terrain.layout.name()));
                match &terrain.layout {
                    Layout::Rules(rules) => rules_list(ui, editor, t, rules, &mut actions),
                    Layout::Edge16(tiles) | Layout::Blob47(tiles) | Layout::WangCorner(tiles) => {
                        preset_entries(ui, editor, t, tiles, &mut actions);
                    }
                }
            }
            ui.separator();
            preview_grid(ui, world, editor, &mut actions);
            if let Some(error) = ui.ctx().data(|d| d.get_temp::<String>(error_id)) {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
        });
    if actions.is_empty()
        || !super::first_apply_this_frame(ui.ctx(), egui::Id::new("mode2d-autotile-applied-frame"))
    {
        return;
    }
    let mut error = None;
    for action in actions {
        if let Err(e) = apply_section(world, action) {
            error = Some(e);
        }
    }
    ui.ctx().data_mut(|d| {
        if let Some(e) = error {
            d.insert_temp(error_id, e);
        } else {
            d.remove::<String>(error_id);
        }
    });
}

fn apply_section(world: &mut World, action: RuleAction) -> Result<(), String> {
    let err = |e: AutotileEditError| e.to_string();
    match action {
        RuleAction::New => {
            let tileset = world
                .get_resource::<Mode2d>()
                .and_then(|m| m.doc.as_ref())
                .and_then(|d| d.map.tilesets.first())
                .map_or_else(|| "tileset".to_string(), |t| t.name.clone());
            let mut dialog = crate::file_dialog::FileDialog::new()
                .set_title("New auto-tiling rules")
                .add_filter("Auto-tiling rules", &["ron"])
                .set_file_name(format!("{tileset}.autotile.ron"));
            if let Some(root) = crate::mesh_source::project_root(world) {
                dialog = dialog.set_directory(root);
            }
            match dialog.save_file() {
                Some(path) => new_document(world, &path, &tileset, false).map_err(err),
                None => Ok(()),
            }
        }
        RuleAction::Open => {
            let mut dialog = crate::file_dialog::FileDialog::new()
                .set_title("Open auto-tiling rules")
                .add_filter("Auto-tiling rules", &["ron"]);
            if let Some(root) = crate::mesh_source::project_root(world) {
                dialog = dialog.set_directory(root);
            }
            match dialog.pick_file() {
                Some(path) => open(world, &path, false).map_err(err),
                None => Ok(()),
            }
        }
        RuleAction::EditBrushRules => {
            let path = world
                .get_resource::<Mode2d>()
                .and_then(|m| m.rules.as_ref())
                .map(|r| r.path.clone())
                .ok_or_else(|| "no rules are loaded for the brush".to_string())?;
            open(world, &path, false).map_err(err)
        }
        RuleAction::Save => save(world, None).map(|_| ()).map_err(err),
        RuleAction::Op(op) => edit(world, &op).map(|_| ()).map_err(err),
        RuleAction::Terrain(t) => {
            let mut editor = world.resource_mut::<AutotileEditor>();
            editor.terrain = t;
            editor.rule = 0;
            Ok(())
        }
        RuleAction::Rule(r) => {
            world.resource_mut::<AutotileEditor>().rule = r;
            Ok(())
        }
        RuleAction::TogglePreview(x, y) => {
            let mut editor = world.resource_mut::<AutotileEditor>();
            if !editor.preview.cells.remove(&(x, y)) {
                editor.preview.cells.insert((x, y));
            }
            Ok(())
        }
        RuleAction::ClearPreview => {
            world.resource_mut::<AutotileEditor>().preview.cells.clear();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: &str = r#"(
    version: 2,
    tileset: "t",
    terrains: [
        (name: "path", layout: Rules([(pattern: "???/?#?/???", tile: 0)])),
        (name: "wall", layout: Edge16([16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31])),
    ],
)"#;

    fn project() -> PathBuf {
        let dir = super::super::tests::project();
        std::fs::write(dir.join("path.autotile.ron"), RULES).expect("write rules");
        dir
    }

    fn agent_world(dir: &Path) -> World {
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.to_path_buf()),
        });
        crate::agent_ext::install(&mut world);
        world
    }

    fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let (_, handler) = world
            .resource::<regia_agent::extension::AgentExtension>()
            .method(method)
            .unwrap_or_else(|| panic!("{method} is registered"));
        handler(world, &params)
    }

    fn code(error: &RpcError) -> String {
        error
            .data
            .as_ref()
            .and_then(|d| d["mode2d_error"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    fn rule(pattern: &str, tile: u32, flip_x: bool) -> PatternRule {
        PatternRule {
            pattern: pattern.into(),
            tile,
            flip_x,
            flip_y: false,
            diagonal: false,
        }
    }

    /// The rule set the edits below must produce, written by hand.
    fn edited() -> AutoTileSet {
        let mut set = AutoTileSet::from_ron(RULES).expect("rules");
        set.terrains[0].layout = Layout::Rules(vec![
            rule("?#? / ?#? / ?#?", 1, false),
            rule("??? / ### / ???", 2, true),
            rule("???/?#?/???", 5, false),
        ]);
        set
    }

    /// A T on the 6 x 4 fixture map: a row of three at y = 1 and a column down to the bottom row.
    const SHAPE: [(u32, u32); 5] = [(1, 1), (2, 1), (3, 1), (2, 2), (2, 3)];

    #[test]
    fn pattern_cells_cycle_and_print_canonically() {
        assert_eq!(
            pattern_cells("?#? / ### / ?.?"),
            Some(['?', '#', '?', '#', '#', '#', '?', '.', '?'])
        );
        assert_eq!(pattern_cells("??"), None);
        assert_eq!(
            pattern_text(['?', '#', '?', '#', '#', '#', '?', '.', '?']),
            "?#? / ### / ?.?"
        );
        assert_eq!(next_cell_value('?', false), '#');
        assert_eq!(next_cell_value('#', false), '.');
        assert_eq!(next_cell_value('.', false), '?');
        assert_eq!(next_cell_value('#', true), '?', "the center has no `.`");
    }

    #[test]
    fn rule_edits_then_the_preview_paints_what_the_runtime_auto_tiler_paints() {
        let dir = project();
        let mut world = agent_world(&dir);
        call(
            &mut world,
            "editor.mode2d_set",
            json!({ "map": "level.tilemap.ron", "rules": "path.autotile.ron" }),
        )
        .expect("map and brush rules");
        call(
            &mut world,
            "editor.autotile_open",
            json!({ "path": "path.autotile.ron" }),
        )
        .expect("opens");
        let edit =
            |world: &mut World, params: Value| call(world, "editor.autotile_edit", params).expect("edits");
        // The catch-all rule writes 5; a horizontal run (added first, moved below) writes 2 mirrored;
        // a vertical run, built cell by cell on the 3x3 grid, comes first.
        edit(
            &mut world,
            json!({ "op": "set_tile", "terrain": "path", "rule": 0, "tile": 5 }),
        );
        edit(
            &mut world,
            json!({ "op": "add_rule", "terrain": "path", "at": 0, "pattern": "???/###/???", "tile": 2 }),
        );
        edit(
            &mut world,
            json!({ "op": "set_tile", "terrain": "path", "rule": 0, "tile": 2, "flip": { "x": true } }),
        );
        edit(
            &mut world,
            json!({ "op": "add_rule", "terrain": "path", "at": 0, "tile": 1 }),
        );
        edit(
            &mut world,
            json!({ "op": "set_cell", "terrain": "path", "rule": 0, "cell": 1, "value": "#" }),
        );
        let out = edit(
            &mut world,
            json!({ "op": "set_cell", "terrain": "path", "rule": 0, "cell": 7, "value": "#" }),
        );
        assert_eq!(
            out["doc"]["terrains"][0]["rules"][0]["pattern"],
            "?#? / ?#? / ?#?"
        );
        // Reorder: swap the first two and back (both orders are one undo entry each).
        edit(
            &mut world,
            json!({ "op": "move_rule", "terrain": "path", "rule": 0, "to": 1 }),
        );
        let out = edit(
            &mut world,
            json!({ "op": "move_rule", "terrain": "path", "rule": 1, "to": 0 }),
        );
        assert_eq!(out["undo_depth"], 8);
        assert_eq!(
            world
                .resource::<AutotileEditor>()
                .doc
                .as_ref()
                .map(|d| d.set.clone()),
            Some(edited())
        );
        assert_eq!(out["doc"]["dirty"], true);

        let cells: Vec<Value> = SHAPE.iter().map(|(x, y)| json!([x, y])).collect();
        let preview = call(
            &mut world,
            "editor.autotile_preview",
            json!({ "terrain": "path", "size": [6, 4], "cells": cells }),
        )
        .expect("previews");
        // Hand-checked (border Same: off the map counts as the terrain): the middle of the row is
        // also the top of the column; N is not set, so the vertical rule fails and the horizontal
        // one matches (2, mirrored); (2, 2) and (2, 3) are vertical (1), the row's ends fall through.
        let tile = |x: usize, y: usize| preview["tiles"][y][x].clone();
        assert_eq!(tile(1, 1), json!(5), "row end: only E is set");
        assert_eq!(tile(2, 1), json!(2));
        assert_eq!(tile(3, 1), json!(5));
        assert_eq!(tile(2, 2), json!(1));
        assert_eq!(tile(2, 3), json!(1), "S is off the map");
        assert_eq!(tile(0, 0), Value::Null);
        assert_eq!(
            preview["gids"][8],
            json!(3 | regia_2d::tilemap::FLIP_X_BIT),
            "the flip is in the GID bits"
        );

        // The same layer from the runtime's auto-tiler, on the fixture map, with the hand-written set.
        let set = edited();
        let mut map = super::super::tests::map();
        set.bind(&map)
            .expect("binds")
            .paint(&mut map, 1, TerrainBrush::Terrain(0), &SHAPE)
            .expect("paints");
        let runtime = map.tile_layer(1).expect("layer").data.clone();
        assert_eq!(preview["gids"], json!(runtime));

        // The 2D brush uses the same file: it follows the edits, and a stroke on the real map paints
        // the runtime's tiles.
        assert_eq!(
            world.resource::<Mode2d>().rules.as_ref().map(|r| r.set.clone()),
            Some(edited())
        );
        call(
            &mut world,
            "editor.mode2d_set",
            json!({ "layer": "ground", "brush": { "terrain": "path" } }),
        )
        .expect("brush");
        call(&mut world, "editor.tile_stroke", json!({ "cells": cells })).expect("strokes");
        assert_eq!(super::super::tests::ground(world.resource::<Mode2d>()), runtime);

        // Save writes the canonical text; reopening gives the same set.
        let saved = call(&mut world, "editor.autotile_save", json!({})).expect("saves");
        let text = std::fs::read_to_string(dir.join("path.autotile.ron")).expect("read");
        assert_eq!(text, edited().to_ron().expect("ron"));
        assert_eq!(saved["bytes"], text.len());
        assert_eq!(
            call(&mut world, "editor.autotile_get", json!({})).expect("get")["doc"]["dirty"],
            false
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn undo_redo_restore_the_set_and_invalid_edits_change_nothing() {
        let dir = project();
        let mut world = agent_world(&dir);
        call(
            &mut world,
            "editor.autotile_open",
            json!({ "path": "path.autotile.ron" }),
        )
        .expect("opens");
        let original = AutoTileSet::from_ron(RULES).expect("rules");
        call(
            &mut world,
            "editor.autotile_edit",
            json!({ "op": "set_tile", "rule": 0, "tile": 3 }),
        )
        .expect("edits");
        call(
            &mut world,
            "editor.autotile_edit",
            json!({ "op": "add_terrain", "name": "water", "layout": "edge16" }),
        )
        .expect("adds a terrain");
        let water = world
            .resource::<AutotileEditor>()
            .doc
            .as_ref()
            .map(|d| d.set.terrains[2].clone())
            .expect("water");
        assert_eq!(
            water.layout,
            Layout::Edge16((0..16).filter(|t| *t != 3).chain([32]).collect()),
            "free tiles"
        );
        let depth = world.resource::<UndoStack>().undo_depth();
        // Tile 16 belongs to `wall`: the format refuses overlapping members.
        let err = call(
            &mut world,
            "editor.autotile_edit",
            json!({ "op": "set_tile", "terrain": "path", "rule": 0, "tile": 16 }),
        )
        .expect_err("overlap");
        assert_eq!(code(&err), "autotile_invalid", "{err:?}");
        let err = call(
            &mut world,
            "editor.autotile_edit",
            json!({ "op": "set_cell", "terrain": "path", "rule": 0, "cell": 4, "value": "." }),
        )
        .expect_err("center");
        assert_eq!(code(&err), "autotile_invalid");
        let err = call(
            &mut world,
            "editor.autotile_edit",
            json!({ "op": "remove_rule", "terrain": "path", "rule": 0 }),
        )
        .expect_err("last rule");
        assert_eq!(code(&err), "autotile_bad_params");
        let err = call(
            &mut world,
            "editor.autotile_edit",
            json!({ "op": "set_entry", "terrain": "path", "index": 0, "tile": 1 }),
        )
        .expect_err("rules have no entries");
        assert_eq!(code(&err), "autotile_bad_params");
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            depth,
            "refused edits push nothing"
        );
        assert!(crate::command::undo_world(&mut world));
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(
            world
                .resource::<AutotileEditor>()
                .doc
                .as_ref()
                .map(|d| d.set.clone()),
            Some(original)
        );
        assert!(crate::command::redo_world(&mut world));
        assert_eq!(
            world
                .resource::<AutotileEditor>()
                .doc
                .as_ref()
                .map(|d| d.set.terrains[0].layout.clone()),
            Some(Layout::Rules(vec![rule("???/?#?/???", 3, false)]))
        );
        // Opening another set over the unsaved edits needs discard.
        let err = call(
            &mut world,
            "editor.autotile_open",
            json!({ "path": "walls.autotile.ron" }),
        )
        .expect_err("unsaved");
        assert_eq!(code(&err), "autotile_unsaved");
        call(
            &mut world,
            "editor.autotile_open",
            json!({ "path": "walls.autotile.ron", "discard": true }),
        )
        .expect("discards");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn new_rules_save_at_the_current_version_and_the_view_state_route_selects() {
        let dir = project();
        let mut world = agent_world(&dir);
        let err = call(
            &mut world,
            "editor.autotile_edit",
            json!({ "op": "set_seed", "seed": 1 }),
        )
        .expect_err("nothing open");
        assert_eq!(code(&err), "autotile_no_document");
        let err = call(
            &mut world,
            "editor.autotile_new",
            json!({ "path": "new.autotile.ron" }),
        )
        .expect_err("no tileset");
        assert_eq!(code(&err), "autotile_bad_params");
        call(
            &mut world,
            "editor.mode2d_set",
            json!({ "map": "level.tilemap.ron" }),
        )
        .expect("map");
        let out = call(
            &mut world,
            "editor.autotile_new",
            json!({ "path": "new.autotile.ron" }),
        )
        .expect("new");
        assert_eq!(out["doc"]["tileset"], "t", "the map's first tileset");
        assert_eq!(out["doc"]["dirty"], true);
        assert!(!dir.join("new.autotile.ron").exists(), "written on save only");
        call(
            &mut world,
            "editor.autotile_edit",
            json!({ "op": "set_border", "border": "empty" }),
        )
        .expect("border");
        call(&mut world, "editor.autotile_save", json!({})).expect("saves");
        let set =
            AutoTileSet::from_ron(&std::fs::read_to_string(dir.join("new.autotile.ron")).expect("read"))
                .expect("loads");
        assert_eq!(set.version, AUTOTILE_FORMAT_VERSION);
        assert_eq!(set.border, Border::Empty);
        let err = call(
            &mut world,
            "editor.autotile_new",
            json!({ "path": "new.autotile.ron" }),
        )
        .expect_err("exists");
        assert_eq!(code(&err), "autotile_bad_params");
        let out = call(
            &mut world,
            "editor.autotile_select",
            json!({ "terrain": 0, "rule": 0, "size": [3, 3], "rect": [0, 0, 1, 0] }),
        )
        .expect("selects");
        assert_eq!(out["preview"]["cells"], json!([[0, 0], [1, 0]]));
        let preview = call(&mut world, "editor.autotile_preview", json!({})).expect("the stored preview");
        assert_eq!(preview["tiles"][0], json!([0, 0, null]));
        let err =
            call(&mut world, "editor.autotile_select", json!({ "cells": [[9, 9]] })).expect_err("outside");
        assert_eq!(code(&err), "autotile_bad_params");
        let _ = std::fs::remove_dir_all(dir);
    }
}
