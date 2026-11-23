// SPDX-License-Identifier: MPL-2.0
//! The Terrain panel (ADR-2411): the terrain the open level names, heightmap export and import
//! (16-bit PNG and RAW), the grass density brush and `.scatter.ron` rule editing.
//!
//! Everything the panel does is a plain function here, so tests and the agent drive it without a
//! display:
//!
//! - [`load_open_level_terrain`] builds the terrain a v12 level names
//!   (`LevelSettings::terrain`, [`regia_terrain::spawn_level_terrain`]) and gives it a
//!   [`TerrainCookTarget`], so a sculpt saves into its cooked heightfield. A level that names none
//!   leaves the host's terrain alone.
//! - [`export_heightmap`] / [`import_heightmap`] write and read the map's own 16-bit grid
//!   (`regia_terrain::height_io`): export then import is the identity. An import of at most
//!   2^22 changed samples is one undo step; a larger import applies with no undo entry.
//! - Import source ([`crate::terrain_source_import`], ADR-8541) reads a count-based source with its
//!   datum, units and registration, previews the destination mapping and the changed samples off
//!   the editor thread, and applies only after the preview (cancel, stale and undo-limit guards).
//! - [`grass_density_stroke`] paints `<map>.grassdensity.bin` ([`regia_terrain::GrassDensityPaint`]),
//!   one undo step per stroke (a viewport drag in Terrain mode `0` is one entry), and
//!   [`save_grass_density`] writes it from every save after an edit.
//! - [`ScatterRuleEditor`] opens a `.scatter.ron`, edits a class's rule field by name, adds and
//!   removes classes, and saves canonical RON (`ScatterSet::to_ron`).

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_asset::Heightfield;
use regia_terrain::{
    GrassDensityFileRefused, GrassDensityPaint, GrassDensityStroke, GrassDensityUndo, LevelTerrainRoots,
    LevelTerrainSource, RawEndian, ScatterSet, Terrain, TerrainEdits, decode_r16, encode_r16, export_heights,
    grass_density_path, import_heights,
};
use serde_json::Value;

use crate::command::{EditorCommand, UndoStack};
use crate::terrain_tool::{TerrainCookTarget, TerrainStrokeCommand};

/// Largest heightmap side read or written (the agent's limit, `terrain.import_heights`).
pub const MAX_HEIGHTMAP_SIDE: u32 = 16_384;

/// Largest heightmap file read, bytes: a RAW of `MAX_HEIGHTMAP_SIDE`² samples plus 1 MiB.
pub const MAX_HEIGHTMAP_FILE: u64 = (MAX_HEIGHTMAP_SIDE as u64) * (MAX_HEIGHTMAP_SIDE as u64) * 2 + (1 << 20);

/// A heightmap file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeightmapFormat {
    /// 16-bit grayscale PNG (big-endian samples, as the PNG standard stores them).
    Png,
    /// Headerless 16-bit samples, row-major, row 0 at −Z.
    R16(RawEndian),
}

impl HeightmapFormat {
    /// The format a file name implies (`.png`, `.r16` / `.raw` little-endian).
    #[must_use]
    pub fn of_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "png" => Some(Self::Png),
            "r16" | "raw" => Some(Self::R16(RawEndian::Little)),
            _ => None,
        }
    }

    /// `"png"` or `"r16"`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::R16(_) => "r16",
        }
    }
}

/// A 16-bit grayscale PNG of `hf`.
///
/// # Errors
///
/// The encoder failed.
pub fn encode_png16(hf: &Heightfield) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, hf.width, hf.height);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Sixteen);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer
            .write_image_data(&encode_r16(hf, RawEndian::Big))
            .map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
    }
    Ok(bytes)
}

/// Decodes a 16-bit grayscale PNG; other color types and depths are refused.
///
/// # Errors
///
/// Not a PNG, not 16-bit gray, or a side outside `2..=MAX_HEIGHTMAP_SIDE`.
pub fn decode_png16(bytes: &[u8]) -> Result<Heightfield, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::IDENTITY);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let (w, h) = (reader.info().width, reader.info().height);
    if w < 2 || h < 2 || w > MAX_HEIGHTMAP_SIDE || h > MAX_HEIGHTMAP_SIDE {
        return Err(format!(
            "the png is {w}x{h}; each side must be 2..={MAX_HEIGHTMAP_SIDE}"
        ));
    }
    let (color, depth) = (reader.info().color_type, reader.info().bit_depth);
    if color != png::ColorType::Grayscale || depth != png::BitDepth::Sixteen {
        return Err(format!(
            "the png is {color:?} at {depth:?} bits; a heightmap is 16-bit grayscale"
        ));
    }
    let size = reader.output_buffer_size().ok_or("png: no output buffer")?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let data = buf.get(..info.buffer_size()).ok_or("png: short frame")?;
    decode_r16(data, w, h, RawEndian::Big).map_err(|e| e.to_string())
}

/// What [`export_heightmap`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeightmapExport {
    /// The file.
    pub path: PathBuf,
    /// Columns.
    pub width: u32,
    /// Rows.
    pub height: u32,
    /// Bytes written.
    pub bytes: u64,
    /// Samples below the map's range, clamped to 0.
    pub clamped_low: u64,
    /// Samples above the map's range, clamped to 65535.
    pub clamped_high: u64,
}

/// Writes the 16-bit grid of `entity`'s terrain to `path` (atomically). The encoding is the map's
/// own: `round((h − offset) · 65535 / scale)` (`docs/formats/terrain.md` § 11).
///
/// # Errors
///
/// No terrain on `entity`, an encoder failure or a write failure.
pub fn export_heightmap(
    world: &World,
    entity: Entity,
    path: &Path,
    format: HeightmapFormat,
) -> Result<HeightmapExport, String> {
    let _span = tracing::info_span!(target: "regia_editor", "terrain_panel.export_heightmap").entered();
    let terrain = world
        .get::<Terrain>(entity)
        .ok_or_else(|| format!("entity {} has no terrain", entity.to_bits()))?;
    let export = export_heights(&terrain.map);
    let bytes = match format {
        HeightmapFormat::Png => encode_png16(&export.heightfield)?,
        HeightmapFormat::R16(endian) => encode_r16(&export.heightfield, endian),
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    crate::level_io::write_atomic(path, &bytes)?;
    Ok(HeightmapExport {
        path: path.to_path_buf(),
        width: export.heightfield.width,
        height: export.heightfield.height,
        bytes: bytes.len() as u64,
        clamped_low: export.clamped_low,
        clamped_high: export.clamped_high,
    })
}

/// What [`import_heightmap`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeightmapImport {
    /// Samples whose height changed.
    pub changed: u64,
    /// Whether the import went on the undo stack (above `MAX_IMPORT_UNDO_SAMPLES` it does not).
    pub undoable: bool,
}

