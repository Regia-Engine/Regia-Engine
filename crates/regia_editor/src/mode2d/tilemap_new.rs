// SPDX-License-Identifier: MPL-2.0
//! New tile maps and the tileset binding editor of the 2D mode (`docs/specs/86-2d-toolkit.md`
//! § 6 "As built", ADR-7742).
//!
//! - [`new_tilemap`] builds a `.tilemap.ron` at the current format version
//!   ([`TILEMAP_FORMAT_VERSION`]): a grid size, a cell size, one empty tile layer and the bound
//!   tilesets. Content ▸ New ▸ Tile map writes the default one (`NewAssetKind::Tilemap`); the 2D
//!   tab's **New tile map…** form and `editor.tilemap_new` take every field.
//! - A tileset comes from a [`TilesetSource`]: an image cut into cells of a tile size, or a
//!   tileset of another `.tilemap.ron` (its image path rebased onto the new map's folder).
//!   [`tileset_candidates`] lists both kinds under the project's `assets/` for the picker.
//! - [`apply_binding`] adds, removes, replaces or reorders the map's tilesets and moves layers.
//!   Tilesets are renumbered compactly in list order (the first starts at GID 1) and every tile of
//!   every layer is remapped with its flip bits; a tile whose tileset was removed, or whose local id
//!   no longer exists in its replacement, becomes empty. One op is one [`TilemapEditCommand`] on
//!   the undo stack, holding the map before and after: undo restores it bit for bit.
//!
//! Agent methods: `editor.tilemap_new`, `editor.tilemap_tilesets`, `editor.tileset_candidates`
//! (`book/agents/editor-2d.md`).

use std::path::{Component, Path, PathBuf};

use regia_2d::tilemap::{
    GID_MASK, MapLayer, ObjectShape, Orientation, TILEMAP_FORMAT_VERSION, TileLayer, TileMapAsset, TilesetDef,
};
use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::{Mode2d, Mode2dError, bad, is_tilemap_ron, mode2d_error, param, read_limited, resolve_path};
use crate::command::{EditorCommand, UndoStack};
use crate::viewport_agent::{Handler, str_param};

/// Grid size of a new map from Content ▸ New (cells).
pub const DEFAULT_MAP_SIZE: [u32; 2] = [16, 16];
/// Cell size of a new map from Content ▸ New (pixels).
pub const DEFAULT_TILE_SIZE: [u32; 2] = [16, 16];
/// Name of a new map's tile layer.
pub const DEFAULT_LAYER: &str = "ground";
/// Largest grid side `editor.tilemap_new` and the form accept (cells).
pub const MAX_NEW_MAP_SIDE: u32 = 4096;
/// Largest cell side (pixels).
pub const MAX_NEW_TILE_SIDE: u32 = 4096;
/// Most candidates [`tileset_candidates`] lists.
pub const MAX_CANDIDATES: usize = 512;
/// Most directory entries [`tileset_candidates`] visits.
const MAX_SCAN_ENTRIES: usize = 20_000;
/// Deepest folder [`tileset_candidates`] descends into under `assets/`.
const MAX_SCAN_DEPTH: usize = 8;

/// What a new map holds.
#[derive(Debug, Clone, PartialEq)]
pub struct NewTilemap {
    /// Grid size in cells (`[width, height]`).
    pub size: [u32; 2],
    /// Cell size in pixels (`[width, height]`).
    pub tile_size: [u32; 2],
    /// Name of the one tile layer.
    pub layer: String,
    /// The tilesets, in order (their `first_gid` is assigned compactly).
    pub tilesets: Vec<TilesetDef>,
}

impl Default for NewTilemap {
    fn default() -> Self {
        Self {
            size: DEFAULT_MAP_SIZE,
            tile_size: DEFAULT_TILE_SIZE,
            layer: DEFAULT_LAYER.to_string(),
            tilesets: Vec::new(),
        }
    }
}

/// Builds a new orthogonal map at the current format version, validated: one empty tile layer,
/// the tilesets renumbered from GID 1.
///
/// # Errors
///
/// [`Mode2dError::BadParams`] for a zero or too large size, an empty layer name, duplicate
/// tileset names, or a map the format refuses.
pub fn new_tilemap(spec: &NewTilemap) -> Result<TileMapAsset, Mode2dError> {
    let [width, height] = spec.size;
    let [tile_width, tile_height] = spec.tile_size;
    if !(1..=MAX_NEW_MAP_SIDE).contains(&width) || !(1..=MAX_NEW_MAP_SIDE).contains(&height) {
        return Err(Mode2dError::BadParams(format!(
            "the grid size is 1..={MAX_NEW_MAP_SIDE} cells per side"
        )));
    }
    if !(1..=MAX_NEW_TILE_SIDE).contains(&tile_width) || !(1..=MAX_NEW_TILE_SIDE).contains(&tile_height) {
        return Err(Mode2dError::BadParams(format!(
            "the cell size is 1..={MAX_NEW_TILE_SIDE} pixels per side"
        )));
    }
    if spec.layer.trim().is_empty() {
        return Err(Mode2dError::BadParams("the layer needs a name".into()));
    }
    let mut map = TileMapAsset {
        version: TILEMAP_FORMAT_VERSION,
        orientation: Orientation::Orthogonal,
        width,
        height,
        tile_width,
        tile_height,
        background: None,
        tilesets: Vec::new(),
        layers: vec![MapLayer::Tiles(TileLayer {
            name: spec.layer.clone(),
            visible: true,
            opacity: 1.0,
            offset: [0.0, 0.0],
            data: vec![0; width as usize * height as usize],
            properties: Default::default(),
        })],
        properties: Default::default(),
    };
    for set in &spec.tilesets {
        map = apply_binding(&map, &BindingOp::AddTileset(set.clone()))?;
    }
    map.validate()
        .map_err(|e| Mode2dError::BadParams(e.to_string()))?;
    Ok(map)
}

/// Where a bound tileset comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TilesetSource {
    /// An image cut into cells of `tile_size` (columns and count follow from its pixel size).
    Image {
        /// The image file.
        path: PathBuf,
        /// Cell size in pixels.
        tile_size: [u32; 2],
        /// The tileset's name (default: the image's file stem).
        name: Option<String>,
    },
    /// The tileset named `tileset` of another `.tilemap.ron`.
    Map {
        /// The `.tilemap.ron` holding it.
        map: PathBuf,
        /// Its name there.
        tileset: String,
    },
}

/// `to` relative to the folder `from_dir` (both absolute, or both relative to one base), with
/// `/` separators; `..` steps out where they share less than all of `from_dir`.
#[must_use]
pub fn relative_path(from_dir: &Path, to: &Path) -> String {
    let norm = |p: &Path| -> Vec<String> {
        p.components()
            .filter(|c| !matches!(c, Component::CurDir))
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect()
    };
    let (from, to) = (norm(from_dir), norm(to));
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut out: Vec<String> = std::iter::repeat_n("..".to_string(), from.len() - common).collect();
    out.extend(to[common..].iter().cloned());
    out.join("/")
}