/// Reads `path` into `entity`'s terrain: the file must match the map's size (a RAW without
/// `size` is a square from its byte count). Only changed samples are written. An import of at
/// most `MAX_IMPORT_UNDO_SAMPLES` (2^22) changed samples is one undo entry; a larger import
/// applies with no undo entry and clears the undo and redo history (its terrain entries would
/// write pre-import heights back, ADR-5575). The map is uploaded to the GPU and marked unsaved
/// like a stroke.
///
/// # Errors
///
/// No terrain, a missing, oversize or undecodable file, or a size mismatch.
pub fn import_heightmap(
    world: &mut World,
    entity: Entity,
    path: &Path,
    format: HeightmapFormat,
    size: Option<(u32, u32)>,
) -> Result<HeightmapImport, String> {
    let _span = tracing::info_span!(target: "regia_editor", "terrain_panel.import_heightmap").entered();
    if world.get::<Terrain>(entity).is_none() {
        return Err(format!("entity {} has no terrain", entity.to_bits()));
    }
    let len = std::fs::metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    if len > MAX_HEIGHTMAP_FILE {
        return Err(format!(
            "{}: {len} bytes is over the {MAX_HEIGHTMAP_FILE} byte limit",
            path.display()
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let hf = match format {
        HeightmapFormat::Png => decode_png16(&bytes)?,
        HeightmapFormat::R16(endian) => {
            let (w, h) = match size {
                Some(s) => s,
                None => {
                    let side = regia_terrain::square_side(bytes.len()).ok_or_else(|| {
                        format!("{} bytes is not a square 16-bit grid; give the size", bytes.len())
                    })?;
                    (side, side)
                }
            };
            decode_r16(&bytes, w, h, endian).map_err(|e| e.to_string())?
        }
    };
    if !world.entity(entity).contains::<TerrainEdits>() {
        world.entity_mut(entity).insert(TerrainEdits::default());
    }
    let mut query = world.query::<(&mut Terrain, &mut TerrainEdits)>();
    let (mut terrain, mut edits) = query
        .get_mut(world, entity)
        .map_err(|_| format!("entity {} has no terrain", entity.to_bits()))?;
    let import = import_heights(&mut terrain, &mut edits, &hf).map_err(|e| e.to_string())?;
    let undoable = import.undo.is_some();
    if let Some(undo) = import.undo
        && import.changed > 0
    {
        let (undo, layer) =
            regia_terrain::absorb_world_stroke(world, entity, Some(None), undo).map_err(|e| e.to_string())?;
        let command = match layer {
            Some(layer) => TerrainStrokeCommand::with_layer(entity, undo, layer),
            None => TerrainStrokeCommand::new(entity, undo),
        };
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .push(Box::new(command));
    } else if import.changed > 0 {
        regia_terrain::edit_layers::reconcile_world_base(world, entity).map_err(|e| e.to_string())?;
        // Too large to undo (ADR-5575): the history before it is history of other heights. A
        // terrain stroke entry restores absolute samples, so undoing or redoing one would stamp
        // pre-import heights into the imported map. Drop both stacks; the level stays unsaved
        // until a save, since no undo can return to the saved state.
        let mut fresh = UndoStack::new();
        fresh.mark_dirty();
        world.insert_resource(fresh);
    }
    Ok(HeightmapImport {
        changed: import.changed,
        undoable,
    })
}

/// The level whose `<map>.grassdensity.bin` was brought in, and on which terrain; `refused` when the
/// file exists and cannot be read (density edits are then off, so the file is never replaced).
#[derive(Resource, Debug, Default, Clone, PartialEq, Eq)]
pub struct GrassDensityLoaded {
    /// The level path and the terrain the file was loaded onto.
    pub key: Option<(PathBuf, Entity)>,
    /// The file exists and cannot be read.
    pub refused: bool,
}

/// One grass density stroke (or a whole viewport drag) on the undo stack.
pub struct GrassDensityCommand {
    entity: Entity,
    /// The pieces in the order they were painted.
    pieces: Vec<GrassDensityUndo>,
}

impl GrassDensityCommand {
    /// A command for pieces already applied to `entity`'s [`GrassDensityPaint`].
    #[must_use]
    pub fn new(entity: Entity, pieces: Vec<GrassDensityUndo>) -> Self {
        Self { entity, pieces }
    }

    fn restore(&self, world: &mut World, undo: bool) {
        let Some(target) = crate::command::resolve_entity(world, self.entity.to_bits()) else {
            tracing::warn!(target: "regia_editor", "grass density undo: entity {} is gone", self.entity.to_bits());
            return;
        };
        let Some(mut paint) = world.get_mut::<GrassDensityPaint>(target) else {
            return;
        };
        if undo {
            for piece in self.pieces.iter().rev() {
                piece.undo(&mut paint);
            }
        } else {
            for piece in &self.pieces {
                piece.redo(&mut paint);
            }
        }
    }
}

impl EditorCommand for GrassDensityCommand {
    fn apply(&mut self, world: &mut World) {
        self.restore(world, false);
    }

    fn revert(&mut self, world: &mut World) {
        self.restore(world, true);
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        "Paint grass density".into()
    }
}

/// Applies one density stroke piece to `entity` (creating a blank 1 m image over its map the
/// first time); the image marks itself unsaved. Returns the piece's record (empty when nothing
/// changed); the caller puts it on the undo stack ([`grass_density_stroke`] does).
///
/// # Errors
///
/// No terrain on `entity`, a bad stroke, or the level's density file exists and cannot be read
/// (edits are refused so a save never replaces it).
pub fn paint_grass_density_piece(
    world: &mut World,
    entity: Entity,
    stroke: &GrassDensityStroke,
) -> Result<GrassDensityUndo, String> {
    if world
        .get_resource::<GrassDensityLoaded>()
        .is_some_and(|l| l.refused)
    {
        return Err("the level's grass density file cannot be read; density edits are off".into());
    }
    let terrain = world
        .get::<Terrain>(entity)
        .ok_or_else(|| format!("entity {} has no terrain", entity.to_bits()))?;
    let world_width = terrain.map.world_width();
    if !world.entity(entity).contains::<GrassDensityPaint>() {
        let blank = GrassDensityPaint::for_map(&terrain.map, 1.0).map_err(|e| e.to_string())?;
        world.entity_mut(entity).insert(blank);
    }
    let mut paint = world
        .get_mut::<GrassDensityPaint>(entity)
        .ok_or_else(|| "no grass density image".to_string())?;
    paint.stroke(stroke, world_width).map_err(|e| e.to_string())
}

/// One grass density stroke as one undo entry (see [`paint_grass_density_piece`]).
///
/// # Errors
///
/// As [`paint_grass_density_piece`].
pub fn grass_density_stroke(
    world: &mut World,
    entity: Entity,
    stroke: &GrassDensityStroke,
) -> Result<GrassDensityUndo, String> {
    let undo = paint_grass_density_piece(world, entity, stroke)?;
    if undo.changed > 0 {
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .push(Box::new(GrassDensityCommand::new(entity, vec![undo.clone()])));
    }
    Ok(undo)
}

/// The open level's path relative to the project's `assets/` (`None` without a project or
/// outside it).
fn open_level_rel(world: &World, level: &Path) -> Option<String> {
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())?;
    level_rel(&root, level)
}

/// The map path a level's side files (`.holes.ron`, `.weights.bin`, `.scatterpaint.bin`,
/// `.grassdensity.bin`) are named after: the map the level's terrain names (scene v12), else the
/// level path itself (the pre-v12 rule, and a level whose terrain leaves `map` empty, so a Save As
/// moves the files with the level). Only a level terrain built for this level counts, so a
/// terrain left over from another level never redirects this level's files.
#[must_use]
pub fn side_file_base(world: &World, level: &Path) -> PathBuf {
    let Some(rel) = open_level_rel(world, level) else {
        return level.to_path_buf();
    };
    let mut named = None;
    for entity in world.iter_entities() {
        if let Some(source) = entity.get::<LevelTerrainSource>()
            && source.level == rel
            && source.terrain.as_ref().is_some_and(|t| !t.map.is_empty())
        {
            named = Some(source.files.map.clone());
        }
    }
    named.unwrap_or_else(|| level.to_path_buf())
}

/// Writes the grass density edited since the last save to `<map>.grassdensity.bin` (atomically):
/// every editor and agent edit marks the image unsaved. Without an edit nothing is touched, nor
/// when the level's file could not be read. Returns the file written.
///
/// # Errors
///
/// Two terrains have unsaved density (a level holds one file), or the write failed.
pub fn save_grass_density(world: &mut World, level: &Path) -> Result<Option<PathBuf>, String> {
    save_grass_density_writing(world, level, true).map(|written| written.map(|(file, _)| file))
}

/// Writes the grass density file. `commit` marks that image saved. The orchestrated level save
/// passes `false` and marks it from `commit_terrain_save` after the open path is stored.
pub(crate) fn save_grass_density_writing(
    world: &mut World,
    level: &Path,
    commit: bool,
) -> Result<Option<(PathBuf, Entity)>, String> {
    if world
        .get_resource::<GrassDensityLoaded>()
        .is_some_and(|l| l.refused)
    {
        tracing::warn!(target: "regia_editor", "the level's grass density file cannot be read; it is left as it is");
        return Ok(None);
    }
    let unsaved: Vec<Entity> = world
        .query::<(Entity, &GrassDensityPaint)>()
        .iter(world)
        .filter(|(_, p)| p.is_unsaved())
        .map(|(e, _)| e)
        .collect();
    if unsaved.len() > 1 {
        return Err(format!(
            "{} terrains have unsaved grass density and a level holds one density file; undo all but one",
            unsaved.len()
        ));
    }
    let Some(&entity) = unsaved.first() else {
        return Ok(None);
    };
    let bytes = world
        .get::<GrassDensityPaint>(entity)
        .ok_or("no grass density image")?
        .to_bytes()
        .map_err(|e| e.to_string())?;
    let file = grass_density_path(&side_file_base(world, level));
    crate::level_io::write_atomic(&file, &bytes)?;
    if commit && let Some(mut paint) = world.get_mut::<GrassDensityPaint>(entity) {
        paint.mark_saved();
    }
    Ok(Some((file, entity)))
}

/// Brings the open level's `<map>.grassdensity.bin` onto its terrain, once per level and terrain
/// (a frame effect): the level terrain the level names, else the host's terrain with the lowest
/// id. A level without a file starts with no painted density (a host terrain kept across levels
/// does not carry the last level's). A file that exists and cannot be read refuses density edits.
pub fn load_level_grass_density(world: &mut World) {
    let Some(level) = world
        .get_resource::<crate::level_io::OpenLevel>()
        .and_then(|open| open.path.clone())
    else {
        return;
    };
    let rel = open_level_rel(world, &level);
    let own = world
        .query::<(Entity, &LevelTerrainSource)>()
        .iter(world)
        .find(|(_, s)| Some(&s.level) == rel.as_ref())
        .map(|(e, _)| e);
    let target = own.or_else(|| {
        world
            .query_filtered::<Entity, With<Terrain>>()
            .iter(world)
            .min_by_key(|e| e.to_bits())
    });
    let Some(entity) = target else {
        return;
    };
    let key = Some((level.clone(), entity));
    if world
        .get_resource::<GrassDensityLoaded>()
        .is_some_and(|l| l.key == key)
    {
        return;
    }
    // A level terrain read its own file when it was built.
    if own == Some(entity) {
        world.insert_resource(GrassDensityLoaded { key, refused: false });
        world.insert_resource(GrassDensityFileRefused { refused: false });
        return;
    }
    let file = grass_density_path(&side_file_base(world, &level));
    let refused = match std::fs::read(&file) {
        Ok(bytes) => match GrassDensityPaint::from_bytes(&bytes) {
            Ok(paint) => {
                world.entity_mut(entity).insert(paint);
                false
            }
            Err(err) => {
                tracing::warn!(target: "regia_editor", "{}: {err}; grass density edits are off", file.display());
                true
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            world.entity_mut(entity).remove::<GrassDensityPaint>();
            false
        }
        Err(err) => {
            tracing::warn!(target: "regia_editor", "{}: {err}; grass density edits are off", file.display());
            true
        }
    };
    world.insert_resource(GrassDensityLoaded { key, refused });
    world.insert_resource(GrassDensityFileRefused { refused });
}

/// The level's path relative to `assets/`, `/`-separated (`None` outside the project's assets).
///
/// The strip compares components case-folded on Windows: the filesystem opens a level whose
/// spelling differs from the project root, so the relative path must be found the same way
/// (audit G finding 1, ADR-4824).
fn level_rel(root: &Path, level: &Path) -> Option<String> {
    let rel = crate::path_norm::strip_prefix_normalized(&root.join("assets"), level)?;
    let parts: Vec<&str> = rel.iter().map(|p| p.to_str()).collect::<Option<_>>()?;
    Some(parts.join("/"))
}

/// The roots the editor reads a level's terrain from: the project's `assets/` and its cooked
/// cache for this platform.
#[must_use]
pub fn level_terrain_roots(root: &Path, level: &Path) -> Option<LevelTerrainRoots> {
    Some(LevelTerrainRoots {
        assets: root.join("assets"),
        cooked: regia_asset::cooked_cache_dir(root, &regia_asset::current_platform()),
        level: level_rel(root, level)?,
    })
}

/// The last level-terrain load of the editor, for the panel and `terrain.level_terrain`.
#[derive(Resource, Debug, Default, Clone, PartialEq)]
pub struct EditorLevelTerrain {
    /// The level the load was for.
    pub level: Option<PathBuf>,
    /// The terrain block the load was for (a new block for the same level loads again).
    pub terrain: Option<regia_scene::level::LevelTerrain>,
    /// The terrain entity built (`None`: the level names no heightfield).
    pub entity: Option<Entity>,
    /// The error of a failed load (code, message). It stays until the block changes or the
    /// panel's Retry removes this resource.
    pub error: Option<(String, String)>,
    /// [`regia_terrain::LevelTerrainLoad::digest`] of the last good load.
    pub digest: Option<u64>,
}

/// Builds the terrain the open level names, once per level path and terrain block (a frame
/// effect; a failed load is not retried until the block changes or the panel's Retry). Opening a
/// level drops the terrain built for another level. The new entity gets a [`TerrainCookTarget`]
/// at the layer file's map id, so a save re-cooks into the cooked heightfield it was loaded from.
/// A changed block does not replace a terrain with unsaved sculpt or paint (save first). Failures
/// are kept in [`EditorLevelTerrain`] and logged; the host's terrain is never removed for them.
pub fn load_open_level_terrain(world: &mut World) {
    let Some(level) = world
        .get_resource::<crate::level_io::OpenLevel>()
        .and_then(|open| open.path.clone())
    else {
        return;
    };
    let Some(root) = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
    else {
        return;
    };
    let block = world
        .get_resource::<regia_scene::level::LevelSettings>()
        .and_then(|s| s.terrain.clone());
    let done = world
        .get_resource::<EditorLevelTerrain>()
        .is_some_and(|state| state.level.as_ref() == Some(&level) && state.terrain == block);
    if done {
        return;
    }
    let fail = |level: PathBuf, block: Option<regia_scene::level::LevelTerrain>, code: &str, msg: String| {
        EditorLevelTerrain {
            level: Some(level),
            terrain: block,
            entity: None,
            error: Some((code.to_owned(), msg)),
            digest: None,
        }
    };
    let Some(roots) = level_terrain_roots(&root, &level) else {
        let state = fail(
            level,
            block,
            "level_terrain_invalid",
            "the level is not under the project's assets/".into(),
        );
        world.insert_resource(state);
        return;
    };
    // Another level's terrain is not this level's ground.
    regia_terrain::despawn_other_levels(world, &roots.level);
    let Some(terrain) = block.clone() else {
        world.insert_resource(EditorLevelTerrain {
            level: Some(level),
            ..EditorLevelTerrain::default()
        });
        return;
    };
    let unsaved = world
        .query::<(&LevelTerrainSource, &TerrainEdits)>()
        .iter(world)
        .any(|(_, e)| !e.unsaved_tiles().is_empty() || e.unsaved_paint());
    if unsaved {
        let state = fail(
            level,
            block,
            "level_terrain_unsaved",
            "the level terrain has unsaved edits; save or undo them, then Retry".into(),
        );
        world.insert_resource(state);
        return;
    }
    let settings = regia_scene::level::LevelSettings {
        terrain: Some(terrain),
        ..regia_scene::level::LevelSettings::default()
    };
    let state = match regia_terrain::spawn_level_terrain(world, &roots, &settings) {
        Ok(spawn) => {
            if let Some(entity) = spawn.entity
                && let Some(map_id) = world.get::<LevelTerrainSource>(entity).and_then(|s| s.map_id)
            {
                world.entity_mut(entity).insert(TerrainCookTarget {
                    map_id,
                    cache_dir: roots.cooked.clone(),
                });
            }
            EditorLevelTerrain {
                level: Some(level),
                terrain: block,
                entity: spawn.entity,
                error: None,
                digest: Some(spawn.digest),
            }
        }
        Err(err) => {
            tracing::warn!(target: "regia_editor", "level terrain: {err}");
            fail(level, block, err.code(), err.to_string())
        }
    };
    world.insert_resource(state);
}

/// The `.scatter.ron` rule editor behind the panel's Scatter rules section.
#[derive(Resource, Debug, Clone, Default)]
pub struct ScatterRuleEditor {
    /// The file open.
    pub path: Option<PathBuf>,
    /// The rules as edited.
    pub set: ScatterSet,
    /// The class shown.
    pub selected: usize,
    /// Edited since the last open or save.
    pub dirty: bool,
}

impl ScatterRuleEditor {
    /// Opens `path`.
    ///
    /// # Errors
    ///
    /// The file cannot be read or is not a valid scatter set.
    pub fn open(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let set = ScatterSet::from_ron(&text, None).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self {
            path: Some(path.to_path_buf()),
            set,
            selected: 0,
            dirty: false,
        })
    }

    fn class_index(&self, class: &str) -> Result<usize, String> {
        self.set
            .classes
            .iter()
            .position(|c| c.name == class)
            .ok_or_else(|| format!("no class `{class}` in the set"))
    }

    /// The editable fields of `class` and their values (every serialized field but `name` and
    /// `meshes`, which have their own edits).
    ///
    /// # Errors
    ///
    /// No such class.
    pub fn fields(&self, class: &str) -> Result<serde_json::Map<String, Value>, String> {
        let i = self.class_index(class)?;
        let mut map = match serde_json::to_value(&self.set.classes[i]).map_err(|e| e.to_string())? {
            Value::Object(map) => map,
            _ => return Err("a class is not a struct".into()),
        };
        map.remove("name");
        map.remove("meshes");
        Ok(map)
    }

    /// Sets field `field` of `class` to `value`. The value must have the field's JSON type, and
    /// the edited set must still validate (else nothing changes).
    ///
    /// # Errors
    ///
    /// No such class or field, a wrong type, or a set that no longer validates.
    pub fn set_field(&mut self, class: &str, field: &str, value: Value) -> Result<(), String> {
        let i = self.class_index(class)?;
        if field == "name" || field == "meshes" {
            return Err(format!("`{field}` is not a rule field"));
        }
        let mut json = serde_json::to_value(&self.set.classes[i]).map_err(|e| e.to_string())?;
        let slot = json
            .get_mut(field)
            .ok_or_else(|| format!("class `{class}` has no field `{field}`"))?;
        let same_kind = matches!(
            (&*slot, &value),
            (Value::Number(_), Value::Number(_))
                | (Value::Bool(_), Value::Bool(_))
                | (Value::String(_), Value::String(_))
                | (Value::Array(_), Value::Array(_))
                | (Value::Null | Value::String(_), Value::Null | Value::String(_))
                | (Value::Object(_), Value::Object(_))
        );
        if !same_kind {
            return Err(format!("`{field}` is {slot}; {value} is another kind"));
        }
        *slot = value;
        let edited: regia_terrain::ScatterClass =
            serde_json::from_value(json).map_err(|e| format!("`{field}`: {e}"))?;
        let mut set = self.set.clone();
        set.classes[i] = edited;
        set.validate(None).map_err(|e| e.to_string())?;
        self.set = set;
        self.dirty = true;
        Ok(())
    }

    /// Adds a class named `name`, a copy of the selected class (or the default class) with that
    /// name.
    ///
    /// # Errors
    ///
    /// The name is taken or empty, or the set no longer validates.
    pub fn add_class(&mut self, name: &str) -> Result<(), String> {
        if name.is_empty() || self.set.classes.iter().any(|c| c.name == name) {
            return Err(format!("class name `{name}` is empty or taken"));
        }
        let mut class = self.set.classes.get(self.selected).cloned().unwrap_or_default();
        class.name = name.to_owned();
        let mut set = self.set.clone();
        set.classes.push(class);
        set.validate(None).map_err(|e| e.to_string())?;
        self.set = set;
        self.selected = self.set.classes.len() - 1;
        self.dirty = true;
        Ok(())
    }

    /// Removes the class named `name`.
    ///
    /// # Errors
    ///
    /// No such class.
    pub fn remove_class(&mut self, name: &str) -> Result<(), String> {
        let i = self.class_index(name)?;
        self.set.classes.remove(i);
        self.selected = self.selected.min(self.set.classes.len().saturating_sub(1));
        self.dirty = true;
        Ok(())
    }

    /// The canonical RON of the set (`ScatterSet::to_ron`).
    ///
    /// # Errors
    ///
    /// The set does not validate.
    pub fn to_ron(&self) -> Result<String, String> {
        self.set.to_ron().map_err(|e| e.to_string())
    }

    /// Saves canonical RON to the open path (atomically) and returns it.
    ///
    /// # Errors
    ///
    /// No path, a set that does not validate, or a write failure.
    pub fn save(&mut self) -> Result<PathBuf, String> {
        let path = self.path.clone().ok_or("no .scatter.ron open")?;
        let text = self.to_ron()?;
        crate::level_io::write_atomic(&path, text.as_bytes())?;
        self.dirty = false;
        Ok(path)
    }
}

/// The panel's own state (text fields and the last message).
#[derive(Resource, Debug, Clone)]
pub struct TerrainPanelState {
    /// Heightmap file, project-relative.
    pub heightmap_path: String,
    /// RAW endianness.
    pub big_endian: bool,
    /// `.scatter.ron` file, project-relative.
    pub scatter_path: String,
    /// Name for a new scatter class.
    pub new_class: String,
    /// The last result or error.
    pub message: String,
}

impl Default for TerrainPanelState {
    fn default() -> Self {
        Self {
            heightmap_path: "assets/maps/heights.png".into(),
            big_endian: false,
            scatter_path: "assets/maps/terrain.scatter.ron".into(),
            new_class: String::new(),
            message: String::new(),
        }
    }
}

/// The terrain a panel edit targets: the selection when it is a terrain, else the lowest id.
pub(crate) fn panel_terrain(world: &mut World) -> Option<Entity> {
    let selected = world
        .get_resource::<crate::selection::Selection>()
        .and_then(crate::selection::Selection::primary)
        .filter(|e| world.get::<Terrain>(*e).is_some());
    selected.or_else(|| {
        world
            .query_filtered::<Entity, With<Terrain>>()
            .iter(world)
            .min_by_key(|e| e.to_bits())
    })
}

/// Resolves a project-relative path typed into the panel (no `..`, no root).
fn project_path(world: &World, rel: &str) -> Result<PathBuf, String> {
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or("open a project first")?;
    let p = Path::new(rel);
    if p.is_absolute()
        || p.components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(format!("`{rel}` must be relative to the project without `..`"));
    }
    Ok(root.join(p))
}

/// The Terrain panel.
pub fn terrain_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut state = world
        .get_resource::<TerrainPanelState>()
        .cloned()
        .unwrap_or_default();
    let entity = panel_terrain(world);
    ui.heading("Level terrain");
    match world.get_resource::<EditorLevelTerrain>().cloned() {
        Some(EditorLevelTerrain {
            error: Some((code, msg)),
            ..
        }) => {
            ui.colored_label(egui::Color32::LIGHT_RED, format!("{code}: {msg}"));
            if crate::agent_parity::action_button(ui, &crate::agent_parity::TERRAIN_RETRY).clicked() {
                world.remove_resource::<EditorLevelTerrain>();
            }
        }
        Some(EditorLevelTerrain {
            entity: Some(e),
            digest,
            ..
        }) => {
            ui.label(format!(
                "Built from the level: entity {} (digest {:016x})",
                e.to_bits(),
                digest.unwrap_or(0)
            ));
        }
        _ => {
            ui.label(
                "The level names no terrain (the host supplies it). New terrain… creates one and names \
                 it in the level.",
            );
        }
    }
    if ui.button("New terrain…").clicked() {
        crate::terrain_create::open_dialog(world, None);
    }
    crate::terrain_create::dialog_window(ui.ctx(), world);
    ui.separator();
    brush_controls(ui, world);
    crate::terrain_region::panel(ui, world);
    ui.separator();
    ui.heading("Heightmap");
    ui.horizontal(|ui| {
        ui.label("File");
        ui.text_edit_singleline(&mut state.heightmap_path);
    });
    ui.checkbox(&mut state.big_endian, "RAW big-endian");
    ui.label("16-bit: sample = round((height − offset) · 65535 / scale); PNG rows start at −Z.");
    ui.horizontal(|ui| {
        let export = ui.button("Export").clicked();
        let import = ui.button("Import").clicked();
        if !(export || import) {
            return;
        }
        let Some(entity) = entity else {
            state.message = "no terrain in the level".into();
            return;
        };
        let result = project_path(world, &state.heightmap_path).and_then(|path| {
            let format = match HeightmapFormat::of_path(&path) {
                Some(HeightmapFormat::R16(_)) if state.big_endian => HeightmapFormat::R16(RawEndian::Big),
                Some(f) => f,
                None => return Err("use a .png, .r16 or .raw file".into()),
            };
            if export {
                export_heightmap(world, entity, &path, format)
                    .map(|e| format!("exported {}x{} ({} bytes)", e.width, e.height, e.bytes))
            } else {
                import_heightmap(world, entity, &path, format, None)
                    .map(|i| format!("imported: {} samples changed", i.changed))
            }
        });
        state.message = result.unwrap_or_else(|e| e);
    });
    ui.separator();
    // Count sources with datum, units and registration: preview, then apply (ADR-8541).
    crate::terrain_source_import::panel_ui(ui, world);
    ui.separator();
    // The source's registered overview, baked as a cancellable job (ADR-8596).
    crate::terrain_overview_job::panel_ui(ui, world);
    ui.separator();
    crate::terrain_layers::layers_ui(ui, world, entity);
    ui.separator();
    ui.heading("Grass density");
    ui.label("Terrain mode 0 paints it in the viewport: - = target, [ ] size, Shift restores.");
    if let Some(tool) = world.get_resource::<crate::terrain_tool::TerrainTool>() {
        ui.label(format!("Target {:.2} of the baked density", tool.density_target));
    }
    if world
        .get_resource::<GrassDensityLoaded>()
        .is_some_and(|l| l.refused)
    {
        ui.colored_label(
            egui::Color32::LIGHT_RED,
            "The level's grass density file cannot be read; density edits are off.",
        );
    }
    if let Some(paint) = entity.and_then(|e| world.get::<GrassDensityPaint>(e)) {
        ui.label(format!(
            "{} × {} texels, {} painted",
            paint.side(),
            paint.side(),
            paint.painted_texels()
        ));
    }
    ui.separator();
    crate::scatter_brush::picker_ui(ui, world);
    ui.separator();
    ui.heading("Scatter rules");
    ui.horizontal(|ui| {
        ui.label("File");
        ui.text_edit_singleline(&mut state.scatter_path);
        if ui.button("Open").clicked() {
            match project_path(world, &state.scatter_path).and_then(|p| ScatterRuleEditor::open(&p)) {
                Ok(editor) => {
                    world.insert_resource(editor);
                    state.message = "opened".into();
                }
                Err(e) => state.message = e,
            }
        }
    });
    if let Some(mut editor) = world.get_resource::<ScatterRuleEditor>().cloned() {
        scatter_rules_ui(ui, &mut editor, &mut state);
        world.insert_resource(editor);
    }
    if !state.message.is_empty() {
        ui.separator();
        ui.label(&state.message);
    }
    world.insert_resource(state);
}

/// Keyboard-focusable brush controls backed by the same values as the viewport hotkeys.
pub fn brush_controls(ui: &mut egui::Ui, world: &mut World) {
    use regia_terrain::brush::BrushFalloff;
    use regia_terrain::{BrushMode, BrushShape};
    let mut tool = world.get_resource_or_insert_with(crate::terrain_tool::TerrainTool::default);
    ui.heading("Brush");
    match &mut tool.brush.shape {
        BrushShape::Circle { radius } => {
            ui.add(
                egui::Slider::new(radius, 0.01..=4096.0)
                    .logarithmic(true)
                    .clamping(egui::SliderClamping::Never)
                    .text("Size"),
            );
        }
        BrushShape::Rectangle { width, length } => {
            ui.add(
                egui::Slider::new(width, 0.01..=4096.0)
                    .logarithmic(true)
                    .clamping(egui::SliderClamping::Never)
                    .text("Size"),
            );
            ui.add(
                egui::Slider::new(length, 0.01..=4096.0)
                    .logarithmic(true)
                    .clamping(egui::SliderClamping::Never)
                    .text("Length"),
            );
        }
    }
    if tool.paint_density {
        ui.add(egui::Slider::new(&mut tool.density_target, 0.0..=1.0).text("Strength"))
            .on_hover_text("Target grass density; the minus and equals shortcuts adjust this value.");
    } else if tool.paint_scatter {
        ui.add(
            egui::Slider::new(
                &mut tool.scatter_density,
                0.001..=regia_terrain::MAX_PAINT_DENSITY,
            )
            .logarithmic(true)
            .text("Strength"),
        )
        .on_hover_text("Scatter instances per square meter.");
    } else {
        match &mut tool.brush.mode {
            BrushMode::Raise { amount } | BrushMode::Lower { amount } | BrushMode::Noise { amount, .. } => {
                ui.add(
                    egui::Slider::new(amount, 0.01..=1000.0)
                        .logarithmic(true)
                        .text("Strength"),
                );
            }
            _ => {
                ui.add(egui::Slider::new(&mut tool.brush.alpha, 0.0..=1.0).text("Strength"));
            }
        }
    }
    ui.add(egui::Slider::new(&mut tool.brush.hardness, 0.0..=1.0).text("Falloff"))
        .on_hover_text("Full-strength inner radius: 0 fades from the center, 1 has a hard edge.");
    let curves = [
        BrushFalloff::Smooth,
        BrushFalloff::Linear,
        BrushFalloff::Sphere,
        BrushFalloff::Tip,
    ];
    let mut curve = curves.iter().position(|&v| v == tool.brush.falloff).unwrap_or(0);
    let curve_enabled = !tool.paint_scatter && !tool.paint_density && !tool.cut_holes;
    if ui
        .add_enabled(
            curve_enabled,
            egui::Slider::new(&mut curve, 0..=3)
                .text("Falloff curve")
                .custom_formatter(|v, _| {
                    ["Smooth", "Linear", "Sphere", "Tip"][v.round().clamp(0.0, 3.0) as usize].into()
                }),
        )
        .changed()
    {
        tool.brush.falloff = curves[curve];
    }
}