/// The folder a map file lives in (`.` for a bare file name).
fn map_dir(map_path: &Path) -> PathBuf {
    map_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// The tileset a [`TilesetSource`] describes, for a map stored at `map_path` (its image path is
/// relative to that map's folder). `first_gid` is left at 1; [`apply_binding`] renumbers it.
///
/// # Errors
///
/// [`Mode2dError::Load`] when the image or map cannot be read, [`Mode2dError::BadParams`] when the
/// image holds no whole cell or the map has no such tileset.
pub fn resolve_source(map_path: &Path, source: &TilesetSource) -> Result<TilesetDef, Mode2dError> {
    match source {
        TilesetSource::Image {
            path,
            tile_size,
            name,
        } => {
            let [tw, th] = *tile_size;
            if !(1..=MAX_NEW_TILE_SIDE).contains(&tw) || !(1..=MAX_NEW_TILE_SIDE).contains(&th) {
                return Err(Mode2dError::BadParams(format!(
                    "the tile size is 1..={MAX_NEW_TILE_SIDE} pixels per side"
                )));
            }
            let (w, h) = image::image_dimensions(path)
                .map_err(|e| Mode2dError::Load(format!("{}: {e}", path.display())))?;
            let (columns, rows) = (w / tw, h / th);
            if columns == 0 || rows == 0 {
                return Err(Mode2dError::BadParams(format!(
                    "{} ({w}x{h}) holds no whole {tw}x{th} tile",
                    path.display()
                )));
            }
            let stem = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.split('.').next())
                .filter(|s| !s.is_empty())
                .unwrap_or("tileset");
            Ok(TilesetDef {
                name: name.clone().unwrap_or_else(|| stem.to_string()),
                first_gid: 1,
                image: relative_path(&map_dir(map_path), path),
                image_width: w,
                image_height: h,
                tile_width: tw,
                tile_height: th,
                columns,
                tile_count: columns.saturating_mul(rows),
                spacing: 0,
                margin: 0,
                tiles: Vec::new(),
            })
        }
        TilesetSource::Map { map, tileset } => {
            let text = read_limited(map).map_err(Mode2dError::Load)?;
            let other = TileMapAsset::from_ron(&text).map_err(|e| Mode2dError::Load(e.to_string()))?;
            let mut set = other
                .tilesets
                .into_iter()
                .find(|s| s.name == *tileset)
                .ok_or_else(|| {
                    Mode2dError::BadParams(format!("{} has no tileset `{tileset}`", map.display()))
                })?;
            set.image = relative_path(&map_dir(map_path), &map_dir(map).join(&set.image));
            set.first_gid = 1;
            Ok(set)
        }
    }
}

/// A tileset the picker offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TilesetCandidate {
    /// An image file (`.png`) under `assets/`.
    Image {
        /// Project-relative path.
        path: String,
    },
    /// A tileset of a `.tilemap.ron` under `assets/`.
    Map {
        /// Project-relative path of the map.
        map: String,
        /// The tileset's name.
        tileset: String,
        /// Its cell size.
        tile_size: [u32; 2],
        /// Its tile count.
        tile_count: u32,
    },
}

impl TilesetCandidate {
    /// The picker's label.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Image { path } => format!("Image {path}"),
            Self::Map { map, tileset, .. } => format!("{tileset} (from {map})"),
        }
    }

    /// The source, with `tile_size` for an image, under `root`.
    #[must_use]
    pub fn source(&self, root: &Path, tile_size: [u32; 2]) -> TilesetSource {
        match self {
            Self::Image { path } => TilesetSource::Image {
                path: root.join(path),
                tile_size,
                name: None,
            },
            Self::Map { map, tileset, .. } => TilesetSource::Map {
                map: root.join(map),
                tileset: tileset.clone(),
            },
        }
    }
}

/// The `.png` images and `.tilemap.ron` tilesets under `root/assets` (or `root` when it has no
/// `assets` folder), sorted by path; at most [`MAX_CANDIDATES`]. Folders starting with `.` are
/// skipped; the walk is bounded in depth and entries. Unreadable maps are left out.
#[must_use]
pub fn tileset_candidates(root: &Path) -> Vec<TilesetCandidate> {
    let base = if root.join("assets").is_dir() {
        root.join("assets")
    } else {
        root.to_path_buf()
    };
    let mut files = Vec::new();
    let mut stack = vec![(base, 0usize)];
    let mut visited = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        if visited > MAX_SCAN_ENTRIES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_SCAN_ENTRIES {
                break;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if name.starts_with('.') {
                continue;
            }
            match entry.file_type() {
                Ok(t) if t.is_dir() && depth < MAX_SCAN_DEPTH => stack.push((path, depth + 1)),
                Ok(t) if t.is_file() && (name.ends_with(".png") || name.ends_with(".tilemap.ron")) => {
                    files.push(path);
                }
                _ => {}
            }
        }
    }
    files.sort();
    let mut out = Vec::new();
    for path in files {
        let rel = relative_path(root, &path);
        if rel.to_ascii_lowercase().ends_with(".png") {
            out.push(TilesetCandidate::Image { path: rel });
        } else if let Some(map) = read_limited(&path)
            .ok()
            .and_then(|t| TileMapAsset::from_ron(&t).ok())
        {
            for set in map.tilesets {
                out.push(TilesetCandidate::Map {
                    map: rel.clone(),
                    tileset: set.name,
                    tile_size: [set.tile_width, set.tile_height],
                    tile_count: set.tile_count,
                });
            }
        }
        if out.len() >= MAX_CANDIDATES {
            out.truncate(MAX_CANDIDATES);
            break;
        }
    }
    out
}

/// One edit of a map's tileset binding or layer order.
#[derive(Debug, Clone, PartialEq)]
pub enum BindingOp {
    /// Appends a tileset.
    AddTileset(TilesetDef),
    /// Removes tileset `index`; its tiles become empty.
    RemoveTileset(usize),
    /// Replaces tileset `index` (its tiles keep their local ids; ids past the new count empty).
    ReplaceTileset(usize, TilesetDef),
    /// Moves tileset `from` to position `to`.
    MoveTileset {
        /// Current index.
        from: usize,
        /// New index.
        to: usize,
    },
    /// Moves layer `from` to position `to` (layers are bottom first).
    MoveLayer {
        /// Current index.
        from: usize,
        /// New index.
        to: usize,
    },
}

/// `map` with `op` applied, validated. Tilesets are renumbered compactly and every GID remapped
/// (see the module docs); the input is not changed.
///
/// # Errors
///
/// [`Mode2dError::BadParams`] for an index out of range, a duplicate tileset name, a tileset with
/// no tiles, or a result the format refuses.
pub fn apply_binding(map: &TileMapAsset, op: &BindingOp) -> Result<TileMapAsset, Mode2dError> {
    apply_binding_kept(map, op).map(|(out, _)| out)
}

/// [`apply_binding`], plus the `(old index, new index)` pairs of the tilesets the edit keeps (a
/// replaced tileset keeps its slot; a removed one has no pair). [`remap_raw_gid`] maps a GID
/// through them, as the map's own cells were.
fn apply_binding_kept(
    map: &TileMapAsset,
    op: &BindingOp,
) -> Result<(TileMapAsset, Vec<(usize, usize)>), Mode2dError> {
    let fail = Mode2dError::BadParams;
    let count = map.tilesets.len();
    let check = |i: usize, what: &str, len: usize| {
        if i < len {
            Ok(())
        } else {
            Err(fail(format!("{what} {i} does not exist ({len} in the map)")))
        }
    };
    let mut out = map.clone();
    // The new tileset list, each with the index it had before (None: new).
    let mut sets: Vec<(Option<usize>, TilesetDef)> = map
        .tilesets
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, s)| (Some(i), s))
        .collect();
    match op {
        BindingOp::MoveLayer { from, to } => {
            check(*from, "layer", map.layers.len())?;
            check(*to, "layer", map.layers.len())?;
            let layer = out.layers.remove(*from);
            out.layers.insert(*to, layer);
            return Ok((out, (0..count).map(|i| (i, i)).collect()));
        }
        BindingOp::AddTileset(set) => sets.push((None, set.clone())),
        BindingOp::RemoveTileset(i) => {
            check(*i, "tileset", count)?;
            sets.remove(*i);
        }
        BindingOp::ReplaceTileset(i, set) => {
            check(*i, "tileset", count)?;
            sets[*i].1 = set.clone();
        }
        BindingOp::MoveTileset { from, to } => {
            check(*from, "tileset", count)?;
            check(*to, "tileset", count)?;
            let set = sets.remove(*from);
            sets.insert(*to, set);
        }
    }
    for (index, (_, set)) in sets.iter().enumerate() {
        if set.tile_count == 0 {
            return Err(fail(format!("tileset `{}` has no tiles", set.name)));
        }
        if sets[..index].iter().any(|(_, s)| s.name == set.name) {
            return Err(fail(format!(
                "the map already has a tileset named `{}`",
                set.name
            )));
        }
    }
    // Old tileset index -> (new first gid, new tile count).
    let mut remap: Vec<Option<(u32, u32)>> = vec![None; count];
    let mut next = 1u32;
    for (old, set) in &mut sets {
        set.first_gid = next;
        if let Some(old) = old {
            remap[*old] = Some((next, set.tile_count));
        }
        next = next
            .checked_add(set.tile_count)
            .filter(|n| n & !GID_MASK == 0)
            .ok_or_else(|| fail("the tilesets hold more tiles than a GID can address".into()))?;
    }
    let new_gid = |raw: u32| -> u32 {
        let gid = raw & GID_MASK;
        let Some((set, local)) = map.tileset_for(gid) else {
            return 0;
        };
        match remap.get(set).copied().flatten() {
            Some((first, tiles)) if local < tiles => (first + local) | (raw & !GID_MASK),
            _ => 0,
        }
    };
    for layer in &mut out.layers {
        match layer {
            MapLayer::Tiles(tiles) => {
                for cell in &mut tiles.data {
                    if *cell != 0 {
                        *cell = new_gid(*cell);
                    }
                }
            }
            MapLayer::Objects(objects) => {
                for object in &mut objects.objects {
                    if let ObjectShape::Tile(gid) = &mut object.shape {
                        *gid = new_gid(*gid);
                    }
                }
            }
        }
    }
    let kept = sets
        .iter()
        .enumerate()
        .filter_map(|(new, (old, _))| old.map(|old| (old, new)))
        .collect();
    out.tilesets = sets.into_iter().map(|(_, s)| s).collect();
    out.validate().map_err(|e| fail(e.to_string()))?;
    Ok((out, kept))
}

/// `raw` (a GID with flag bits) of `from` as the same tile of `to`, through the `(from index, to
/// index)` tileset pairs: the mapping [`apply_binding`] gives the map's cells, so the brush and the
/// copied stamp keep painting the tile they showed. 0 when the tile's tileset was removed or the
/// replacement has fewer tiles (as such cells become empty).
#[must_use]
pub fn remap_raw_gid(raw: u32, from: &TileMapAsset, to: &TileMapAsset, pairs: &[(usize, usize)]) -> u32 {
    let gid = raw & GID_MASK;
    let Some((set, local)) = from.tileset_for(gid) else {
        return if gid == 0 { raw } else { 0 };
    };
    match pairs
        .iter()
        .find(|(f, _)| *f == set)
        .and_then(|(_, t)| to.tilesets.get(*t))
    {
        Some(def) if local < def.tile_count => (def.first_gid + local) | (raw & !GID_MASK),
        _ => 0,
    }
}

/// One tileset-binding or layer-order edit on the undo stack: the map before and after.
#[derive(Debug, Clone)]
pub struct TilemapEditCommand {
    /// The document's path (the command does nothing once another map is loaded).
    pub path: PathBuf,
    /// The map before the edit.
    pub before: Box<TileMapAsset>,
    /// The map after it.
    pub after: Box<TileMapAsset>,
    /// The active layer index before and after (a moved layer stays active).
    pub layer: (usize, usize),
    /// The `(before index, after index)` pairs of the tilesets the edit keeps: the brush and the
    /// copied stamp are remapped through them on apply, and through the swapped pairs on revert.
    pub kept_tilesets: Vec<(usize, usize)>,
    /// What it did ("Add tileset grass").
    pub what: String,
}

impl TilemapEditCommand {
    fn write(&self, world: &mut World, undo: bool) {
        let Some(mut mode) = world.get_resource_mut::<Mode2d>() else {
            return;
        };
        let (map, layer) = if undo {
            (&self.before, self.layer.0)
        } else {
            (&self.after, self.layer.1)
        };
        let mode = &mut *mode;
        let Some(doc) = mode.doc.as_mut().filter(|d| d.path == self.path) else {
            tracing::warn!(target: "regia_editor::mode2d", "tile map edit on {} skipped: another map is loaded", self.path.display());
            return;
        };
        let images_change = doc.map.tilesets != map.tilesets;
        if images_change {
            // The brush and the stamp hold raw GIDs: renumber them with the map's cells (review
            // finding 8 of ADR-7742), or the next stroke paints another tileset's tile.
            let pairs: Vec<(usize, usize)> = if undo {
                self.kept_tilesets.iter().map(|&(b, a)| (a, b)).collect()
            } else {
                self.kept_tilesets.clone()
            };
            let remap = |raw: u32| remap_raw_gid(raw, &doc.map, map, &pairs);
            if let super::Brush2d::Tile(raw) = &mut mode.brush {
                *raw = remap(*raw);
            }
            if let Some(stamp) = &mut mode.stamp {
                for cell in &mut stamp.cells {
                    *cell = remap(*cell);
                }
            }
        }
        doc.map = (**map).clone();
        doc.dirty = true;
        if images_change {
            doc.images = super::tiles::load_images(&doc.map, &doc.path);
            mode.image_generation += 1;
        }
        mode.layer = layer;
    }
}

impl EditorCommand for TilemapEditCommand {
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

/// Applies `op` to the 2D mode's map as one undo entry (already applied when pushed). Returns the
/// command's description.
///
/// # Errors
///
/// [`Mode2dError::NoMap`] with no map loaded, or the errors of [`apply_binding`].
pub fn edit_binding(world: &mut World, op: &BindingOp) -> Result<String, Mode2dError> {
    let mode = world.get_resource::<Mode2d>().ok_or(Mode2dError::NoMap)?;
    let doc = mode.doc.as_ref().ok_or(Mode2dError::NoMap)?;
    let (after, kept_tilesets) = apply_binding_kept(&doc.map, op)?;
    let layer_before = mode.layer;
    let layer_after = match *op {
        BindingOp::MoveLayer { from, to } => {
            // The active layer keeps its identity through the move.
            let mut order: Vec<usize> = (0..doc.map.layers.len()).collect();
            let moved = order.remove(from);
            order.insert(to, moved);
            order
                .iter()
                .position(|&l| l == layer_before)
                .unwrap_or(layer_before)
        }
        _ => layer_before,
    };
    let what = match op {
        BindingOp::AddTileset(set) => format!("Add tileset {}", set.name),
        BindingOp::RemoveTileset(i) => format!("Remove tileset {}", doc.map.tilesets[*i].name),
        BindingOp::ReplaceTileset(i, set) => {
            format!("Replace tileset {} with {}", doc.map.tilesets[*i].name, set.name)
        }
        BindingOp::MoveTileset { .. } => "Reorder tilesets".to_string(),
        BindingOp::MoveLayer { from, .. } => format!("Move layer {}", doc.map.layers[*from].name()),
    };
    let command = TilemapEditCommand {
        path: doc.path.clone(),
        before: Box::new(doc.map.clone()),
        after: Box::new(after),
        layer: (layer_before, layer_after),
        kept_tilesets,
        what: what.clone(),
    };
    let mut boxed: Box<dyn EditorCommand> = Box::new(command);
    boxed.apply(world);
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    stack.push(boxed);
    stack.seal();
    Ok(what)
}

/// Writes `map` to a new file at `path` through the format's canonical writer
/// (`TileMapAsset::to_ron`), atomically (a temporary file, then a rename; the folder is created).
/// Refuses a path that exists or is not a `.tilemap.ron`. Returns the bytes written.
///
/// # Errors
///
/// [`Mode2dError::NotTilemapRon`], [`Mode2dError::BadParams`] (the file exists) or
/// [`Mode2dError::Save`].
pub fn write_new_tilemap(path: &Path, map: &TileMapAsset) -> Result<usize, Mode2dError> {
    if !is_tilemap_ron(path) {
        return Err(Mode2dError::NotTilemapRon(path.display().to_string()));
    }
    if path.exists() {
        return Err(Mode2dError::BadParams(format!(
            "{} already exists",
            path.display()
        )));
    }
    let text = map.to_ron().map_err(|e| Mode2dError::Save(e.to_string()))?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| Mode2dError::Save(format!("{}: {e}", parent.display())))?;
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Mode2dError::Save("the path has no file name".into()))?;
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text.as_bytes())
        .map_err(|e| Mode2dError::Save(format!("{}: {e}", tmp.display())))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(Mode2dError::Save(format!("{}: {e}", path.display())));
    }
    Ok(text.len())
}