fn scatter_rules_ui(ui: &mut egui::Ui, editor: &mut ScatterRuleEditor, state: &mut TerrainPanelState) {
    let names: Vec<String> = editor.set.classes.iter().map(|c| c.name.clone()).collect();
    ui.horizontal_wrapped(|ui| {
        for (i, name) in names.iter().enumerate() {
            if ui.selectable_label(editor.selected == i, name).clicked() {
                editor.selected = i;
            }
        }
    });
    ui.horizontal(|ui| {
        ui.text_edit_singleline(&mut state.new_class);
        if ui.button("Add class").clicked() {
            let name = state.new_class.clone();
            state.message = editor
                .add_class(&name)
                .map_or_else(|e| e, |()| format!("added {name}"));
        }
        if let Some(name) = names.get(editor.selected).cloned()
            && ui.button("Remove").clicked()
        {
            state.message = editor
                .remove_class(&name)
                .map_or_else(|e| e, |()| format!("removed {name}"));
        }
    });
    let Some(name) = names.get(editor.selected).cloned() else {
        return;
    };
    let Ok(fields) = editor.fields(&name) else {
        return;
    };
    egui::Grid::new("scatter_rule_fields")
        .num_columns(2)
        .show(ui, |ui| {
            for (field, value) in fields {
                ui.label(&field);
                let edited = match value {
                    Value::Number(n) => {
                        let mut v = n.as_f64().unwrap_or(0.0);
                        ui.add(egui::DragValue::new(&mut v).speed(0.01))
                            .changed()
                            .then(|| serde_json::Number::from_f64(v).map(Value::Number))
                            .flatten()
                    }
                    Value::Bool(b) => {
                        let mut v = b;
                        ui.checkbox(&mut v, "").changed().then_some(Value::Bool(v))
                    }
                    other => {
                        ui.label(other.to_string());
                        None
                    }
                };
                if let Some(v) = edited
                    && let Err(e) = editor.set_field(&name, &field, v)
                {
                    state.message = e;
                }
                ui.end_row();
            }
        });
    let label = if editor.dirty { "Save *" } else { "Save" };
    if ui.button(label).clicked() {
        state.message = editor
            .save()
            .map_or_else(|e| e, |p| format!("saved {}", p.display()));
    }
}

/// `editor.scatter_class { path, add?, remove? }`: the Scatter rules section's Add class and Remove
/// (ADR-7307). `add` copies the first class (or the default) under the new name; `remove` drops a
/// class; the file is saved as canonical RON. Field edits are `terrain.set_scatter_rule`.
fn editor_scatter_class(world: &mut World, params: &Value) -> Result<Value, regia_agent::protocol::RpcError> {
    use regia_agent::protocol::{INVALID_PARAMS, RpcError};
    let bad = |code: &str, message: String| {
        RpcError::new(
            INVALID_PARAMS,
            code,
            message,
            "editor.scatter_class takes path (a project-relative .scatter.ron) and add or remove (a class name)",
        )
    };
    let rel = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("scatter_class_bad_params", "`path` must be a string".into()))?;
    let add = params.get("add").and_then(Value::as_str);
    let remove = params.get("remove").and_then(Value::as_str);
    if add.is_none() && remove.is_none() {
        return Err(bad("scatter_class_bad_params", "give `add` or `remove`".into()));
    }
    let path = project_path(world, rel).map_err(|e| bad("scatter_class_bad_path", e))?;
    let mut editor = ScatterRuleEditor::open(&path).map_err(|e| bad("scatter_class_unreadable", e))?;
    if let Some(name) = remove {
        editor
            .remove_class(name)
            .map_err(|e| bad("scatter_class_unknown", e))?;
    }
    if let Some(name) = add {
        editor.selected = 0;
        editor
            .add_class(name)
            .map_err(|e| bad("scatter_class_refused", e))?;
    }
    editor.save().map_err(|e| bad("scatter_class_refused", e))?;
    // A panel that has this file open shows the saved set.
    if world
        .get_resource::<ScatterRuleEditor>()
        .is_some_and(|open| open.path.as_deref() == Some(path.as_path()))
    {
        world.insert_resource(editor.clone());
    }
    let classes: Vec<&str> = editor.set.classes.iter().map(|c| c.name.as_str()).collect();
    Ok(serde_json::json!({ "path": rel, "classes": classes }))
}