/// Creates the map `spec` describes at `path` (sources resolved against it), writes it and opens
/// it in the 2D mode (refusing unsaved edits of the loaded map unless `discard`). Returns the
/// bytes written.
///
/// # Errors
///
/// The errors of [`resolve_source`], [`new_tilemap`], [`write_new_tilemap`] and
/// [`super::open_map`]; [`Mode2dError::Unsaved`] before anything is written.
pub fn create_and_open(
    world: &mut World,
    path: &Path,
    spec: &NewTilemap,
    sources: &[TilesetSource],
    discard: bool,
) -> Result<usize, Mode2dError> {
    if !discard
        && world
            .get_resource::<Mode2d>()
            .and_then(|m| m.doc.as_ref())
            .is_some_and(|d| d.dirty)
    {
        return Err(Mode2dError::Unsaved);
    }
    let mut spec = spec.clone();
    for source in sources {
        spec.tilesets.push(resolve_source(path, source)?);
    }
    let map = new_tilemap(&spec)?;
    let bytes = write_new_tilemap(path, &map)?;
    super::open_map(world, path, true)?;
    Ok(bytes)
}

// ---- Agent methods ------------------------------------------------------------------------

fn pair(params: &Value, key: &str) -> Result<Option<[u32; 2]>, RpcError> {
    match params.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_array()
            .and_then(|a| match a.as_slice() {
                [w, h] => Some([w.as_u64()?, h.as_u64()?]),
                _ => None,
            })
            .and_then(|[w, h]| Some([u32::try_from(w).ok()?, u32::try_from(h).ok()?]))
            .map(Some)
            .ok_or_else(|| bad(format!("`{key}` is [width, height]"))),
    }
}

/// A tileset source of an agent request: `{ image, tile_size?, name? }` or `{ map, tileset }`.
fn parse_source(world: &World, value: &Value, tile_size: [u32; 2]) -> Result<TilesetSource, RpcError> {
    if let Some(image) = value.get("image").and_then(Value::as_str) {
        return Ok(TilesetSource::Image {
            path: resolve_path(world, "image", image)?,
            tile_size: pair(value, "tile_size")?.unwrap_or(tile_size),
            name: value.get("name").and_then(Value::as_str).map(str::to_string),
        });
    }
    if let (Some(map), Some(tileset)) = (
        value.get("map").and_then(Value::as_str),
        value.get("tileset").and_then(Value::as_str),
    ) {
        return Ok(TilesetSource::Map {
            map: resolve_path(world, "map", map)?,
            tileset: tileset.to_string(),
        });
    }
    Err(bad(
        "a tileset source is {image, tile_size?, name?} or {map, tileset}",
    ))
}

fn tilesets_json(map: &TileMapAsset) -> Value {
    json!(
        map.tilesets
            .iter()
            .map(|s| json!({
                "name": s.name,
                "first_gid": s.first_gid,
                "tile_count": s.tile_count,
                "tile_size": [s.tile_width, s.tile_height],
                "image": s.image,
            }))
            .collect::<Vec<_>>()
    )
}

fn layers_json(map: &TileMapAsset) -> Value {
    json!(
        map.layers
            .iter()
            .enumerate()
            .map(|(i, l)| json!({
                "index": i,
                "name": l.name(),
                "kind": if matches!(l, MapLayer::Tiles(_)) { "tiles" } else { "objects" },
            }))
            .collect::<Vec<_>>()
    )
}

/// `editor.tilemap_new`.
fn tilemap_new(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path").ok_or_else(|| bad("`path` (a .tilemap.ron) is required"))?;
    let path = resolve_path(world, "path", path)?;
    let mut spec = NewTilemap::default();
    if let Some(size) = pair(params, "size")? {
        spec.size = size;
    }
    if let Some(tile) = pair(params, "tile_size")? {
        spec.tile_size = tile;
    }
    if let Some(layer) = params.get("layer") {
        spec.layer = layer
            .as_str()
            .ok_or_else(|| bad("`layer` is a name"))?
            .to_string();
    }
    let sources = match params.get("tilesets") {
        None => Vec::new(),
        Some(Value::Array(list)) => list
            .iter()
            .map(|v| parse_source(world, v, spec.tile_size))
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(bad("`tilesets` is a list of sources")),
    };
    let discard = params.get("discard").and_then(Value::as_bool).unwrap_or(false);
    let bytes = create_and_open(world, &path, &spec, &sources, discard).map_err(|e| mode2d_error(&e))?;
    let map = world
        .get_resource::<Mode2d>()
        .and_then(|m| m.doc.as_ref())
        .map(|d| d.map.clone())
        .ok_or_else(|| mode2d_error(&Mode2dError::NoMap))?;
    Ok(json!({
        "path": path.display().to_string(),
        "bytes": bytes,
        "version": map.version,
        "tilesets": tilesets_json(&map),
        "layers": layers_json(&map),
    }))
}

/// A tileset of the loaded map by index or name.
fn tileset_index(map: &TileMapAsset, value: Option<&Value>) -> Result<usize, RpcError> {
    match value {
        Some(Value::String(name)) => map
            .tilesets
            .iter()
            .position(|s| s.name == *name)
            .ok_or_else(|| bad(format!("the map has no tileset `{name}`"))),
        Some(v) => v
            .as_u64()
            .and_then(|i| usize::try_from(i).ok())
            .ok_or_else(|| bad("`tileset` is a name or an index")),
        None => Err(bad("`tileset` (name or index) is required")),
    }
}

fn index_param(params: &Value, key: &str) -> Result<usize, RpcError> {
    params
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|i| usize::try_from(i).ok())
        .ok_or_else(|| bad(format!("`{key}` is an index")))
}

/// `editor.tilemap_tilesets`.
fn tilemap_tilesets(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op =
        str_param(params, "op").ok_or_else(|| bad("`op` is add, remove, replace, move or layer_move"))?;
    let (map, map_path) = world
        .get_resource::<Mode2d>()
        .and_then(|m| m.doc.as_ref())
        .map(|d| (d.map.clone(), d.path.clone()))
        .ok_or_else(|| mode2d_error(&Mode2dError::NoMap))?;
    let tile_size = [map.tile_width, map.tile_height];
    let source = |world: &World| -> Result<TilesetDef, RpcError> {
        let value = params.get("source").ok_or_else(|| bad("`source` is required"))?;
        let source = parse_source(world, value, tile_size)?;
        resolve_source(&map_path, &source).map_err(|e| mode2d_error(&e))
    };
    let op = match op {
        "add" => BindingOp::AddTileset(source(world)?),
        "remove" => BindingOp::RemoveTileset(tileset_index(&map, params.get("tileset"))?),
        "replace" => BindingOp::ReplaceTileset(tileset_index(&map, params.get("tileset"))?, source(world)?),
        "move" => BindingOp::MoveTileset {
            from: tileset_index(&map, params.get("tileset"))?,
            to: index_param(params, "to")?,
        },
        "layer_move" => BindingOp::MoveLayer {
            from: match params.get("layer") {
                Some(Value::String(name)) => map
                    .layer_index(name)
                    .ok_or_else(|| bad(format!("the map has no layer `{name}`")))?,
                _ => index_param(params, "layer")?,
            },
            to: index_param(params, "to")?,
        },
        other => {
            return Err(bad(format!(
                "unknown op `{other}` (add, remove, replace, move, layer_move)"
            )));
        }
    };
    let what = edit_binding(world, &op).map_err(|e| mode2d_error(&e))?;
    let mode = world.resource::<Mode2d>();
    let map = mode
        .doc
        .as_ref()
        .map(|d| &d.map)
        .ok_or_else(|| mode2d_error(&Mode2dError::NoMap))?;
    let undo_depth = world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth);
    Ok(json!({
        "done": what,
        "tilesets": tilesets_json(map),
        "layers": layers_json(map),
        "layer": mode.layer,
        "undo_depth": undo_depth,
    }))
}

/// `editor.tileset_candidates`.
fn candidates(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let root = crate::mesh_source::project_root(world).unwrap_or_else(|| crate::agent_ext::agent_root(world));
    let list: Vec<Value> = tileset_candidates(&root)
        .into_iter()
        .map(|c| match c {
            TilesetCandidate::Image { path } => json!({ "kind": "image", "path": path }),
            TilesetCandidate::Map {
                map,
                tileset,
                tile_size,
                tile_count,
            } => json!({ "kind": "map", "map": map, "tileset": tileset, "tile_size": tile_size, "tile_count": tile_count }),
        })
        .collect();
    Ok(json!({ "candidates": list }))
}

const NEW_PARAMS: &[ParamSchema] = &[
    param(
        "path",
        "string",
        true,
        "The new .tilemap.ron, project-relative (must not exist).",
    ),
    param(
        "size",
        "array",
        false,
        "[width, height] in cells, 1..=4096 (default [16, 16]).",
    ),
    param(
        "tile_size",
        "array",
        false,
        "[width, height] of a cell in pixels, 1..=4096 (default [16, 16]).",
    ),
    param(
        "layer",
        "string",
        false,
        "Name of the tile layer (default ground).",
    ),
    param(
        "tilesets",
        "array",
        false,
        "Sources: {image, tile_size?, name?} (an image cut into cells) or {map, tileset} (a tileset of another .tilemap.ron).",
    ),
    param(
        "discard",
        "boolean",
        false,
        "Drop unsaved edits of the loaded map.",
    ),
];

const TILESETS_PARAMS: &[ParamSchema] = &[
    param(
        "op",
        "string",
        true,
        "add, remove, replace, move (a tileset) or layer_move.",
    ),
    param(
        "tileset",
        "string",
        false,
        "remove, replace, move: the tileset's name or index.",
    ),
    param(
        "source",
        "object",
        false,
        "add, replace: {image, tile_size?, name?} or {map, tileset}.",
    ),
    param("layer", "string", false, "layer_move: the layer's name or index."),
    param("to", "integer", false, "move, layer_move: the new index."),
];

/// The new-map and tileset-binding agent methods.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.tilemap_new",
                tool: "tilemap_new",
                description: "Creates a .tilemap.ron at the current format version (grid size, cell size, one empty \
                    tile layer, the tilesets bound from images or other maps), writes it through the format's \
                    writer and opens it in the 2D mode. Errors in data.mode2d_error: mode2d_bad_params, \
                    mode2d_not_tilemap_ron, mode2d_load_failed, mode2d_save_failed, mode2d_unsaved.",
                params: NEW_PARAMS,
                result: "{ path, bytes, version, tilesets, layers }",
                mutating: true,
                permission: Permission::EditScene,
            },
            tilemap_new as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tilemap_tilesets",
                tool: "tilemap_tilesets",
                description: "Edits the 2D mode map's tileset binding (add, remove, replace, move a tileset) or its \
                    layer order (layer_move), as one undo entry. Tilesets renumber from GID 1 in list order and \
                    every tile is remapped; tiles of a removed tileset become empty. Errors in \
                    data.mode2d_error: mode2d_bad_params, mode2d_no_map, mode2d_load_failed.",
                params: TILESETS_PARAMS,
                result: "{ done, tilesets, layers, layer, undo_depth }",
                mutating: true,
                permission: Permission::EditScene,
            },
            tilemap_tilesets as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tileset_candidates",
                tool: "tileset_candidates",
                description: "The tilesets the 2D tab's picker offers: .png images and the tilesets of .tilemap.ron \
                    files under the project's assets folder (at most 512).",
                params: &[],
                result: "{ candidates: [{ kind: image, path } | { kind: map, map, tileset, tile_size, tile_count }] }",
                mutating: false,
                permission: Permission::Read,
            },
            candidates as Handler,
        ),
    ]
}

// ---- The 2D tab's sections ----------------------------------------------------------------

/// The fields of the 2D tab's **New tile map** form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMapForm {
    /// Project-relative path of the new file.
    pub path: String,
    /// Grid size in cells.
    pub size: [u32; 2],
    /// Cell size in pixels.
    pub tile_size: [u32; 2],
    /// The candidate (index into [`BindingPanel::candidates`]) bound as its tileset.
    pub tileset: Option<usize>,
}

impl Default for NewMapForm {
    fn default() -> Self {
        Self {
            path: "assets/maps/new.tilemap.ron".into(),
            size: DEFAULT_MAP_SIZE,
            tile_size: DEFAULT_TILE_SIZE,
            tileset: None,
        }
    }
}

/// The state of the new-map and tileset-binding sections (a world resource).
#[derive(Resource, Debug, Default)]
pub struct BindingPanel {
    /// The new-map form.
    pub form: NewMapForm,
    /// The picker's tilesets ([`tileset_candidates`]; refreshed by **Refresh list**, or when a
    /// section opens with an empty list).
    pub candidates: Vec<TilesetCandidate>,
    /// The list was scanned at least once.
    pub scanned: bool,
    /// The candidate **Add tileset** and **Replace** use.
    pub pick: Option<usize>,
    /// The cell size an image candidate is cut into.
    pub tile_size: [u32; 2],
    /// The last failed action.
    pub error: Option<String>,
}

enum BindAction {
    Refresh,
    Create,
    Op(BindingOp),
    AddPicked,
    ReplacePicked(usize),
}

fn project_root(world: &World) -> PathBuf {
    crate::mesh_source::project_root(world).unwrap_or_else(|| crate::agent_ext::agent_root(world))
}

fn candidate_combo(ui: &mut egui::Ui, id: &str, candidates: &[TilesetCandidate], pick: &mut Option<usize>) {
    let text = pick
        .and_then(|i| candidates.get(i))
        .map_or_else(|| "(no tileset)".to_string(), TilesetCandidate::label);
    egui::ComboBox::from_id_salt(id)
        .selected_text(text)
        .show_ui(ui, |ui| {
            ui.selectable_value(pick, None, "(no tileset)");
            for (i, c) in candidates.iter().enumerate() {
                ui.selectable_value(pick, Some(i), c.label());
            }
        });
}