/// An editor agent method's handler.
type Handler = fn(&mut World, &Value) -> Result<Value, regia_agent::protocol::RpcError>;

/// The Terrain panel's editor methods (`editor.scatter_class`, and New terrain's
/// `editor.terrain_create`, ADR-7727).
#[must_use]
pub fn methods() -> Vec<(regia_agent::schema::MethodSchema, Handler)> {
    use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
    let mut methods = vec![(
        MethodSchema {
            name: "editor.scatter_class",
            tool: "scatter_class",
            description: "Add or remove a class of a .scatter.ron as the Terrain panel's Scatter rules section \
                does (ADR-7307): `add` copies the first class under the new name, `remove` drops one; the file \
                is saved as canonical RON. Field edits are terrain.set_scatter_rule. Errors \
                scatter_class_bad_params, scatter_class_bad_path, scatter_class_unreadable, \
                scatter_class_unknown, scatter_class_refused.",
            params: &[
                ParamSchema {
                    name: "path",
                    kind: "string",
                    required: true,
                    description: "Project-relative .scatter.ron.",
                },
                ParamSchema {
                    name: "add",
                    kind: "string",
                    required: false,
                    description: "New class name (a copy of the first class).",
                },
                ParamSchema {
                    name: "remove",
                    kind: "string",
                    required: false,
                    description: "Class name to remove (applied before `add`).",
                },
            ],
            result: "{ path, classes }",
            mutating: true,
            permission: Permission::Assets,
        },
        editor_scatter_class as Handler,
    )];
    methods.extend(crate::terrain_create::methods());
    methods.extend(crate::terrain_source_import::methods());
    methods.extend(crate::terrain_overview_job::methods());
    methods
}

#[cfg(test)]
mod tests {
    /// Audit G finding 1 (Windows-only scenario): a level opened with a different drive-letter
    /// or component casing than the project root must still resolve under `assets/` — the
    /// filesystem opens it, so the terrain must build. The folded compare itself is proven on
    /// every host in `path_norm`'s tests.
    #[cfg(windows)]
    #[test]
    fn level_rel_accepts_case_variant_root_on_windows() {
        let roots = level_terrain_roots(
            std::path::Path::new("C:/Dev/Game"),
            std::path::Path::new("c:/dev/game/assets/maps/arena.level.ron"),
        );
        let roots = roots.expect("a case-variant root still resolves");
        assert_eq!(roots.level, "maps/arena.level.ron");
    }
    use super::*;
    use regia_terrain::{CdlodConfig, TerrainLayers, TerrainMap};

    const SIDE: u32 = 65;

    fn terrain_with(f: impl Fn(u32, u32) -> f32) -> Terrain {
        let heights = (0..SIDE * SIDE).map(|i| f(i % SIDE, i / SIDE)).collect();
        let map = TerrainMap::from_heights(SIDE, SIDE, heights, 64.0, 50.0, -5.0).expect("map");
        Terrain::new(
            &map.to_heightfield(),
            64.0,
            50.0,
            -5.0,
            CdlodConfig::default(),
            2,
            TerrainLayers::default(),
        )
        .expect("terrain")
    }

    fn hilly() -> Terrain {
        terrain_with(|x, z| 10.0 + (x as f32 * 0.3).sin() * 6.0 + (z as f32 * 0.17).cos() * 4.0)
    }

    fn undo(world: &mut World) -> bool {
        world.resource_scope::<UndoStack, bool>(|w, mut s| s.undo(w))
    }

    fn redo(world: &mut World) -> bool {
        world.resource_scope::<UndoStack, bool>(|w, mut s| s.redo(w))
    }