fn size_row(ui: &mut egui::Ui, label: &str, value: &mut [u32; 2], max: u32) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(egui::DragValue::new(&mut value[0]).range(1..=max));
        ui.label("x");
        ui.add(egui::DragValue::new(&mut value[1]).range(1..=max));
    });
}

/// Draws the **New tile map** and **Tilesets and layers** sections of the 2D tab (collapsed by
/// default) and applies their clicks.
pub fn section_ui(ui: &mut egui::Ui, world: &mut World) {
    let mut panel = world.remove_resource::<BindingPanel>().unwrap_or_default();
    if panel.tile_size == [0, 0] {
        panel.tile_size = DEFAULT_TILE_SIZE;
    }
    let mut actions = Vec::new();
    egui::CollapsingHeader::new("New tile map")
        .id_salt("mode2d-new-map")
        .show(ui, |ui| {
            if !panel.scanned {
                actions.push(BindAction::Refresh);
            }
            ui.horizontal(|ui| {
                ui.label("File");
                ui.text_edit_singleline(&mut panel.form.path);
            });
            size_row(ui, "Grid (cells)", &mut panel.form.size, MAX_NEW_MAP_SIDE);
            size_row(ui, "Cell (px)", &mut panel.form.tile_size, MAX_NEW_TILE_SIDE);
            ui.horizontal(|ui| {
                ui.label("Tileset");
                let BindingPanel { form, candidates, .. } = &mut panel;
                candidate_combo(ui, "mode2d-new-map-tileset", candidates, &mut form.tileset);
            });
            ui.horizontal(|ui| {
                if ui.button("Refresh list").clicked() {
                    actions.push(BindAction::Refresh);
                }
                if ui.button("Create and open").clicked() {
                    actions.push(BindAction::Create);
                }
            });
        });
    let doc = world
        .get_resource::<Mode2d>()
        .and_then(|m| m.doc.as_ref())
        .map(|d| {
            (
                d.map.tilesets.clone(),
                d.map
                    .layers
                    .iter()
                    .map(|l| l.name().to_string())
                    .collect::<Vec<_>>(),
            )
        });
    if let Some((tilesets, layers)) = doc {
        egui::CollapsingHeader::new("Tilesets and layers")
            .id_salt("mode2d-binding")
            .show(ui, |ui| {
                if !panel.scanned {
                    actions.push(BindAction::Refresh);
                }
                for (i, set) in tilesets.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "{} ({} tiles, gid {})",
                            set.name, set.tile_count, set.first_gid
                        ));
                        if ui.add_enabled(i > 0, egui::Button::new("Up").small()).clicked() {
                            actions.push(BindAction::Op(BindingOp::MoveTileset { from: i, to: i - 1 }));
                        }
                        if ui
                            .add_enabled(i + 1 < tilesets.len(), egui::Button::new("Down").small())
                            .clicked()
                        {
                            actions.push(BindAction::Op(BindingOp::MoveTileset { from: i, to: i + 1 }));
                        }
                        if ui.small_button("Replace").clicked() {
                            actions.push(BindAction::ReplacePicked(i));
                        }
                        if ui.small_button("Remove").clicked() {
                            actions.push(BindAction::Op(BindingOp::RemoveTileset(i)));
                        }
                    });
                }
                ui.horizontal(|ui| {
                    let BindingPanel { candidates, pick, .. } = &mut panel;
                    candidate_combo(ui, "mode2d-binding-pick", candidates, pick);
                    if ui.button("Add tileset").clicked() {
                        actions.push(BindAction::AddPicked);
                    }
                });
                size_row(ui, "Image cells (px)", &mut panel.tile_size, MAX_NEW_TILE_SIDE);
                ui.separator();
                ui.weak("Layers, bottom first");
                for (i, name) in layers.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(name);
                        if ui
                            .add_enabled(i > 0, egui::Button::new("Lower").small())
                            .clicked()
                        {
                            actions.push(BindAction::Op(BindingOp::MoveLayer { from: i, to: i - 1 }));
                        }
                        if ui
                            .add_enabled(i + 1 < layers.len(), egui::Button::new("Raise").small())
                            .clicked()
                        {
                            actions.push(BindAction::Op(BindingOp::MoveLayer { from: i, to: i + 1 }));
                        }
                    });
                }
            });
    }
    if let Some(error) = &panel.error {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }
    // Once per egui frame: a second pass of the frame sees the same click (Up / Down would swap back,
    // Remove would drop two tilesets).
    if !actions.is_empty()
        && super::first_apply_this_frame(ui.ctx(), egui::Id::new("mode2d-binding-applied-frame"))
    {
        for action in actions {
            let result = apply_section(world, &mut panel, action);
            panel.error = result.err();
        }
    }
    world.insert_resource(panel);
}

fn picked(world: &World, panel: &BindingPanel, map_path: &Path) -> Result<TilesetDef, String> {
    let candidate = panel
        .pick
        .and_then(|i| panel.candidates.get(i))
        .ok_or_else(|| "pick a tileset first".to_string())?;
    resolve_source(map_path, &candidate.source(&project_root(world), panel.tile_size))
        .map_err(|e| e.to_string())
}

fn apply_section(world: &mut World, panel: &mut BindingPanel, action: BindAction) -> Result<(), String> {
    let map_path = || {
        world
            .get_resource::<Mode2d>()
            .and_then(|m| m.doc.as_ref())
            .map(|d| d.path.clone())
            .ok_or_else(|| Mode2dError::NoMap.to_string())
    };
    match action {
        BindAction::Refresh => {
            panel.candidates = tileset_candidates(&project_root(world));
            panel.scanned = true;
            if panel.pick.is_some_and(|i| i >= panel.candidates.len()) {
                panel.pick = None;
            }
            Ok(())
        }
        BindAction::Create => {
            let path = crate::agent_ext::document_request_path(world, "path", &panel.form.path)
                .map_err(|e| e.message)?;
            let root = project_root(world);
            let sources: Vec<TilesetSource> = panel
                .form
                .tileset
                .and_then(|i| panel.candidates.get(i))
                .map(|c| c.source(&root, panel.form.tile_size))
                .into_iter()
                .collect();
            let spec = NewTilemap {
                size: panel.form.size,
                tile_size: panel.form.tile_size,
                ..NewTilemap::default()
            };
            create_and_open(world, &path, &spec, &sources, false).map_err(|e| e.to_string())?;
            panel.scanned = false;
            Ok(())
        }
        BindAction::Op(op) => edit_binding(world, &op).map(|_| ()).map_err(|e| e.to_string()),
        BindAction::AddPicked => {
            let set = picked(world, panel, &map_path()?)?;
            edit_binding(world, &BindingOp::AddTileset(set))
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        BindAction::ReplacePicked(i) => {
            let set = picked(world, panel, &map_path()?)?;
            edit_binding(world, &BindingOp::ReplaceTileset(i, set))
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use regia_2d::tilemap::{FLIP_X_BIT, FLIP_Y_BIT, ObjectLayer};

    use super::*;

    fn project() -> PathBuf {
        let dir = regia_test_utils::unique_temp_dir("mode2d-new-map");
        std::fs::create_dir_all(dir.join("assets/tiles")).expect("dir");
        image::RgbaImage::new(64, 32)
            .save(dir.join("assets/tiles/grass.png"))
            .expect("png");
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

    fn set(name: &str, count: u32) -> TilesetDef {
        TilesetDef {
            name: name.into(),
            first_gid: 1,
            image: format!("{name}.png"),
            image_width: 16 * count,
            image_height: 16,
            tile_width: 16,
            tile_height: 16,
            columns: count,
            tile_count: count,
            spacing: 0,
            margin: 0,
            tiles: Vec::new(),
        }
    }

    /// The map `editor.tilemap_new` must write, built by hand.
    fn expected_new_map() -> TileMapAsset {
        TileMapAsset {
            version: TILEMAP_FORMAT_VERSION,
            orientation: Orientation::Orthogonal,
            width: 6,
            height: 4,
            tile_width: 16,
            tile_height: 16,
            background: None,
            tilesets: vec![TilesetDef {
                name: "grass".into(),
                first_gid: 1,
                image: "../tiles/grass.png".into(),
                image_width: 64,
                image_height: 32,
                tile_width: 16,
                tile_height: 16,
                columns: 4,
                tile_count: 8,
                spacing: 0,
                margin: 0,
                tiles: Vec::new(),
            }],
            layers: vec![MapLayer::Tiles(TileLayer {
                name: "ground".into(),
                visible: true,
                opacity: 1.0,
                offset: [0.0, 0.0],
                data: vec![0; 24],
                properties: BTreeMap::new(),
            })],
            properties: BTreeMap::new(),
        }
    }

    #[test]
    fn relative_paths_step_out_of_the_map_folder() {
        assert_eq!(
            relative_path(Path::new("/p/assets/maps"), Path::new("/p/assets/tiles/a.png")),
            "../tiles/a.png"
        );
        assert_eq!(
            relative_path(Path::new("/p/maps"), Path::new("/p/maps/a.png")),
            "a.png"
        );
        assert_eq!(
            relative_path(Path::new("a/b"), Path::new("a/./c/d.png")),
            "../c/d.png"
        );
    }

    #[test]
    fn a_new_map_is_written_by_the_canonical_writer_loads_in_the_runtime_and_paints() {
        let dir = project();
        let mut world = agent_world(&dir);
        let out = call(
            &mut world,
            "editor.tilemap_new",
            json!({
                "path": "assets/maps/level.tilemap.ron",
                "size": [6, 4],
                "tile_size": [16, 16],
                "tilesets": [{ "image": "assets/tiles/grass.png" }],
            }),
        )
        .expect("creates");
        assert_eq!(out["version"], TILEMAP_FORMAT_VERSION);
        assert_eq!(out["tilesets"][0]["tile_count"], 8);
        let file = dir.join("assets/maps/level.tilemap.ron");
        let bytes = std::fs::read_to_string(&file).expect("written");
        assert_eq!(
            bytes,
            expected_new_map().to_ron().expect("writes"),
            "the canonical writer's bytes"
        );
        assert_eq!(out["bytes"], bytes.len());
        // The runtime's loader takes it (version gate, migration, validation).
        let loaded = TileMapAsset::from_ron(&bytes).expect("the runtime loads it");
        assert_eq!(loaded, expected_new_map());
        // It is open in the 2D mode and paints: a stroke, a save, and the file holds the tile.
        assert!(
            world
                .resource::<Mode2d>()
                .doc
                .as_ref()
                .is_some_and(|d| d.path == file)
        );
        call(&mut world, "editor.mode2d_set", json!({ "brush": { "tile": 3 } })).expect("brush");
        let stroke = call(&mut world, "editor.tile_stroke", json!({ "cells": [[2, 1]] })).expect("paints");
        assert_eq!(stroke["changed"], 1);
        call(&mut world, "editor.tilemap_save", json!({})).expect("saves");
        let saved = TileMapAsset::from_ron(&std::fs::read_to_string(&file).expect("read")).expect("parses");
        assert_eq!(saved.tile_at(0, 2, 1).map(|t| (t.gid, t.local_id)), Some((3, 2)));
        // A second create on the same path is refused and writes nothing.
        let err = call(
            &mut world,
            "editor.tilemap_new",
            json!({ "path": "assets/maps/level.tilemap.ron" }),
        )
        .expect_err("exists");
        assert_eq!(code(&err), "mode2d_bad_params");
        let err = call(
            &mut world,
            "editor.tilemap_new",
            json!({ "path": "assets/maps/x.tmx" }),
        )
        .expect_err("kind");
        assert_eq!(code(&err), "mode2d_not_tilemap_ron");
        let err = call(
            &mut world,
            "editor.tilemap_new",
            json!({ "path": "assets/maps/y.tilemap.ron", "size": [0, 4] }),
        )
        .expect_err("size");
        assert_eq!(code(&err), "mode2d_bad_params");
        assert!(!dir.join("assets/maps/y.tilemap.ron").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn content_new_tile_map_writes_the_default_map_at_the_current_version() {
        let dir = project();
        let rel = crate::content::create_asset(
            &dir,
            crate::content::NewAssetKind::Tilemap,
            "assets/maps/new.tilemap.ron",
        )
        .expect("creates");
        assert_eq!(rel, "assets/maps/new.tilemap.ron");
        let text = std::fs::read_to_string(dir.join(&rel)).expect("written");
        let map = TileMapAsset::from_ron(&text).expect("the runtime loads it");
        assert_eq!(map.version, TILEMAP_FORMAT_VERSION);
        assert_eq!([map.width, map.height], DEFAULT_MAP_SIZE);
        assert_eq!([map.tile_width, map.tile_height], DEFAULT_TILE_SIZE);
        assert_eq!(
            text,
            new_tilemap(&NewTilemap::default())
                .expect("map")
                .to_ron()
                .expect("ron")
        );
        assert_eq!(
            crate::content::NewAssetKind::parse("tilemap"),
            Ok(crate::content::NewAssetKind::Tilemap)
        );
        assert!(crate::content::NewAssetKind::ALL.contains(&crate::content::NewAssetKind::Tilemap));
        assert!(
            crate::content::create_asset(
                &dir,
                crate::content::NewAssetKind::Tilemap,
                "assets/maps/n.level.ron"
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Two tilesets (a: 4 tiles, b: 3 tiles), tiles of both with flips, a tile object of b.
    fn two_set_map() -> TileMapAsset {
        let mut map = new_tilemap(&NewTilemap {
            size: [4, 1],
            tile_size: [16, 16],
            layer: "ground".into(),
            tilesets: vec![set("a", 4), set("b", 3)],
        })
        .expect("map");
        if let MapLayer::Tiles(t) = &mut map.layers[0] {
            // a:0, a:3 flipped x, b:0, b:2 flipped y.
            t.data = vec![1, 4 | FLIP_X_BIT, 5, 7 | FLIP_Y_BIT];
        }
        map.layers.push(MapLayer::Objects(ObjectLayer {
            name: "props".into(),
            visible: true,
            objects: vec![regia_2d::tilemap::MapObject {
                id: 1,
                name: String::new(),
                class: String::new(),
                position: [0.0, 0.0],
                size: [16.0, 16.0],
                rotation: 0.0,
                shape: ObjectShape::Tile(6),
                properties: BTreeMap::new(),
            }],
            properties: BTreeMap::new(),
        }));
        map
    }

    fn ground(map: &TileMapAsset) -> Vec<u32> {
        map.tile_layer(0).map(|t| t.data.clone()).unwrap_or_default()
    }

    #[test]
    fn binding_ops_renumber_the_tilesets_and_remap_every_tile() {
        let map = two_set_map();
        assert_eq!(map.tilesets[1].first_gid, 5);
        let moved = apply_binding(&map, &BindingOp::MoveTileset { from: 1, to: 0 }).expect("moves");
        assert_eq!(
            moved
                .tilesets
                .iter()
                .map(|s| (s.name.as_str(), s.first_gid))
                .collect::<Vec<_>>(),
            [("b", 1), ("a", 4)]
        );
        assert_eq!(ground(&moved), [4, 7 | FLIP_X_BIT, 1, 3 | FLIP_Y_BIT]);
        assert!(
            matches!(&moved.layers[1], MapLayer::Objects(o) if o.objects[0].shape == ObjectShape::Tile(2))
        );
        let removed = apply_binding(&map, &BindingOp::RemoveTileset(0)).expect("removes");
        assert_eq!(
            ground(&removed),
            [0, 0, 1, 3 | FLIP_Y_BIT],
            "a's tiles empty, b renumbered"
        );
        let replaced = apply_binding(&map, &BindingOp::ReplaceTileset(0, set("c", 2))).expect("replaces");
        assert_eq!(
            ground(&replaced),
            [1, 0, 3, 5 | FLIP_Y_BIT],
            "a:3 has no tile in c"
        );
        let added = apply_binding(&map, &BindingOp::AddTileset(set("d", 2))).expect("adds");
        assert_eq!(ground(&added), ground(&map));
        assert_eq!(added.tilesets[2].first_gid, 8);
        assert!(matches!(
            apply_binding(&map, &BindingOp::AddTileset(set("a", 2))),
            Err(Mode2dError::BadParams(m)) if m.contains("already has")
        ));
        assert!(apply_binding(&map, &BindingOp::RemoveTileset(5)).is_err());
        let layers = apply_binding(&map, &BindingOp::MoveLayer { from: 1, to: 0 }).expect("moves the layer");
        assert_eq!(layers.layers[0].name(), "props");
        assert_eq!(layers.tilesets, map.tilesets);
    }

    /// Review finding 8 of ADR-7742: a binding edit renumbers every GID of the map, so it renumbers
    /// the brush and the copied stamp with them (the next stroke paints the tile the brush showed);
    /// undo puts them back, and a removed tileset's tiles become empty, as the map's cells do.
    #[test]
    fn a_binding_edit_remaps_the_brush_and_the_stamp_and_undo_restores_them() {
        use crate::mode2d::Brush2d;
        let dir = project();
        let file = dir.join("assets/maps/two.tilemap.ron");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("dir");
        std::fs::write(&file, two_set_map().to_ron().expect("ron")).expect("write");
        let mut world = agent_world(&dir);
        call(
            &mut world,
            "editor.mode2d_set",
            json!({ "map": "assets/maps/two.tilemap.ron" }),
        )
        .expect("opens");
        let brush_and_stamp = |world: &World| {
            let mode = world.resource::<Mode2d>();
            (
                mode.brush.clone(),
                mode.stamp.as_ref().map(|s| s.cells.clone()).unwrap_or_default(),
            )
        };
        {
            let mut mode = world.resource_mut::<Mode2d>();
            // b:0 (gid 5) and, in the stamp, b:0 and a:1 flipped x.
            mode.brush = Brush2d::Tile(5);
            mode.stamp = Some(regia_2d::stamp::Stamp::new(2, 1, vec![5, 2 | FLIP_X_BIT]).expect("stamp"));
        }
        call(
            &mut world,
            "editor.tilemap_tilesets",
            json!({ "op": "move", "tileset": "b", "to": 0 }),
        )
        .expect("moves");
        // b now starts at gid 1 and a at gid 4.
        assert_eq!(
            brush_and_stamp(&world),
            (Brush2d::Tile(1), vec![1, 5 | FLIP_X_BIT]),
            "the brush still paints b:0 and the stamp b:0, a:1"
        );
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(
            brush_and_stamp(&world),
            (Brush2d::Tile(5), vec![5, 2 | FLIP_X_BIT]),
            "undo renumbers them back"
        );
        call(
            &mut world,
            "editor.tilemap_tilesets",
            json!({ "op": "remove", "tileset": "b" }),
        )
        .expect("removes");
        assert_eq!(
            brush_and_stamp(&world),
            (Brush2d::Tile(0), vec![0, 2 | FLIP_X_BIT]),
            "b's tiles are gone from the brush and the stamp; a keeps its numbers"
        );
    }

    #[test]
    fn the_agent_route_edits_the_binding_as_one_undo_entry_and_saves_the_canonical_bytes() {
        let dir = project();
        let file = dir.join("assets/maps/two.tilemap.ron");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("dir");
        std::fs::write(&file, two_set_map().to_ron().expect("ron")).expect("write");
        let mut world = agent_world(&dir);
        call(
            &mut world,
            "editor.mode2d_set",
            json!({ "map": "assets/maps/two.tilemap.ron" }),
        )
        .expect("opens");
        let out = call(
            &mut world,
            "editor.tilemap_tilesets",
            json!({ "op": "move", "tileset": "b", "to": 0 }),
        )
        .expect("moves");
        assert_eq!(out["tilesets"][0]["name"], "b");
        assert_eq!(out["undo_depth"], 1);
        let out = call(
            &mut world,
            "editor.tilemap_tilesets",
            json!({ "op": "add", "source": { "image": "assets/tiles/grass.png", "tile_size": [16, 16] } }),
        )
        .expect("adds");
        assert_eq!(out["tilesets"][2]["name"], "grass");
        assert_eq!(out["tilesets"][2]["image"], "../tiles/grass.png");
        let out = call(
            &mut world,
            "editor.tilemap_tilesets",
            json!({ "op": "layer_move", "layer": "props", "to": 0 }),
        )
        .expect("moves the layer");
        assert_eq!(out["layer"], 1, "the active tile layer follows its move");
        call(&mut world, "editor.tilemap_save", json!({})).expect("saves");
        let expected = [
            BindingOp::MoveTileset { from: 1, to: 0 },
            BindingOp::AddTileset(
                resolve_source(
                    &file,
                    &TilesetSource::Image {
                        path: dir.join("assets/tiles/grass.png"),
                        tile_size: [16, 16],
                        name: None,
                    },
                )
                .expect("source"),
            ),
            BindingOp::MoveLayer { from: 1, to: 0 },
        ]
        .iter()
        .try_fold(two_set_map(), |map, op| apply_binding(&map, op))
        .expect("ops");
        assert_eq!(
            std::fs::read_to_string(&file).expect("read"),
            expected.to_ron().expect("ron"),
            "the saved bytes are the canonical writer's"
        );
        // Undo three times: the map is the loaded one bit for bit.
        for _ in 0..3 {
            assert!(crate::command::undo_world(&mut world));
        }
        assert_eq!(
            world.resource::<Mode2d>().doc.as_ref().map(|d| d.map.clone()),
            Some(two_set_map())
        );
        assert_eq!(world.resource::<Mode2d>().layer, 0);
        assert!(crate::command::redo_world(&mut world));
        assert_eq!(
            world
                .resource::<Mode2d>()
                .doc
                .as_ref()
                .map(|d| d.map.tilesets[0].name.clone())
                .as_deref(),
            Some("b")
        );
        let err = call(
            &mut world,
            "editor.tilemap_tilesets",
            json!({ "op": "remove", "tileset": "zzz" }),
        )
        .expect_err("unknown");
        assert_eq!(code(&err), "mode2d_bad_params");
        let err = call(&mut world, "editor.tilemap_tilesets", json!({ "op": "spin" })).expect_err("op");
        assert_eq!(code(&err), "mode2d_bad_params");
        let list = call(&mut world, "editor.tileset_candidates", json!({})).expect("lists");
        let kinds: Vec<&str> = list["candidates"]
            .as_array()
            .expect("list")
            .iter()
            .filter_map(|c| c["kind"].as_str())
            .collect();
        assert!(kinds.contains(&"image") && kinds.contains(&"map"), "{list}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn binding_ops_without_a_map_say_so() {
        let dir = project();
        let mut world = agent_world(&dir);
        let err = call(
            &mut world,
            "editor.tilemap_tilesets",
            json!({ "op": "remove", "tileset": 0 }),
        )
        .expect_err("no map");
        assert_eq!(code(&err), "mode2d_no_map");
        let _ = std::fs::remove_dir_all(dir);
    }
}