    /// Acceptance (ADR-2411): heightmap export then import round-trips exactly, through a 16-bit
    /// PNG and RAW in both byte orders, into a terrain of other heights; the import is one undo.
    #[test]
    fn heightmap_export_then_import_round_trips_exactly() {
        let dir = regia_test_utils::unique_temp_dir("terrain_panel_heightmap");
        for (name, format) in [
            ("heights.png", HeightmapFormat::Png),
            ("heights.r16", HeightmapFormat::R16(RawEndian::Little)),
            ("heights_be.r16", HeightmapFormat::R16(RawEndian::Big)),
        ] {
            let mut world = World::new();
            let source = world.spawn(hilly()).id();
            let flat = world.spawn(terrain_with(|_, _| 2.0)).id();
            let original = world.get::<Terrain>(source).expect("t").map.to_heightfield();
            let before = world.get::<Terrain>(flat).expect("t").map.to_heightfield();
            let path = dir.join(name);
            let export = export_heightmap(&world, source, &path, format).expect("export");
            assert_eq!((export.width, export.height), (SIDE, SIDE));
            assert_eq!((export.clamped_low, export.clamped_high), (0, 0));
            assert_eq!(
                HeightmapFormat::of_path(&path).map(HeightmapFormat::name),
                Some(format.name())
            );
            let import = import_heightmap(&mut world, flat, &path, format, None).expect("import");
            assert!(import.changed > 0 && import.undoable);
            let after = world.get::<Terrain>(flat).expect("t").map.to_heightfield();
            assert_eq!(
                after.samples, original.samples,
                "{name}: export then import is the identity"
            );
            // The same file again changes nothing and adds no undo entry.
            let depth = world.resource::<UndoStack>().undo_depth();
            let again = import_heightmap(&mut world, flat, &path, format, None).expect("import");
            assert_eq!(again.changed, 0);
            assert_eq!(world.resource::<UndoStack>().undo_depth(), depth);
            assert!(undo(&mut world));
            let undone = world.get::<Terrain>(flat).expect("t").map.to_heightfield();
            assert_eq!(
                undone.samples, before.samples,
                "{name}: undo restores the heights"
            );
            assert!(redo(&mut world));
            let redone = world.get::<Terrain>(flat).expect("t").map.to_heightfield();
            assert_eq!(redone.samples, original.samples);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An import too large to undo must not leave older terrain entries on the stacks: they hold
    /// absolute samples of the pre-import map, so a redo (or undo) would stamp them into it.
    #[test]
    fn an_import_too_large_to_undo_clears_the_stale_history() {
        const BIG: u32 = 2049; // 2049^2 samples > MAX_IMPORT_UNDO_SAMPLES (2^22)
        assert!((BIG as usize * BIG as usize) > regia_terrain::MAX_IMPORT_UNDO_SAMPLES);
        let dir = regia_test_utils::unique_temp_dir("terrain_panel_big_import");
        std::fs::create_dir_all(&dir).expect("dir");
        let mut world = World::new();
        let map = TerrainMap::from_heights(BIG, BIG, vec![2.0; (BIG * BIG) as usize], 64.0, 50.0, -5.0)
            .expect("map");
        let terrain = Terrain::new(
            &map.to_heightfield(),
            64.0,
            50.0,
            -5.0,
            CdlodConfig::default(),
            2,
            TerrainLayers::default(),
        )
        .expect("terrain");
        let e = world.spawn(terrain).id();
        let path = dir.join("big.r16");
        let format = HeightmapFormat::R16(RawEndian::Little);
        export_heightmap(&world, e, &path, format).expect("export");
        let flat = std::fs::read(&path).expect("read");

        // A small undoable import, then undone: it waits on the redo stack.
        let mut small = flat.clone();
        small[0] ^= 0xFF;
        small[1] ^= 0x0F;
        regia_test_utils::write_atomic(&path, &small).expect("write");
        let first = import_heightmap(&mut world, e, &path, format, None).expect("small import");
        assert!(first.undoable && first.changed == 1);
        assert!(undo(&mut world));
        assert_eq!(world.resource::<UndoStack>().redo_depth(), 1);

        // Every sample changes: no undo entry, and the stale redo entry must go.
        let big: Vec<u8> = flat.iter().map(|b| b ^ 0x55).collect();
        regia_test_utils::write_atomic(&path, &big).expect("write");
        let second = import_heightmap(&mut world, e, &path, format, None).expect("big import");
        assert!(!second.undoable);
        assert!(second.changed as usize > regia_terrain::MAX_IMPORT_UNDO_SAMPLES);
        let imported = world.get::<Terrain>(e).expect("t").map.to_heightfield();
        let stack = world.resource::<UndoStack>();
        assert_eq!((stack.undo_depth(), stack.redo_depth()), (0, 0));
        assert!(stack.is_dirty(), "the import is unsaved");
        assert!(!redo(&mut world), "no stale stroke to redo");
        assert_eq!(
            world.get::<Terrain>(e).expect("t").map.to_heightfield().samples,
            imported.samples,
            "the imported heights stand"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn heightmap_import_refuses_a_size_mismatch_and_a_bad_png() {
        let dir = regia_test_utils::unique_temp_dir("terrain_panel_heightmap_bad");
        let mut world = World::new();
        let e = world.spawn(hilly()).id();
        let small = dir.join("small.r16");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(&small, vec![0u8; 2 * 9 * 9]).expect("write");
        let err = import_heightmap(
            &mut world,
            e,
            &small,
            HeightmapFormat::R16(RawEndian::Little),
            None,
        )
        .expect_err("9x9 into 65x65");
        assert!(err.contains("65") && err.contains('9'), "{err}");
        let junk = dir.join("junk.png");
        std::fs::write(&junk, b"not a png").expect("write");
        assert!(import_heightmap(&mut world, e, &junk, HeightmapFormat::Png, None).is_err());
        let odd = dir.join("odd.r16");
        std::fs::write(&odd, vec![0u8; 7]).expect("write");
        let err = import_heightmap(&mut world, e, &odd, HeightmapFormat::R16(RawEndian::Little), None)
            .expect_err("odd");
        assert!(err.contains("square"), "{err}");
        assert_eq!(HeightmapFormat::of_path(Path::new("a.exr")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn density_stroke(path: Vec<[f32; 2]>, target: f32) -> GrassDensityStroke {
        GrassDensityStroke {
            path,
            radius_m: 7.0,
            hardness: 0.4,
            strength: 0.8,
            target,
        }
    }

    /// Acceptance (ADR-2411): density strokes are deterministic and undoable. Two worlds given
    /// the same strokes hold the same bytes; undo walks back to the blank image and redo forward.
    #[test]
    fn density_strokes_are_deterministic_and_undoable() {
        let strokes = [
            density_stroke(vec![[-20.0, -5.0], [4.0, 9.0]], 0.0),
            density_stroke(vec![[0.0, 0.0]], 0.5),
            density_stroke(vec![[10.0, -12.0], [-3.0, -1.0], [12.0, 14.0]], 0.1),
        ];
        let run = || {
            let mut world = World::new();
            let e = world.spawn(hilly()).id();
            let mut digests = Vec::new();
            for s in &strokes {
                grass_density_stroke(&mut world, e, s).expect("stroke");
                digests.push(world.get::<GrassDensityPaint>(e).expect("paint").digest());
            }
            (world, e, digests)
        };
        let (mut a, ea, da) = run();
        let (_, _, db) = run();
        assert_eq!(da, db, "the same strokes give the same bytes");
        assert_eq!(a.resource::<UndoStack>().undo_depth(), 3);
        let blank = GrassDensityPaint::for_map(&a.get::<Terrain>(ea).expect("t").map, 1.0).expect("blank");
        assert!(undo(&mut a));
        assert_eq!(a.get::<GrassDensityPaint>(ea).expect("p").digest(), da[1]);
        assert!(undo(&mut a) && undo(&mut a));
        assert_eq!(
            a.get::<GrassDensityPaint>(ea).expect("p").texels(),
            blank.texels()
        );
        assert!(redo(&mut a) && redo(&mut a) && redo(&mut a));
        assert_eq!(a.get::<GrassDensityPaint>(ea).expect("p").digest(), da[2]);
        assert!(a.get::<GrassDensityPaint>(ea).expect("p").is_unsaved());
    }

    #[test]
    fn density_saves_after_an_edit_only_and_reloads() {
        let dir = regia_test_utils::unique_temp_dir("terrain_panel_density_save");
        std::fs::create_dir_all(&dir).expect("dir");
        let level = dir.join("arena.level.ron");
        let mut world = World::new();
        let e = world.spawn(hilly()).id();
        assert_eq!(save_grass_density(&mut world, &level).expect("save"), None);
        grass_density_stroke(&mut world, e, &density_stroke(vec![[1.0, 1.0]], 0.0)).expect("stroke");
        let file = save_grass_density(&mut world, &level)
            .expect("save")
            .expect("written");
        assert_eq!(file, dir.join("arena.grassdensity.bin"));
        let back = GrassDensityPaint::from_bytes(&std::fs::read(&file).expect("read")).expect("parse");
        assert_eq!(&back, world.get::<GrassDensityPaint>(e).expect("p"));
        assert_eq!(
            save_grass_density(&mut world, &level).expect("save"),
            None,
            "nothing new"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Terrain mode `0` drag in the viewport is one undo entry.
    #[test]
    fn a_density_drag_in_the_viewport_is_one_undo_entry() {
        use crate::terrain_tool::{TerrainTool, begin_stroke, continue_stroke, end_stroke};
        use regia_core::math::Vec2;
        let mut world = World::new();
        let e = world.spawn(hilly()).id();
        world.init_resource::<TerrainTool>();
        world.resource_mut::<TerrainTool>().select_mode(0);
        assert!(world.resource::<TerrainTool>().paint_density);
        assert!(world.resource::<TerrainTool>().status().contains("grass density"));
        assert!(begin_stroke(&mut world, e, Vec2::new(-10.0, 0.0), false));
        continue_stroke(&mut world, Vec2::new(0.0, 4.0));
        continue_stroke(&mut world, Vec2::new(10.0, 8.0));
        end_stroke(&mut world);
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        assert!(world.get::<GrassDensityPaint>(e).expect("p").painted_texels() > 0);
        assert!(undo(&mut world));
        assert_eq!(world.get::<GrassDensityPaint>(e).expect("p").painted_texels(), 0);
        world.resource_mut::<TerrainTool>().select_mode(1);
        assert!(!world.resource::<TerrainTool>().paint_density);
    }

    fn scatter_set() -> ScatterSet {
        let class = |name: &str| regia_terrain::ScatterClass {
            name: name.into(),
            meshes: vec![regia_terrain::ScatterMeshRef {
                mesh: "models/rock.gltf".into(),
                weight: 1.0,
                min_scale: 1.0,
                max_scale: 1.0,
                scale_bias: 1.0,
            }],
            ..regia_terrain::ScatterClass::default()
        };
        ScatterSet {
            classes: vec![class("rocks"), class("trees")],
            ..ScatterSet::default()
        }
    }

    /// `editor.scatter_class` adds and removes classes and saves (ADR-7307).
    #[test]
    fn scatter_class_adds_and_removes_through_the_agent() {
        let dir = regia_test_utils::unique_temp_dir("terrain_panel_scatter_class");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("t.scatter.ron"), scatter_set().to_ron().expect("ron")).expect("write");
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
        let (schema, handler) = methods().remove(0);
        assert_eq!(schema.name, "editor.scatter_class");
        let out = handler(
            &mut world,
            &serde_json::json!({ "path": "t.scatter.ron", "add": "bushes", "remove": "rocks" }),
        )
        .expect("edit");
        assert_eq!(out["classes"], serde_json::json!(["trees", "bushes"]), "{out}");
        let reopened = ScatterRuleEditor::open(&dir.join("t.scatter.ron")).expect("reopen");
        assert_eq!(reopened.set.classes.len(), 2);
        let mut code = |v: serde_json::Value| {
            handler(&mut world, &v)
                .expect_err("refused")
                .data
                .and_then(|d| d["code"].as_str().map(str::to_owned))
                .unwrap_or_default()
        };
        assert_eq!(
            code(serde_json::json!({ "path": "t.scatter.ron" })),
            "scatter_class_bad_params"
        );
        assert_eq!(
            code(serde_json::json!({ "path": "t.scatter.ron", "remove": "nope" })),
            "scatter_class_unknown"
        );
        assert_eq!(
            code(serde_json::json!({ "path": "t.scatter.ron", "add": "trees" })),
            "scatter_class_refused"
        );
        assert_eq!(
            code(serde_json::json!({ "path": "../x.scatter.ron", "add": "a" })),
            "scatter_class_bad_path"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Acceptance (ADR-2411): the scatter panel edits a rule headless and saves canonical RON.
    #[test]
    fn the_scatter_panel_edits_a_rule_headless_and_saves_canonical_ron() {
        let dir = regia_test_utils::unique_temp_dir("terrain_panel_scatter");
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("terrain.scatter.ron");
        // Written by hand with odd spacing: the save rewrites it canonically.
        let text = scatter_set().to_ron().expect("ron").replace("    ", "  ");
        std::fs::write(&path, &text).expect("write");
        let mut editor = ScatterRuleEditor::open(&path).expect("open");
        assert!(editor.fields("trees").expect("fields").contains_key("density"));
        editor
            .set_field("trees", "density", serde_json::json!(0.25))
            .expect("density");
        editor
            .set_field("trees", "avoid_holes", serde_json::json!(false))
            .expect("bool");
        assert!(
            editor
                .set_field("trees", "density", serde_json::json!("lots"))
                .is_err(),
            "wrong kind"
        );
        assert!(
            editor
                .set_field("trees", "no_such_field", serde_json::json!(1))
                .is_err()
        );
        assert!(
            editor
                .set_field("ferns", "density", serde_json::json!(1))
                .is_err()
        );
        assert!(editor.set_field("trees", "name", serde_json::json!("x")).is_err());
        editor.add_class("bushes").expect("add");
        assert!(editor.add_class("bushes").is_err(), "taken");
        editor.remove_class("rocks").expect("remove");
        assert!(editor.dirty);
        let saved = editor.save().expect("save");
        assert!(!editor.dirty);
        let on_disk = std::fs::read_to_string(&saved).expect("read");
        let reopened = ScatterRuleEditor::open(&saved).expect("reopen");
        assert_eq!(
            on_disk,
            reopened.set.to_ron().expect("ron"),
            "the file is canonical RON"
        );
        let names: Vec<&str> = reopened.set.classes.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["trees", "bushes"]);
        let trees = &reopened.set.classes[0];
        assert!((trees.density - 0.25).abs() < 1e-6);
        assert!(!trees.avoid_holes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn editor_world(root: &Path, level: &Path) -> World {
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
        world.insert_resource(crate::level_io::OpenLevel {
            id: regia_scene::level::LevelId::default(),
            path: Some(level.to_path_buf()),
        });
        world
    }

    /// A host terrain gets the level's saved density when the level opens (so a save does not
    /// wipe it); a level without a file starts with none; a broken file refuses edits and is kept.
    #[test]
    fn a_host_terrain_loads_the_levels_density_and_a_broken_file_refuses_edits() {
        let root = regia_test_utils::unique_temp_dir("terrain_panel_host_density");
        let maps = root.join("assets/maps");
        std::fs::create_dir_all(&maps).expect("dir");
        let level = maps.join("arena.level.ron");
        let mut saved = GrassDensityPaint::new(16).expect("new");
        saved
            .stroke(&density_stroke(vec![[0.0, 0.0]], 0.0), 64.0)
            .expect("stroke");
        std::fs::write(
            maps.join("arena.grassdensity.bin"),
            saved.to_bytes().expect("bytes"),
        )
        .expect("write");
        let mut world = editor_world(&root, &level);
        let e = world.spawn(hilly()).id();
        load_level_grass_density(&mut world);
        let loaded = world.get::<GrassDensityPaint>(e).expect("loaded");
        assert_eq!(loaded.digest(), saved.digest());
        assert!(!loaded.is_unsaved());
        assert_eq!(
            save_grass_density(&mut world, &level).expect("save"),
            None,
            "nothing new to write"
        );
        // Another level without a file: the host terrain carries no density over.
        let other = maps.join("other.level.ron");
        world.resource_mut::<crate::level_io::OpenLevel>().path = Some(other.clone());
        load_level_grass_density(&mut world);
        assert!(world.get::<GrassDensityPaint>(e).is_none());
        // A broken file: edits refused, the save leaves it alone.
        std::fs::write(maps.join("other.grassdensity.bin"), b"junk").expect("write");
        world.remove_resource::<GrassDensityLoaded>();
        load_level_grass_density(&mut world);
        assert!(world.resource::<GrassDensityLoaded>().refused);
        assert!(grass_density_stroke(&mut world, e, &density_stroke(vec![[0.0, 0.0]], 0.0)).is_err());
        assert_eq!(save_grass_density(&mut world, &other).expect("save"), None);
        assert_eq!(
            std::fs::read(maps.join("other.grassdensity.bin")).expect("read"),
            b"junk"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The editor builds the terrain a v12 level names, with a cook target at its map id, and
    /// side files follow the level terrain's map.
    #[test]
    fn the_editor_loads_the_terrain_the_level_names() {
        use regia_asset_pipeline::terrain_data::{
            HeightfieldTile, TiledHeightfield, write_tiled_heightfield_cooked,
        };
        let root = regia_test_utils::unique_temp_dir("terrain_panel_level_terrain");
        let assets = root.join("assets");
        std::fs::create_dir_all(assets.join("maps/arena")).expect("dirs");
        let layers = assets.join("maps/arena/terrain.layers.ron");
        std::fs::write(
            &layers,
            include_str!("../../regia_terrain/tests/fixtures/ground.terrain.layers.ron"),
        )
        .expect("write");
        let map_id = regia_asset::AssetId::new_random();
        std::fs::write(
            regia_asset::meta_path_for(&layers),
            regia_asset::MetaSidecar::new("terrain", map_id)
                .to_ron_string()
                .expect("meta"),
        )
        .expect("meta");
        let source = hilly();
        let cooked = regia_terrain::cook_all_tiles(&source.map, 32).expect("cook");
        let tiled = TiledHeightfield {
            version: cooked.version,
            width: cooked.width,
            height: cooked.height,
            tile_size: cooked.tile_size,
            tiles: cooked
                .tiles
                .into_iter()
                .map(|t| HeightfieldTile {
                    x: t.x,
                    y: t.y,
                    min: t.min,
                    max: t.max,
                    samples: t.samples,
                })
                .collect(),
        };
        let cache = regia_asset::cooked_cache_dir(&root, &regia_asset::current_platform());
        write_tiled_heightfield_cooked(map_id, &tiled, &cache).expect("cooked");
        let level = assets.join("maps/arena.level.ron");

        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        world.insert_resource(crate::level_io::OpenLevel {
            id: regia_scene::level::LevelId::default(),
            path: Some(level.clone()),
        });
        world.insert_resource(regia_scene::level::LevelSettings {
            terrain: Some(regia_scene::level::LevelTerrain {
                layers: "maps/arena/terrain.layers.ron".into(),
                map: "maps/arena/shared".into(),
                world_width_m: Some(64.0),
                height_scale_m: 50.0,
                height_offset_m: -5.0,
                ..Default::default()
            }),
            ..regia_scene::level::LevelSettings::default()
        });
        load_open_level_terrain(&mut world);
        let state = world.resource::<EditorLevelTerrain>().clone();
        assert_eq!(state.error, None);
        let e = state.entity.expect("built");
        let built = world.get::<Terrain>(e).expect("terrain");
        assert_eq!(
            built.map.to_heightfield().samples,
            source.map.to_heightfield().samples
        );
        let target = world.get::<TerrainCookTarget>(e).expect("cook target");
        assert_eq!(target.map_id, map_id);
        assert_eq!(target.cache_dir, cache);
        assert_eq!(side_file_base(&world, &level), assets.join("maps/arena/shared"));
        // Once per level: a second frame builds nothing new.
        load_open_level_terrain(&mut world);
        assert_eq!(world.resource::<EditorLevelTerrain>().entity, Some(e));
        // A density edit saves under the level terrain's map.
        grass_density_stroke(&mut world, e, &density_stroke(vec![[0.0, 0.0]], 0.0)).expect("stroke");
        let file = save_grass_density(&mut world, &level)
            .expect("save")
            .expect("written");
        assert_eq!(file, assets.join("maps/arena/shared.grassdensity.bin"));
        // A failed load is not retried every frame; Retry (removing the state) does.
        world.resource_mut::<regia_scene::level::LevelSettings>().terrain =
            Some(regia_scene::level::LevelTerrain {
                layers: "maps/none/terrain.layers.ron".into(),
                ..regia_scene::level::LevelTerrain::default()
            });
        // The sculpt-free terrain is replaced only once the new block loads; it fails here.
        load_open_level_terrain(&mut world);
        let failed = world.resource::<EditorLevelTerrain>().clone();
        assert_eq!(
            failed.error.as_ref().map(|(c, _)| c.as_str()),
            Some("level_terrain_read")
        );
        world.resource_mut::<EditorLevelTerrain>().digest = Some(7);
        load_open_level_terrain(&mut world);
        assert_eq!(
            world.resource::<EditorLevelTerrain>().digest,
            Some(7),
            "not reloaded"
        );
        // Opening another level that names none drops this level's terrain and its side-file map.
        let other = assets.join("maps/other.level.ron");
        world.resource_mut::<crate::level_io::OpenLevel>().path = Some(other.clone());
        world.resource_mut::<regia_scene::level::LevelSettings>().terrain = None;
        load_open_level_terrain(&mut world);
        assert!(world.get_entity(e).is_err(), "the other level's terrain is gone");
        assert_eq!(side_file_base(&world, &other), other);
        let _ = std::fs::remove_dir_all(&root);
    }
}
