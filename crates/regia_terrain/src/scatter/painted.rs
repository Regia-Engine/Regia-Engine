// SPDX-License-Identifier: MPL-2.0
//! Painted mesh instances (`docs/specs/82-terrain-editing.md` § 13, ADR-1936): the hand-placed
//! half of a map's scatter, next to the seeded half in [`super::placement`].
//!
//! - [`PaintedScatter`]: instances stored per [`PAINT_TILE_SIZE_M`] tile and per class. A tile is
//!   an `Arc`, so a clone of the store (what [`super::field::ScatterField::set_painted`] keeps)
//!   costs one pointer per tile and an edit copies only the tiles it touches.
//! - [`ScatterBrush`]: density, scale range and slope / height filters, on top of the class's
//!   own rules (layer mask, exclusions, holes).
//! - [`apply_scatter_stroke`]: one stroke, whose [`ScatterStroke`] is a [`StrokeUndo::Scatter`]
//!   entry of the terrain edit journal, so undo and redo restore the store bit for bit.
//! - `<map>.scatterpaint.bin`: the saved form, a magic and a version ahead of a postcard body.
//!
//! A brush walks a jittered grid of `1 / sqrt(density)` meter cells anchored at the world origin.
//! Cell `(cx, cz)` draws its position and its other values from a PCG32 stream seeded by the set
//! seed, the class name, the brush seed, the cell size and the cell coordinate, and keeps its
//! candidate when its `keep` draw is below the brush coverage there. So:
//!
//! - a stroke is a function of the brush, the path and the terrain, on every client;
//! - density is a target: painting the same place again with the same brush adds nothing, and a
//!   soft edge fills in as the brush passes closer;
//! - a stroke applied as one path or as consecutive segments gives the same instances, because a
//!   candidate's coverage is the maximum over the path (with `min_distance` 0).
//!
//! Instances keep the ground height and normal they were seated with; a sculpt re-seats the ones
//! it moved ([`reseat_painted_scatter`]). Tilt and tint are drawn values, resolved against the
//! class when the instance is placed in a field, so a class edit restyles what was painted.
//! `flags` holds per-instance foliage wind (spec 88 § 2.5, ADR-2304): [`PAINTED_FLAG_STILL`] keeps an
//! instance at rest when its mesh's material sways ([`ScatterBrush::wind`] off paints it). The other
//! bits are reserved: written 0 and kept as read. A v1 file's zero flags follow the wind.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use glam::Vec2;
use regia_app::prelude::*;
use regia_core::math::{Rng, det_acos, det_powf};
use serde::{Deserialize, Serialize};

use super::cell_seed;
use super::placement::{PlacedInstance, cos_approx, name_hash, tilted_up};
use super::runtime::RuntimeScatter;
use super::set::{ScatterClass, ScatterSet};
use crate::Terrain;
use crate::brush::raster::{BrushRaster, CircleRaster, RectRaster, apply_hardness};
use crate::brush::{Brush, BrushShape, TileCoord, union_rect, world_to_sample};
use crate::deform::DirtyRect;
use crate::edit::{EDIT_CELL_SIZE, StrokeUndo, TerrainEdits};
use crate::holes::HolesMask;
use crate::layers::TerrainLayers;
use crate::sampler::TerrainMap;

/// Version of the painted-scatter file (`<map>.scatterpaint.bin`).
pub const SCATTER_PAINT_FORMAT_VERSION: u32 = 1;
/// First four bytes of a painted-scatter file; the `u32` LE version follows.
pub const SCATTER_PAINT_MAGIC: &[u8; 4] = b"RSPT";
/// Side of a storage tile, meters, anchored at the world origin.
pub const PAINT_TILE_SIZE_M: f32 = 64.0;
/// [`PaintedInstance::flags`] bit: the instance stays at rest when its mesh's material sways
/// (`regia_render::FoliageSway`). Clear (every v1 file) it follows the scene wind like a seeded
/// instance (`specs/88` § 2.5).
pub const PAINTED_FLAG_STILL: u16 = 1;
/// Most instances one store holds (a corrupt file or a runaway agent must not exhaust memory).
pub const MAX_PAINTED_INSTANCES: usize = 16 << 20;
/// Most candidates one stroke may walk.
pub const MAX_STROKE_CANDIDATES: u64 = 4 << 20;
/// Largest density a brush accepts, instances per square meter.
pub const MAX_PAINT_DENSITY: f32 = 64.0;
/// Largest brush reach, meters.
pub const MAX_BRUSH_REACH_M: f32 = 4096.0;
/// Largest painted-scatter file read, bytes.
pub const MAX_SCATTER_PAINT_BYTES: u64 = 2 << 30;

/// Values drawn per candidate (x, z, mesh, scale, yaw, tilt x, tilt z, shade, keep, brush scale).
const DRAWS: usize = 10;

/// The painted-scatter file of a map, next to it and named as [`crate::holes_ron_path`] names the
/// holes file: `maps/switchbacks.level.ron` → `maps/switchbacks.scatterpaint.bin`.
#[must_use]
pub fn scatter_paint_path(map: &Path) -> PathBuf {
    let holes = crate::holes_ron_path(map);
    let name = holes.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let stem = name.strip_suffix(".holes.ron").unwrap_or(name);
    holes.with_file_name(format!("{stem}.scatterpaint.bin"))
}

/// Errors from painting scatter or reading a painted-scatter file.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ScatterPaintError {
    /// The stroke path is empty or not finite.
    #[error("a scatter stroke needs at least one finite point")]
    BadPath,
    /// A brush parameter is not finite or out of range, or the class has no meshes.
    #[error("invalid scatter brush: {0}")]
    BadBrush(String),
    /// The brush names a class the scatter set does not have.
    #[error("no scatter class `{0}`")]
    UnknownClass(String),
    /// The stroke would walk too many candidates.
    #[error("the stroke walks {candidates} candidates; the limit is {limit}: lower the density or the brush")]
    TooLarge {
        /// Candidates the stroke would walk.
        candidates: u64,
        /// [`MAX_STROKE_CANDIDATES`].
        limit: u64,
    },
    /// The store is full.
    #[error("the stroke would leave {total} painted instances; the limit is {limit}")]
    Full {
        /// Instances after the stroke.
        total: usize,
        /// [`MAX_PAINTED_INSTANCES`].
        limit: usize,
    },
    /// The file's version is not one this build reads.
    #[error("unsupported painted scatter version {found}, expected {expected}")]
    BadVersion {
        /// Version in the file.
        found: u32,
        /// Version this build reads.
        expected: u32,
    },
    /// The file could not be read.
    #[error("{0}")]
    Read(String),
    /// The file is corrupt.
    #[error("invalid painted scatter file: {0}")]
    Decode(String),
}

impl ScatterPaintError {
    /// The agent protocol's error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadPath | Self::BadBrush(_) => "scatter_paint_bad_brush",
            Self::UnknownClass(_) => "scatter_paint_unknown_class",
            Self::TooLarge { .. } | Self::Full { .. } => "scatter_paint_too_large",
            Self::Read(_) => "scatter_paint_read",
            Self::BadVersion { .. } | Self::Decode(_) => "scatter_paint_decode",
        }
    }
}

/// One painted instance.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaintedInstance {
    /// Stable id: the candidate's, so the same candidate is never placed twice. Keys the
    /// renderer's object cache and orders a tile.
    pub id: u64,
    /// Index into the class's mesh table in the store.
    pub mesh: u16,
    /// Per-instance foliage wind ([`PAINTED_FLAG_STILL`]); the other bits are reserved (0, kept as
    /// read).
    pub flags: u16,
    /// Ground contact point, meters.
    pub position: [f32; 3],
    /// Unit ground normal at the position.
    pub normal: [f32; 3],
    /// Yaw about the instance's up axis, radians.
    pub yaw: f32,
    /// Uniform scale, the class's and the brush's.
    pub scale: f32,
    /// Draw for the tint (`0..1`, turned into a brightness by the class's `tint_jitter`).
    pub shade: f32,
    /// Draws for the tilt jitter (`0..1` each, scaled by the class's `tilt_jitter`).
    pub tilt: [f32; 2],
}

impl PaintedInstance {
    /// The instance as a field places it: tilt and tint resolved against `class`.
    pub(super) fn placed(&self, class_index: u16, mesh: u16, class: &ScatterClass) -> PlacedInstance {
        PlacedInstance {
            class: class_index,
            mesh,
            position: self.position,
            up: tilted_up(self.normal, class, self.tilt),
            yaw: self.yaw,
            scale: self.scale,
            shade: 1.0 + (self.shade * 2.0 - 1.0) * class.tint_jitter,
            normal: self.normal,
        }
    }

    fn tile(&self) -> (i32, i32) {
        paint_tile(self.position[0], self.position[2])
    }
}

/// The storage tile of world point `(x, z)`.
fn paint_tile(x: f32, z: f32) -> (i32, i32) {
    (
        (x / PAINT_TILE_SIZE_M).floor() as i32,
        (z / PAINT_TILE_SIZE_M).floor() as i32,
    )
}

/// Instances of a tile, sorted by id.
type PaintTile = Arc<Vec<PaintedInstance>>;

/// The painted instances of one class.
#[derive(Clone, Debug, Default, PartialEq)]
struct PaintClass {
    name: String,
    /// Mesh paths, by [`PaintedInstance::mesh`]. Paths, so reordering a class's meshes does not
    /// swap what was painted; only ever appended to.
    meshes: Vec<String>,
    tiles: BTreeMap<(i32, i32), PaintTile>,
}

impl PaintClass {
    fn len(&self) -> usize {
        self.tiles.values().map(|t| t.len()).sum()
    }

    fn mesh_slot(&mut self, path: &str) -> u16 {
        match self.meshes.iter().position(|m| m == path) {
            Some(i) => i as u16,
            None => {
                self.meshes.push(path.to_owned());
                (self.meshes.len() - 1) as u16
            }
        }
    }

    /// Adds `instances`, skipping ids the tile already holds. Returns how many were added.
    fn insert(&mut self, instances: impl IntoIterator<Item = PaintedInstance>) -> usize {
        let mut by_tile: BTreeMap<(i32, i32), Vec<PaintedInstance>> = BTreeMap::new();
        for instance in instances {
            by_tile.entry(instance.tile()).or_default().push(instance);
        }
        let mut added = 0;
        for (key, new) in by_tile {
            let tile = Arc::make_mut(self.tiles.entry(key).or_default());
            let before = tile.len();
            tile.extend(new);
            tile.sort_by_key(|i| i.id);
            tile.dedup_by_key(|i| i.id);
            added += tile.len() - before;
        }
        added
    }

    /// Removes `instances` by id. Returns how many were removed. A tile left empty goes, so an
    /// undone stroke leaves the store as it was.
    fn remove(&mut self, instances: &[PaintedInstance]) -> usize {
        let mut by_tile: BTreeMap<(i32, i32), BTreeSet<u64>> = BTreeMap::new();
        for instance in instances {
            by_tile.entry(instance.tile()).or_default().insert(instance.id);
        }
        let mut removed = 0;
        for (key, ids) in by_tile {
            let Some(tile) = self.tiles.get_mut(&key) else {
                continue;
            };
            let list = Arc::make_mut(tile);
            let before = list.len();
            list.retain(|i| !ids.contains(&i.id));
            removed += before - list.len();
            if list.is_empty() {
                self.tiles.remove(&key);
            }
        }
        removed
    }
}

/// One class in a [`PaintedScatter::report`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PaintedClassReport {
    /// Class name.
    pub name: String,
    /// Instances painted.
    pub instances: u32,
    /// Tiles holding them.
    pub tiles: u32,
}

/// Hands out edit generations: unique in a process, so a store replaced by a load is never
/// mistaken for the one a field already drew.
static GENERATIONS: AtomicU64 = AtomicU64::new(1);

fn next_generation() -> u64 {
    GENERATIONS.fetch_add(1, Ordering::Relaxed)
}

/// A map's painted instances, per class and per tile. See the module docs.
///
/// A clone is cheap (the tiles are shared), so a clone is a snapshot.
#[derive(Clone, Debug, Default)]
pub struct PaintedScatter {
    classes: Vec<PaintClass>,
    /// Changes with every edit; 0 for a new store.
    generation: u64,
}

impl PartialEq for PaintedScatter {
    /// Same instances: classes without any do not count, nor does the edit generation.
    fn eq(&self, other: &Self) -> bool {
        self.live_classes() == other.live_classes()
    }
}

impl PaintedScatter {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Changes with every edit: cheap change detection for a field.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Instances painted.
    #[must_use]
    pub fn len(&self) -> usize {
        self.classes.iter().map(PaintClass::len).sum()
    }

    /// `true` when nothing is painted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.classes.iter().all(|c| c.tiles.is_empty())
    }

    /// Per-class counts, classes without instances left out.
    #[must_use]
    pub fn report(&self) -> Vec<PaintedClassReport> {
        self.classes
            .iter()
            .filter(|c| !c.tiles.is_empty())
            .map(|c| PaintedClassReport {
                name: c.name.clone(),
                instances: c.len() as u32,
                tiles: c.tiles.len() as u32,
            })
            .collect()
    }

    /// Classes holding instances, by name.
    fn live_classes(&self) -> Vec<&PaintClass> {
        let mut classes: Vec<&PaintClass> = self.classes.iter().filter(|c| !c.tiles.is_empty()).collect();
        classes.sort_by(|a, b| a.name.cmp(&b.name));
        classes
    }

    fn class(&self, name: &str) -> Option<&PaintClass> {
        self.classes.iter().find(|c| c.name == name)
    }

    fn class_mut(&mut self, name: &str) -> &mut PaintClass {
        let index = match self.classes.iter().position(|c| c.name == name) {
            Some(i) => i,
            None => {
                self.classes.push(PaintClass {
                    name: name.to_owned(),
                    ..PaintClass::default()
                });
                self.classes.len() - 1
            }
        };
        &mut self.classes[index]
    }

    /// Every instance of class `name`, tile by tile.
    pub fn instances(&self, name: &str) -> impl Iterator<Item = &PaintedInstance> {
        self.class(name)
            .into_iter()
            .flat_map(|c| c.tiles.values())
            .flat_map(|t| t.iter())
    }

    /// Mesh path of `instance` in class `name`.
    #[must_use]
    pub fn mesh_path(&self, name: &str, instance: &PaintedInstance) -> Option<&str> {
        let class = self.class(name)?;
        class.meshes.get(usize::from(instance.mesh)).map(String::as_str)
    }

    /// Visits the instances of class `name` in the half-open XZ box `min..max`, with their mesh
    /// path, in tile then id order.
    pub(super) fn visit_box(
        &self,
        name: &str,
        min: [f32; 2],
        max: [f32; 2],
        mut visit: impl FnMut(&str, &PaintedInstance),
    ) {
        let Some(class) = self.class(name) else { return };
        let (tx0, tz0) = paint_tile(min[0], min[1]);
        let (tx1, tz1) = paint_tile(max[0], max[1]);
        for (&(tx, tz), tile) in &class.tiles {
            if tx < tx0 || tx > tx1 || tz < tz0 || tz > tz1 {
                continue;
            }
            for instance in tile.iter() {
                let [x, _, z] = instance.position;
                if x >= min[0]
                    && x < max[0]
                    && z >= min[1]
                    && z < max[1]
                    && let Some(path) = class.meshes.get(usize::from(instance.mesh))
                {
                    visit(path, instance);
                }
            }
        }
    }

    /// Tiles of class `name` that differ from `older`, which must be an earlier state of this
    /// store: a tile `Arc` that an edit did not touch is still the same `Arc`.
    pub(super) fn changed_tiles(&self, older: &Self, name: &str) -> Vec<(i32, i32)> {
        static EMPTY: BTreeMap<(i32, i32), PaintTile> = BTreeMap::new();
        let new = self.class(name).map_or(&EMPTY, |c| &c.tiles);
        let old = older.class(name).map_or(&EMPTY, |c| &c.tiles);
        let mut out: Vec<(i32, i32)> = new
            .iter()
            .filter(|(key, tile)| old.get(*key).is_none_or(|t| !Arc::ptr_eq(t, tile)))
            .map(|(key, _)| *key)
            .collect();
        out.extend(old.keys().filter(|key| !new.contains_key(*key)));
        out
    }

    /// Re-seats instances on `map`: height and normal from the surface, for those in `rect` (every
    /// instance for `None`). Returns how many moved. Tiles that do not change are not copied.
    pub fn reseat(&mut self, map: &TerrainMap, rect: Option<DirtyRect>) -> usize {
        let bounds = rect.map(|r| world_bounds(map, r));
        let inside = |i: &PaintedInstance| {
            bounds.is_none_or(|[lo, hi]| {
                let [x, _, z] = i.position;
                x >= lo.x && x <= hi.x && z >= lo.y && z <= hi.y
            })
        };
        let seat = |i: &PaintedInstance| {
            let [x, _, z] = i.position;
            (map.sample_bilinear(x, z), map.normal_at(x, z))
        };
        let mut moved = 0;
        for class in &mut self.classes {
            for tile in class.tiles.values_mut() {
                let stale = |i: &PaintedInstance| inside(i) && seat(i) != (i.position[1], i.normal);
                if !tile.iter().any(stale) {
                    continue;
                }
                for instance in Arc::make_mut(tile).iter_mut().filter(|i| inside(i)) {
                    let (y, normal) = seat(instance);
                    if (y, normal) != (instance.position[1], instance.normal) {
                        instance.position[1] = y;
                        instance.normal = normal;
                        moved += 1;
                    }
                }
            }
        }
        if moved > 0 {
            self.generation = next_generation();
        }
        moved
    }

    /// The saved form: magic, `u32` LE version, postcard body. The same instances give the same
    /// bytes (classes by name; classes without instances are not written).
    ///
    /// # Errors
    ///
    /// [`ScatterPaintError::Decode`] when postcard fails.
    pub fn to_bytes(&self) -> Result<Vec<u8>, ScatterPaintError> {
        let classes: Vec<PaintFileClass> = self
            .live_classes()
            .into_iter()
            .map(|c| PaintFileClass {
                name: c.name.clone(),
                meshes: c.meshes.clone(),
                tiles: c
                    .tiles
                    .iter()
                    .map(|(&(x, z), tile)| PaintFileTile {
                        tile: [x, z],
                        instances: tile.as_ref().clone(),
                    })
                    .collect(),
            })
            .collect();
        let mut out = SCATTER_PAINT_MAGIC.to_vec();
        out.extend(SCATTER_PAINT_FORMAT_VERSION.to_le_bytes());
        let body = postcard::to_allocvec(&PaintFile { classes })
            .map_err(|e| ScatterPaintError::Decode(e.to_string()))?;
        out.extend(body);
        Ok(out)
    }

    /// Reads [`Self::to_bytes`]' output.
    ///
    /// # Errors
    ///
    /// [`ScatterPaintError::BadVersion`] for another version, read before the body, and
    /// [`ScatterPaintError::Decode`] for anything else wrong with the file.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ScatterPaintError> {
        let bad = |what: String| ScatterPaintError::Decode(what);
        let (magic, rest) = bytes
            .split_first_chunk::<4>()
            .ok_or_else(|| bad("shorter than its header".into()))?;
        if magic != SCATTER_PAINT_MAGIC {
            return Err(bad("wrong magic".into()));
        }
        let (version, body) = rest
            .split_first_chunk::<4>()
            .ok_or_else(|| bad("shorter than its header".into()))?;
        let version = u32::from_le_bytes(*version);
        if version != SCATTER_PAINT_FORMAT_VERSION {
            return Err(ScatterPaintError::BadVersion {
                found: version,
                expected: SCATTER_PAINT_FORMAT_VERSION,
            });
        }
        let file: PaintFile = postcard::from_bytes(body).map_err(|e| bad(e.to_string()))?;
        let mut classes: Vec<PaintClass> = Vec::with_capacity(file.classes.len());
        let mut total = 0usize;
        for class in file.classes {
            if class.name.is_empty() || classes.iter().any(|c| c.name == class.name) {
                return Err(bad(format!("bad or repeated class name `{}`", class.name)));
            }
            let mut tiles = BTreeMap::new();
            for mut tile in class.tiles {
                let key = (tile.tile[0], tile.tile[1]);
                total = total.saturating_add(tile.instances.len());
                if total > MAX_PAINTED_INSTANCES {
                    return Err(bad(format!("more than {MAX_PAINTED_INSTANCES} instances")));
                }
                for instance in &tile.instances {
                    if !usable(instance, class.meshes.len()) || instance.tile() != key {
                        return Err(bad(format!(
                            "instance {} of class `{}` is not valid in tile {key:?}",
                            instance.id, class.name
                        )));
                    }
                }
                tile.instances.sort_by_key(|i| i.id);
                if tile.instances.is_empty() || tile.instances.windows(2).any(|w| w[0].id == w[1].id) {
                    return Err(bad(format!("tile {key:?} is empty or repeats an id")));
                }
                if tiles.insert(key, Arc::new(tile.instances)).is_some() {
                    return Err(bad(format!("tile {key:?} appears twice")));
                }
            }
            classes.push(PaintClass {
                name: class.name,
                meshes: class.meshes,
                tiles,
            });
        }
        Ok(Self {
            classes,
            generation: next_generation(),
        })
    }
}

fn usable(i: &PaintedInstance, meshes: usize) -> bool {
    usize::from(i.mesh) < meshes
        && i.scale.is_finite()
        && i.scale > 0.0
        && i.yaw.is_finite()
        && i.position.iter().chain(&i.normal).all(|v| v.is_finite())
        && [i.shade, i.tilt[0], i.tilt[1]]
            .iter()
            .all(|v| (0.0..=1.0).contains(v))
}

/// The world XZ box of `rect`, grown by the reach of a height or normal lookup (two samples).
fn world_bounds(map: &TerrainMap, rect: DirtyRect) -> [Vec2; 2] {
    let half = map.world_width() * 0.5;
    let spacing = map.sample_spacing();
    let world = |col: u32, row: u32| Vec2::new(col as f32 * spacing - half, row as f32 * spacing - half);
    let margin = Vec2::splat(2.0 * spacing);
    [
        world(rect.col_start, rect.row_start) - margin,
        world(rect.col_end, rect.row_end) + margin,
    ]
}

/// Loads a `<map>.scatterpaint.bin`. `Ok(None)` when the file does not exist.
///
/// # Errors
///
/// [`ScatterPaintError::Read`] for an unreadable or oversize file, and the
/// [`PaintedScatter::from_bytes`] errors.
pub fn load_scatter_paint(file: &Path) -> Result<Option<PaintedScatter>, ScatterPaintError> {
    let read_error = |e: std::io::Error| ScatterPaintError::Read(format!("{}: {e}", file.display()));
    let size = match std::fs::metadata(file) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(read_error(e)),
    };
    if size > MAX_SCATTER_PAINT_BYTES {
        return Err(ScatterPaintError::Read(format!(
            "{} is {size} bytes; the largest painted scatter file read is {MAX_SCATTER_PAINT_BYTES}",
            file.display()
        )));
    }
    let bytes = std::fs::read(file).map_err(read_error)?;
    PaintedScatter::from_bytes(&bytes).map(Some)
}

/// The file's body. Field order is the format.
#[derive(Serialize, Deserialize)]
struct PaintFile {
    classes: Vec<PaintFileClass>,
}

#[derive(Serialize, Deserialize)]
struct PaintFileClass {
    name: String,
    meshes: Vec<String>,
    tiles: Vec<PaintFileTile>,
}

#[derive(Serialize, Deserialize)]
struct PaintFileTile {
    tile: [i32; 2],
    instances: Vec<PaintedInstance>,
}

/// The versions a migration step leaves, for `cargo xtask check-formats`: none, v1 is the first.
#[must_use]
pub fn scatter_paint_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde field-list fingerprint of the `.scatterpaint.bin` body, hashed by
/// `cargo xtask check-formats`.
#[must_use]
pub fn scatter_paint_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<PaintFile>(),
        serde_field_names::<PaintFileClass>(),
        serde_field_names::<PaintFileTile>(),
        serde_field_names::<PaintedInstance>(),
    ])
}

/// What a scatter stroke does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ScatterBrushMode {
    /// Place instances up to the brush's density.
    #[default]
    Paint,
    /// Remove instances.
    Erase,
}

/// A scatter brush. [`ScatterBrush::for_class`] starts from a class's own filters.
#[derive(Clone, Debug, PartialEq)]
pub struct ScatterBrush {
    /// Class painted. Needed to paint; an erase without one takes every class.
    pub class: Option<String>,
    /// Paint or erase.
    pub mode: ScatterBrushMode,
    /// Shape and size, meters.
    pub shape: BrushShape,
    /// Edge softness: 0 fades from the center, 1 keeps full strength to the edge.
    pub hardness: f32,
    /// Fraction of the brush's density placed (or of the instances removed) at full coverage.
    pub strength: f32,
    /// Target instances per square meter.
    pub density: f32,
    /// Random multiplier on the class's scale, `[min, max]`.
    pub scale: [f32; 2],
    /// Slope filter, radians from up, `[min, max]`.
    pub slope: [f32; 2],
    /// Altitude filter, meters, `[min, max]`.
    pub height: [f32; 2],
    /// Nothing lands closer than this to an instance of the class, meters (0 = off). Off, the
    /// grid's jitter is the only spacing.
    pub min_distance: f32,
    /// Picks the candidate grid: strokes with the same seed share it, so they do not stack.
    pub seed: u32,
    /// Painted instances follow the scene wind when their mesh's material sways (`true`, the
    /// default); `false` paints them at rest ([`PAINTED_FLAG_STILL`]: a dead tree in a swaying
    /// wood). Erasing ignores it.
    pub wind: bool,
}

impl Default for ScatterBrush {
    fn default() -> Self {
        Self {
            class: None,
            mode: ScatterBrushMode::Paint,
            shape: BrushShape::default(),
            hardness: 0.0,
            strength: 1.0,
            density: 0.1,
            scale: [1.0, 1.0],
            slope: [0.0, std::f32::consts::FRAC_PI_2],
            height: [-100_000.0, 100_000.0],
            min_distance: 0.0,
            seed: 0,
            wind: true,
        }
    }
}

impl ScatterBrush {
    /// A paint brush for `class` with the class's slope and altitude filters.
    #[must_use]
    pub fn for_class(class: &ScatterClass) -> Self {
        Self {
            class: Some(class.name.clone()),
            slope: [class.slope_min, class.slope_max],
            height: [class.height_min, class.height_max],
            ..Self::default()
        }
    }

    /// An erase brush for `class` (every class for `None`), with open filters.
    #[must_use]
    pub fn eraser(class: Option<&str>) -> Self {
        Self {
            class: class.map(str::to_owned),
            mode: ScatterBrushMode::Erase,
            ..Self::default()
        }
    }

    /// Checks every parameter.
    ///
    /// # Errors
    ///
    /// [`ScatterPaintError::BadBrush`] naming the field.
    pub fn validate(&self) -> Result<(), ScatterPaintError> {
        let bad = |what: &str| Err(ScatterPaintError::BadBrush(what.to_owned()));
        let reach = self.shape.reach();
        if !(reach.is_finite() && reach > 0.0 && reach <= MAX_BRUSH_REACH_M) {
            return bad("radius (above 0, at most 4096 m)");
        }
        if let BrushShape::Rectangle { width, length } = self.shape
            && !(width > 0.0 && length > 0.0)
        {
            return bad("width and length (above 0)");
        }
        if !(0.0..=1.0).contains(&self.hardness) {
            return bad("hardness (0..=1)");
        }
        if !(0.0..=1.0).contains(&self.strength) {
            return bad("strength (0..=1)");
        }
        if !(self.density.is_finite() && self.density > 0.0 && self.density <= MAX_PAINT_DENSITY) {
            return bad("density (above 0, at most 64 per square meter)");
        }
        if !(self.scale[0].is_finite() && self.scale[0] > 0.0 && self.scale[1] >= self.scale[0])
            || !self.scale[1].is_finite()
        {
            return bad("scale (0 < min <= max)");
        }
        if !(self.slope[0] >= 0.0 && self.slope[1] >= self.slope[0] && self.slope[1].is_finite()) {
            return bad("slope (0 <= min <= max, radians)");
        }
        if !(self.height[0].is_finite() && self.height[1].is_finite() && self.height[0] <= self.height[1]) {
            return bad("height (min <= max, finite)");
        }
        if !(self.min_distance.is_finite() && self.min_distance >= 0.0) {
            return bad("min_distance (at least 0)");
        }
        Ok(())
    }

    /// The candidate grid's cell size, meters.
    fn cell(&self) -> f32 {
        1.0 / self.density.sqrt()
    }

    /// Coverage (`0..=1`) of `point` from the segment `a..b` of the path: the falloff at the
    /// nearest point of the segment. Exact for a circle; a rectangle stays axis-aligned and is a
    /// little narrow on a diagonal drag.
    fn coverage(&self, point: Vec2, a: Vec2, b: Vec2) -> f32 {
        let ab = b - a;
        let len2 = ab.length_squared();
        let t = if len2 > 0.0 {
            ((point - a).dot(ab) / len2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let local = point - (a + ab * t);
        let strength = match self.shape {
            BrushShape::Circle { radius } => CircleRaster(radius).strength_at(local),
            BrushShape::Rectangle { width, length } => {
                RectRaster(width * 0.5, length * 0.5).strength_at(local)
            }
        };
        apply_hardness(self.hardness, strength)
    }

    /// `true` when the ground passes the brush's slope and altitude filters.
    fn ground_passes(&self, y: f32, normal: &[f32; 3]) -> bool {
        y >= self.height[0]
            && y <= self.height[1]
            && normal[1] <= cos_approx(self.slope[0])
            && normal[1] >= cos_approx(self.slope[1])
    }

    /// The path as segments (a single point is one empty segment).
    fn segments(path: &[Vec2]) -> impl Iterator<Item = (Vec2, Vec2)> + '_ {
        let single = (path.len() == 1).then(|| (path[0], path[0]));
        single.into_iter().chain(path.windows(2).map(|w| (w[0], w[1])))
    }
}

/// What a stroke reads besides the brush.
#[derive(Clone, Copy)]
pub struct ScatterPaintInputs<'a> {
    /// The surface instances sit on.
    pub map: &'a TerrainMap,
    /// Holes, for classes that avoid them.
    pub holes: Option<&'a HolesMask>,
    /// Layer stack, for a class's layer mask.
    pub layers: Option<&'a TerrainLayers>,
    /// The scatter set's seed.
    pub seed: u64,
}

/// One class's instances added or removed by a stroke. The mesh table goes with them, so an undo
/// after the store was replaced still restores the right meshes.
#[derive(Clone, Debug, PartialEq)]
pub struct ScatterDelta {
    /// Class name.
    pub class: String,
    /// The class's mesh table at the stroke.
    pub meshes: Vec<String>,
    /// The instances.
    pub instances: Vec<PaintedInstance>,
}

/// A scatter stroke's journal entry: what it added and removed.
#[derive(Clone, Debug, PartialEq)]
pub struct ScatterStroke {
    /// Paint or erase.
    pub mode: ScatterBrushMode,
    /// Added instances (a paint).
    pub added: Vec<ScatterDelta>,
    /// Removed instances (an erase).
    pub removed: Vec<ScatterDelta>,
    /// Shape, hardness and strength, for the journal's description of a stroke.
    pub brush: Brush,
    /// Bounds of the changed instances, sample coordinates.
    pub dirty: DirtyRect,
    /// Edit cells ([`EDIT_CELL_SIZE`]) holding them.
    pub tiles: BTreeSet<TileCoord>,
}

impl ScatterStroke {
    fn new(brush: &ScatterBrush) -> Self {
        Self {
            mode: brush.mode,
            added: Vec::new(),
            removed: Vec::new(),
            brush: Brush {
                shape: brush.shape,
                hardness: brush.hardness,
                alpha: brush.strength,
                ..Brush::default()
            },
            dirty: DirtyRect {
                col_start: 0,
                col_end: 0,
                row_start: 0,
                row_end: 0,
            },
            tiles: BTreeSet::new(),
        }
    }

    /// Instances changed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.added
            .iter()
            .chain(&self.removed)
            .map(|d| d.instances.len())
            .sum()
    }

    /// `true` when the stroke changed nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Writes the store as it was before (`undo`) or after the stroke. Instances come back as
    /// they were recorded, bit for bit; a sculpt since then re-seats them
    /// ([`TerrainEdits::note_scatter`]).
    pub fn restore(&self, store: &mut PaintedScatter, undo: bool) {
        let (gone, back) = if undo {
            (&self.added, &self.removed)
        } else {
            (&self.removed, &self.added)
        };
        for delta in gone {
            store.class_mut(&delta.class).remove(&delta.instances);
        }
        for delta in back {
            let class = store.class_mut(&delta.class);
            let slots: Vec<u16> = delta.meshes.iter().map(|m| class.mesh_slot(m)).collect();
            class.insert(delta.instances.iter().map(|i| PaintedInstance {
                mesh: slots.get(usize::from(i.mesh)).copied().unwrap_or(i.mesh),
                ..*i
            }));
        }
        store.generation = next_generation();
    }

    /// Adds a later piece of the same stroke, so a drag is one journal entry.
    pub fn merge(&mut self, next: Self) {
        let extend = |into: &mut Vec<ScatterDelta>, from: Vec<ScatterDelta>| {
            for delta in from {
                match into.iter_mut().find(|d| d.class == delta.class) {
                    Some(d) => {
                        d.meshes = delta.meshes;
                        d.instances.extend(delta.instances);
                    }
                    None => into.push(delta),
                }
            }
        };
        if self.is_empty() {
            self.dirty = next.dirty;
        } else if !next.is_empty() {
            self.dirty = union_rect(self.dirty, next.dirty);
        }
        extend(&mut self.added, next.added);
        extend(&mut self.removed, next.removed);
        self.tiles.extend(next.tiles);
    }

    /// Fills `dirty` and `tiles` from the instances.
    fn locate(&mut self, map: &TerrainMap) {
        let mut dirty: Option<DirtyRect> = None;
        for instance in self.added.iter().chain(&self.removed).flat_map(|d| &d.instances) {
            let [x, _, z] = instance.position;
            let s = world_to_sample(map, x, z);
            let (col, row) = (s.x.max(0.0) as u32, s.y.max(0.0) as u32);
            self.tiles.insert((col / EDIT_CELL_SIZE, row / EDIT_CELL_SIZE));
            let one = DirtyRect {
                col_start: col,
                col_end: col + 1,
                row_start: row,
                row_end: row + 1,
            };
            dirty = Some(dirty.map_or(one, |d| union_rect(d, one)));
        }
        if let Some(dirty) = dirty {
            self.dirty = dirty;
        }
    }
}

/// Runs one stroke on `store`: a stamp at `path[0]` (world XZ, meters), smeared through the rest.
/// The class's layer mask, exclusions and holes rule always apply; slope and altitude are the
/// brush's. Nothing changes on an error.
///
/// # Errors
///
/// [`ScatterPaintError`] for a bad path or brush, an unknown class, a stroke past
/// [`MAX_STROKE_CANDIDATES`] or a store past [`MAX_PAINTED_INSTANCES`].
pub fn scatter_stroke(
    store: &mut PaintedScatter,
    set: &ScatterSet,
    inputs: ScatterPaintInputs<'_>,
    brush: &ScatterBrush,
    path: &[Vec2],
) -> Result<ScatterStroke, ScatterPaintError> {
    let _span = tracing::info_span!(target: "regia_terrain", "scatter.stroke").entered();
    if path.is_empty() || path.iter().any(|p| !p.is_finite()) {
        return Err(ScatterPaintError::BadPath);
    }
    brush.validate()?;
    let mut stroke = match brush.mode {
        ScatterBrushMode::Paint => paint(store, set, inputs, brush, path)?,
        ScatterBrushMode::Erase => erase(store, set, brush, path)?,
    };
    if !stroke.is_empty() {
        stroke.locate(inputs.map);
        stroke.restore(store, false);
    }
    Ok(stroke)
}

fn paint(
    store: &PaintedScatter,
    set: &ScatterSet,
    inputs: ScatterPaintInputs<'_>,
    brush: &ScatterBrush,
    path: &[Vec2],
) -> Result<ScatterStroke, ScatterPaintError> {
    let Some(name) = brush.class.as_deref() else {
        return Err(ScatterPaintError::BadBrush("class (painting needs one)".into()));
    };
    let class = set
        .classes
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| ScatterPaintError::UnknownClass(name.to_owned()))?;
    // A set from a file cannot get here without meshes; one built in code can.
    if class.meshes.is_empty() {
        return Err(ScatterPaintError::BadBrush(format!(
            "class `{name}` has no meshes"
        )));
    }
    let map = inputs.map;
    let half = map.world_width() * 0.5;
    let cell = brush.cell();
    let reach = Vec2::splat(brush.shape.reach());
    let base = inputs.seed
        ^ name_hash(&class.name)
        ^ u64::from(brush.seed).rotate_left(32)
        ^ u64::from(cell.to_bits()).rotate_left(11);

    // Every candidate the path covers, with its best coverage. Walked by cell, so the outcome
    // does not depend on how the path is cut into segments.
    let mut candidates: BTreeMap<(i32, i32), ([f32; DRAWS], f32)> = BTreeMap::new();
    let mut walked = 0u64;
    for (a, b) in ScatterBrush::segments(path) {
        let lo = (a.min(b) - reach).max(Vec2::splat(-half));
        let hi = (a.max(b) + reach).min(Vec2::splat(half));
        if lo.x >= hi.x || lo.y >= hi.y {
            continue;
        }
        let (cx0, cz0) = ((lo.x / cell).floor() as i32, (lo.y / cell).floor() as i32);
        let (cx1, cz1) = ((hi.x / cell).floor() as i32, (hi.y / cell).floor() as i32);
        walked = walked.saturating_add(
            (i64::from(cx1) - i64::from(cx0) + 1) as u64 * (i64::from(cz1) - i64::from(cz0) + 1) as u64,
        );
        if walked > MAX_STROKE_CANDIDATES {
            return Err(ScatterPaintError::TooLarge {
                candidates: walked,
                limit: MAX_STROKE_CANDIDATES,
            });
        }
        for cz in cz0..=cz1 {
            for cx in cx0..=cx1 {
                let (draws, best) = candidates.entry((cx, cz)).or_insert_with(|| {
                    let mut rng = Rng::with_stream(cell_seed(base, cx, cz), 0x5ca7_7e52_0003_0001);
                    let mut draws = [0.0f32; DRAWS];
                    for value in &mut draws {
                        *value = rng.f32();
                    }
                    (draws, 0.0)
                });
                let point = Vec2::new((cx as f32 + draws[0]) * cell, (cz as f32 + draws[1]) * cell);
                *best = best.max(brush.coverage(point, a, b));
            }
        }
    }

    let mut occupied = (brush.min_distance > 0.0).then(|| Occupied::near(store, name, brush, path));
    let total_weight: f32 = class.meshes.iter().map(|m| m.weight).sum();
    let existing = store.class(name);
    let mut placed = Vec::new();
    for (&(cx, cz), (u, coverage)) in &candidates {
        if u[8] >= *coverage * brush.strength {
            continue;
        }
        let (x, z) = ((cx as f32 + u[0]) * cell, (cz as f32 + u[1]) * cell);
        if !(-half..half).contains(&x) || !(-half..half).contains(&z) {
            continue;
        }
        let y = map.sample_bilinear(x, z);
        let normal = map.normal_at(x, z);
        if !brush.ground_passes(y, &normal) || !class_allows(class, inputs, x, y, z, &normal) {
            continue;
        }
        let id = cell_seed(base ^ 0xb5ad_4ece_da1c_e2a9, cx, cz);
        let held = existing
            .and_then(|c| c.tiles.get(&paint_tile(x, z)))
            .is_some_and(|tile| tile.binary_search_by_key(&id, |i| i.id).is_ok());
        if held {
            continue;
        }
        let mesh = pick_mesh(class, u[2] * total_weight);
        let m = &class.meshes[mesh];
        // The deterministic kernel, not the platform libm: a painted store's bytes must be the
        // same on every target (docs/specs/26 § 6, like the scatter-v2 placement).
        let scale = (m.min_scale + (m.max_scale - m.min_scale) * det_powf(u[3], m.scale_bias))
            * (brush.scale[0] + (brush.scale[1] - brush.scale[0]) * u[9]);
        // A file holding any other scale is refused whole, so such an instance must not exist.
        if !(scale.is_finite() && scale > 0.0) {
            continue;
        }
        if let Some(occupied) = &mut occupied
            && !occupied.claim(x, z)
        {
            continue;
        }
        placed.push((
            mesh,
            PaintedInstance {
                id,
                mesh: 0,
                flags: if brush.wind { 0 } else { PAINTED_FLAG_STILL },
                position: [x, y, z],
                normal,
                yaw: u[4] * class.yaw_jitter,
                scale,
                shade: u[7],
                tilt: [u[5], u[6]],
            },
        ));
    }
    let total = store.len().saturating_add(placed.len());
    if total > MAX_PAINTED_INSTANCES {
        return Err(ScatterPaintError::Full {
            total,
            limit: MAX_PAINTED_INSTANCES,
        });
    }
    let mut stroke = ScatterStroke::new(brush);
    if !placed.is_empty() {
        // The whole class's meshes, so the table does not depend on what this stroke drew.
        let mut meshes = existing.map_or_else(Vec::new, |c| c.meshes.clone());
        for m in &class.meshes {
            if !meshes.contains(&m.mesh) {
                meshes.push(m.mesh.clone());
            }
        }
        let slot = |mesh: usize| {
            let path = &class.meshes[mesh].mesh;
            meshes.iter().position(|m| m == path).unwrap_or(0) as u16
        };
        let instances = placed
            .into_iter()
            .map(|(mesh, i)| PaintedInstance {
                mesh: slot(mesh),
                ..i
            })
            .collect();
        stroke.added.push(ScatterDelta {
            class: name.to_owned(),
            meshes,
            instances,
        });
    }
    Ok(stroke)
}

/// Weighted mesh pick, as [`super::placement::place_class_cell`] picks.
fn pick_mesh(class: &ScatterClass, mut pick: f32) -> usize {
    for (i, m) in class.meshes.iter().enumerate() {
        if pick < m.weight {
            return i;
        }
        pick -= m.weight;
    }
    class.meshes.len() - 1
}

/// The class rules a brush does not replace: holes, exclusion zones and the layer mask.
fn class_allows(
    class: &ScatterClass,
    inputs: ScatterPaintInputs<'_>,
    x: f32,
    y: f32,
    z: f32,
    normal: &[f32; 3],
) -> bool {
    if class.avoid_holes
        && let Some(holes) = inputs.holes
        && holes.contains(x, z)
    {
        return false;
    }
    if class.exclusions.iter().any(|zone| zone.contains(x, z)) {
        return false;
    }
    if class.layer_mask != 0
        && let Some(layers) = inputs.layers
    {
        // det_acos, not the platform acos: the slope angle feeds the layer bands, so its last
        // bit decides keep/reject on some ground (docs/specs/26 § 6).
        let weights = layers.weights_in(
            det_acos(normal[1].clamp(-1.0, 1.0)),
            y,
            Some({
                let (lo, hi) = inputs.map.min_max_height();
                [lo, hi]
            }),
        );
        let sum: f32 = weights
            .iter()
            .enumerate()
            .filter(|(i, _)| *i < 8 && class.layer_mask & (1u8 << i) != 0)
            .map(|(_, w)| *w)
            .sum();
        if sum < class.layer_min_weight {
            return false;
        }
    }
    true
}

/// Instance positions on a hash grid of `distance` meters, for the brush's `min_distance`.
struct Occupied {
    distance: f32,
    cells: HashMap<(i32, i32), Vec<[f32; 2]>>,
}

impl Occupied {
    /// The instances of `class` within reach of the stroke.
    fn near(store: &PaintedScatter, class: &str, brush: &ScatterBrush, path: &[Vec2]) -> Self {
        let mut out = Self {
            distance: brush.min_distance,
            cells: HashMap::new(),
        };
        let reach = Vec2::splat(brush.shape.reach() + brush.min_distance);
        let lo = path.iter().fold(Vec2::INFINITY, |a, p| a.min(*p)) - reach;
        let hi = path.iter().fold(Vec2::NEG_INFINITY, |a, p| a.max(*p)) + reach;
        let (tx0, tz0) = paint_tile(lo.x, lo.y);
        let (tx1, tz1) = paint_tile(hi.x, hi.y);
        let tiles = store.class(class).into_iter().flat_map(|c| &c.tiles);
        for (_, tile) in tiles.filter(|((x, z), _)| (tx0..=tx1).contains(x) && (tz0..=tz1).contains(z)) {
            for instance in tile.iter() {
                out.add(instance.position[0], instance.position[2]);
            }
        }
        out
    }

    fn cell(&self, x: f32, z: f32) -> (i32, i32) {
        (
            (x / self.distance).floor() as i32,
            (z / self.distance).floor() as i32,
        )
    }

    fn add(&mut self, x: f32, z: f32) {
        let cell = self.cell(x, z);
        self.cells.entry(cell).or_default().push([x, z]);
    }

    /// Takes the spot, or `false` when something is closer than the distance.
    fn claim(&mut self, x: f32, z: f32) -> bool {
        let (cx, cz) = self.cell(x, z);
        let d2 = self.distance * self.distance;
        for dz in -1..=1 {
            for dx in -1..=1 {
                let Some(list) = self.cells.get(&(cx + dx, cz + dz)) else {
                    continue;
                };
                if list.iter().any(|p| (p[0] - x).powi(2) + (p[1] - z).powi(2) < d2) {
                    return false;
                }
            }
        }
        self.add(x, z);
        true
    }
}

fn erase(
    store: &PaintedScatter,
    set: &ScatterSet,
    brush: &ScatterBrush,
    path: &[Vec2],
) -> Result<ScatterStroke, ScatterPaintError> {
    if let Some(name) = brush.class.as_deref()
        && store.class(name).is_none()
        && !set.classes.iter().any(|c| c.name == name)
    {
        return Err(ScatterPaintError::UnknownClass(name.to_owned()));
    }
    let reach = Vec2::splat(brush.shape.reach());
    let mut stroke = ScatterStroke::new(brush);
    for class in &store.classes {
        if brush.class.as_deref().is_some_and(|name| name != class.name) {
            continue;
        }
        let mut removed = Vec::new();
        for (a, b) in ScatterBrush::segments(path) {
            let lo = a.min(b) - reach;
            let hi = a.max(b) + reach;
            let (tx0, tz0) = paint_tile(lo.x, lo.y);
            let (tx1, tz1) = paint_tile(hi.x, hi.y);
            for (&(tx, tz), tile) in &class.tiles {
                if tx < tx0 || tx > tx1 || tz < tz0 || tz > tz1 {
                    continue;
                }
                for instance in tile.iter() {
                    let [x, y, z] = instance.position;
                    let coverage = brush.coverage(Vec2::new(x, z), a, b) * brush.strength;
                    if coverage <= 0.0 || !brush.ground_passes(y, &instance.normal) {
                        continue;
                    }
                    // The instance's own draw: the same brush thins the same instances.
                    let mut rng = Rng::with_stream(
                        cell_seed(instance.id ^ u64::from(brush.seed), 0, 0),
                        0x5ca7_7e52_0003_0002,
                    );
                    if rng.f32() < coverage {
                        removed.push(*instance);
                    }
                }
            }
        }
        removed.sort_by_key(|i| i.id);
        removed.dedup_by_key(|i| i.id);
        if !removed.is_empty() {
            stroke.removed.push(ScatterDelta {
                class: class.name.clone(),
                meshes: class.meshes.clone(),
                instances: removed,
            });
        }
    }
    Ok(stroke)
}

/// A terrain's painted instances.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct TerrainScatterPaint(pub PaintedScatter);

/// The scatter set a brush paints from, for a host that draws its field itself. Without it the
/// brush uses the [`RuntimeScatter`] field's set.
#[derive(Resource, Clone, Debug)]
pub struct ScatterPaintSet(pub Arc<ScatterSet>);

/// The scatter set a brush paints from: [`ScatterPaintSet`], else the [`RuntimeScatter`] field's.
#[must_use]
pub fn paintable_scatter_set(world: &World) -> Option<Arc<ScatterSet>> {
    world
        .get_resource::<ScatterPaintSet>()
        .map(|s| Arc::clone(&s.0))
        .or_else(|| {
            world
                .get_resource::<RuntimeScatter>()
                .map(|scatter| Arc::new(scatter.set()))
        })
}

/// Runs one stroke on a terrain's painted instances, over its map, holes and layers (see
/// [`scatter_stroke`]), records the edit in `edits` and returns the stroke's journal entry.
///
/// # Errors
///
/// As [`scatter_stroke`].
pub fn apply_scatter_stroke(
    terrain: &Terrain,
    paint: &mut TerrainScatterPaint,
    edits: &mut TerrainEdits,
    set: &ScatterSet,
    brush: &ScatterBrush,
    path: &[Vec2],
) -> Result<StrokeUndo, ScatterPaintError> {
    let inputs = ScatterPaintInputs {
        map: &terrain.map,
        holes: terrain.holes.as_ref(),
        layers: Some(&terrain.layers),
        seed: set.seed,
    };
    let stroke = scatter_stroke(&mut paint.0, set, inputs, brush, path)?;
    if !stroke.is_empty() {
        edits.note_scatter(stroke.dirty);
    }
    Ok(StrokeUndo::Scatter(stroke))
}

/// Re-seats `paint` on what the terrain's last height edits moved (a sculpt, an undone sculpt, a
/// height import) and on what a stroke's undo brought back.
pub fn reseat_terrain_scatter(terrain: &Terrain, paint: &mut TerrainScatterPaint, edits: &mut TerrainEdits) {
    let Some(rect) = edits.take_scatter_reseat() else {
        return;
    };
    let _span = tracing::info_span!(target: "regia_terrain", "scatter.reseat").entered();
    if !paint.0.is_empty() && paint.0.reseat(&terrain.map, Some(rect)) > 0 {
        edits.mark_scatter_unsaved();
    }
}

/// `Update`: [`reseat_terrain_scatter`] on every painted terrain with a pending edit.
pub fn reseat_painted_scatter(mut terrains: Query<(&Terrain, &mut TerrainScatterPaint, &mut TerrainEdits)>) {
    for (terrain, mut paint, mut edits) in &mut terrains {
        if edits.scatter_reseat().is_some() {
            reseat_terrain_scatter(terrain, &mut paint, &mut edits);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scatter::set::{ScatterExclusion, ScatterMeshRef};

    fn flat() -> TerrainMap {
        TerrainMap::from_heights(129, 129, vec![10.0; 129 * 129], 256.0, 1.0, 0.0).expect("map")
    }

    /// Flat, with a steep wall past x = 40.
    fn walled() -> TerrainMap {
        let n = 129usize;
        let heights = (0..n * n)
            .map(|i| {
                let x = (i % n) as f32 / (n - 1) as f32 * 256.0 - 128.0;
                10.0 + (x - 40.0).max(0.0) * 2.0
            })
            .collect();
        TerrainMap::from_heights(n as u32, n as u32, heights, 256.0, 1.0, 0.0).expect("map")
    }

    /// A gentle uniform slope: every ground normal is tilted, so the layer-mask slope angle
    /// (det_acos) is exercised on every candidate.
    fn sloped() -> TerrainMap {
        let n = 129usize;
        let heights = (0..n * n)
            .map(|i| {
                let x = (i % n) as f32 / (n - 1) as f32 * 256.0 - 128.0;
                10.0 + x * 0.05
            })
            .collect();
        TerrainMap::from_heights(n as u32, n as u32, heights, 256.0, 1.0, 0.0).expect("map")
    }

    /// One grass layer that covers the whole slope (its band sits well inside the map's slope
    /// angle), so the layer path keeps every candidate and pins the det_acos slope math.
    fn slope_layers() -> TerrainLayers {
        let mut layers = TerrainLayers::default();
        if let Some(first) = layers.layers.first_mut() {
            first.slope_min = 0.0;
            first.slope_max = 0.3;
            first.altitude_min = f32::NEG_INFINITY;
            first.altitude_max = f32::INFINITY;
        }
        layers
    }

    /// A class whose scale bias is not 1: the instance scales come from det_powf.
    fn biased_class(name: &str) -> ScatterClass {
        ScatterClass {
            name: name.into(),
            density: 0.0,
            meshes: vec![ScatterMeshRef {
                mesh: "tree_a.glb".into(),
                weight: 1.0,
                min_scale: 0.8,
                max_scale: 1.2,
                scale_bias: 0.184,
            }],
            layer_mask: 0x1,
            layer_min_weight: 0.25,
            slope_max: 1.5,
            ..ScatterClass::default()
        }
    }

    fn biased_set() -> ScatterSet {
        ScatterSet {
            classes: vec![biased_class("trees")],
            ..ScatterSet::default()
        }
    }

    fn sloped_inputs<'a>(map: &'a TerrainMap, layers: &'a TerrainLayers) -> ScatterPaintInputs<'a> {
        ScatterPaintInputs {
            map,
            holes: None,
            layers: Some(layers),
            seed: 7,
        }
    }

    fn class(name: &str) -> ScatterClass {
        ScatterClass {
            name: name.into(),
            density: 0.0,
            meshes: vec![
                ScatterMeshRef {
                    mesh: "tree_a.glb".into(),
                    weight: 3.0,
                    min_scale: 0.8,
                    max_scale: 1.2,
                    scale_bias: 1.0,
                },
                ScatterMeshRef {
                    mesh: "tree_b.glb".into(),
                    weight: 1.0,
                    min_scale: 2.0,
                    max_scale: 2.0,
                    scale_bias: 1.0,
                },
            ],
            slope_max: 1.5,
            ..ScatterClass::default()
        }
    }

    fn set() -> ScatterSet {
        ScatterSet {
            classes: vec![class("trees"), class("bushes")],
            ..ScatterSet::default()
        }
    }

    fn inputs(map: &TerrainMap) -> ScatterPaintInputs<'_> {
        ScatterPaintInputs {
            map,
            holes: None,
            layers: None,
            seed: 7,
        }
    }

    fn brush(radius: f32, density: f32) -> ScatterBrush {
        ScatterBrush {
            shape: BrushShape::Circle { radius },
            hardness: 1.0,
            density,
            ..ScatterBrush::for_class(&class("trees"))
        }
    }

    fn at(x: f32, z: f32) -> Vec<Vec2> {
        vec![Vec2::new(x, z)]
    }

    fn stroke(
        store: &mut PaintedScatter,
        map: &TerrainMap,
        b: &ScatterBrush,
        path: &[Vec2],
    ) -> ScatterStroke {
        scatter_stroke(store, &set(), inputs(map), b, path).expect("stroke")
    }

    /// docs/specs/26 § 6: a painted stroke's store bytes are pinned. The placement routes the
    /// scale bias through `det_powf` and the layer-mask slope through `det_acos`, so the bytes
    /// are identical on every target; a platform `powf`/`acos` in either moves them and fails
    /// here. The pin is the det-kernel output (a `scale_bias` of 0.184, a tilted ground with the
    /// grass band inside its slope angle), as a FNV-1a digest of the whole serialized store.
    #[test]
    fn painted_placement_bytes_are_pinned_for_every_platform() {
        let map = sloped();
        let layers = slope_layers();
        let mut store = PaintedScatter::new();
        let b = ScatterBrush {
            shape: BrushShape::Circle { radius: 6.0 },
            hardness: 1.0,
            density: 1.0,
            ..ScatterBrush::for_class(&biased_class("trees"))
        };
        let stroke = scatter_stroke(
            &mut store,
            &biased_set(),
            sloped_inputs(&map, &layers),
            &b,
            &at(0.0, 0.0),
        )
        .expect("stroke");
        assert!(!stroke.added.is_empty(), "the stroke placed nothing");
        let bytes = store.to_bytes().expect("bytes");
        // FNV-1a over the store bytes: any bit the placement writes differently moves the digest.
        let mut digest = 0xcbf2_9ce4_8422_2325u64;
        for byte in &bytes {
            digest ^= u64::from(*byte);
            digest = digest.wrapping_mul(0x0000_0100_0000_01b3);
        }
        assert_eq!(
            digest,
            278_415_888_245_974_528,
            "painted placement bytes moved ({} instances): platform libm in the placement?",
            store.len()
        );
    }

    #[test]
    fn density_hits_its_target_inside_the_brush() {
        let map = flat();
        let mut store = PaintedScatter::new();
        let b = brush(50.0, 0.5);
        stroke(&mut store, &map, &b, &at(0.0, 0.0));
        let area = std::f32::consts::PI * 50.0 * 50.0;
        let expected = area * 0.5;
        let got = store.len() as f32;
        assert!((got - expected).abs() < expected * 0.03, "{got} vs {expected}");
        for i in store.instances("trees") {
            assert!(i.position[0].hypot(i.position[2]) < 50.0, "{i:?}");
            assert_eq!(i.position[1], 10.0);
        }
    }

    #[test]
    fn strength_scales_the_density() {
        let map = flat();
        let mut half = PaintedScatter::new();
        let mut full = PaintedScatter::new();
        let b = brush(100.0, 1.0);
        stroke(&mut full, &map, &b, &at(0.0, 0.0));
        stroke(
            &mut half,
            &map,
            &ScatterBrush {
                strength: 0.5,
                ..b.clone()
            },
            &at(0.0, 0.0),
        );
        let ratio = half.len() as f32 / full.len() as f32;
        assert!((ratio - 0.5).abs() < 0.03, "{ratio}");
    }

    #[test]
    fn a_soft_brush_thins_toward_its_edge() {
        let map = flat();
        let mut store = PaintedScatter::new();
        let b = ScatterBrush {
            hardness: 0.0,
            ..brush(60.0, 1.0)
        };
        stroke(&mut store, &map, &b, &at(0.0, 0.0));
        let ring = |lo: f32, hi: f32| {
            store
                .instances("trees")
                .filter(|i| (lo..hi).contains(&i.position[0].hypot(i.position[2])))
                .count() as f32
                / (std::f32::consts::PI * (hi * hi - lo * lo))
        };
        let (inner, outer) = (ring(0.0, 20.0), ring(40.0, 60.0));
        assert!(inner > outer * 2.0, "{inner} vs {outer}");
    }

    #[test]
    fn the_same_stroke_is_the_same_bytes_and_painting_again_adds_nothing() {
        let map = flat();
        let (mut a, mut b) = (PaintedScatter::new(), PaintedScatter::new());
        let br = brush(40.0, 0.3);
        let first = stroke(&mut a, &map, &br, &at(5.0, -3.0));
        stroke(&mut b, &map, &br, &at(5.0, -3.0));
        assert!(!first.is_empty());
        assert_eq!(a.to_bytes().expect("a"), b.to_bytes().expect("b"));
        let again = stroke(&mut a, &map, &br, &at(5.0, -3.0));
        assert!(again.is_empty(), "density is a target: {}", again.len());
        assert_eq!(a.to_bytes().expect("a"), b.to_bytes().expect("b"));
    }

    #[test]
    fn another_seed_paints_other_instances() {
        let map = flat();
        let (mut a, mut b) = (PaintedScatter::new(), PaintedScatter::new());
        let br = brush(30.0, 0.3);
        stroke(&mut a, &map, &br, &at(0.0, 0.0));
        stroke(&mut b, &map, &ScatterBrush { seed: 1, ..br }, &at(0.0, 0.0));
        assert_ne!(a, b);
    }

    #[test]
    fn a_drag_cut_into_pieces_matches_the_whole_drag() {
        let map = flat();
        let path = [
            Vec2::new(-40.0, 0.0),
            Vec2::new(-10.0, 12.0),
            Vec2::new(20.0, -8.0),
            Vec2::new(50.0, 4.0),
        ];
        let b = ScatterBrush {
            hardness: 0.3,
            ..brush(12.0, 0.6)
        };
        let mut whole = PaintedScatter::new();
        let one = stroke(&mut whole, &map, &b, &path);
        let mut parts = PaintedScatter::new();
        let mut merged = stroke(&mut parts, &map, &b, &path[..1]);
        for pair in path.windows(2) {
            merged.merge(stroke(&mut parts, &map, &b, pair));
        }
        assert_eq!(whole.to_bytes().expect("whole"), parts.to_bytes().expect("parts"));
        assert_eq!(one.len(), merged.len());
        assert_eq!(one.tiles, merged.tiles);
        // The merged record undoes the whole drag.
        merged.restore(&mut parts, true);
        assert!(parts.is_empty());
    }

    #[test]
    fn undo_and_redo_restore_the_bytes() {
        let map = flat();
        let mut store = PaintedScatter::new();
        stroke(&mut store, &map, &brush(30.0, 0.3), &at(-20.0, 0.0));
        let before = store.to_bytes().expect("before");
        let paint = stroke(&mut store, &map, &brush(30.0, 0.6), &at(20.0, 0.0));
        let painted = store.to_bytes().expect("painted");
        paint.restore(&mut store, true);
        assert_eq!(store.to_bytes().expect("undone"), before);
        paint.restore(&mut store, false);
        assert_eq!(store.to_bytes().expect("redone"), painted);

        let erase = stroke(
            &mut store,
            &map,
            &ScatterBrush::eraser(None).with_radius(35.0),
            &at(0.0, 0.0),
        );
        assert!(!erase.removed.is_empty());
        erase.restore(&mut store, true);
        assert_eq!(store.to_bytes().expect("erase undone"), painted);
    }

    impl ScatterBrush {
        fn with_radius(mut self, radius: f32) -> Self {
            self.shape = BrushShape::Circle { radius };
            self.hardness = 1.0;
            self
        }
    }

    #[test]
    fn undoing_the_first_stroke_matches_a_store_never_painted() {
        let map = flat();
        let mut store = PaintedScatter::new();
        let empty = store.to_bytes().expect("empty");
        let record = stroke(&mut store, &map, &brush(30.0, 0.3), &at(0.0, 0.0));
        record.restore(&mut store, true);
        assert_eq!(store.to_bytes().expect("undone"), empty);
        assert_eq!(store, PaintedScatter::new());
    }

    #[test]
    fn slope_and_height_filters_reject_what_they_should() {
        let map = walled();
        let mut gentle = PaintedScatter::new();
        let b = ScatterBrush {
            slope: [0.0, 0.3],
            ..brush(100.0, 0.5)
        };
        stroke(&mut gentle, &map, &b, &at(30.0, 0.0));
        assert!(gentle.len() > 100);
        assert!(
            gentle
                .instances("trees")
                .all(|i| i.normal[1] >= 0.3f32.cos() - 1e-3),
            "slope_max"
        );
        assert!(gentle.instances("trees").all(|i| i.position[0] < 42.0));

        let mut steep = PaintedScatter::new();
        let b = ScatterBrush {
            slope: [0.9, 1.5],
            ..brush(100.0, 0.5)
        };
        stroke(&mut steep, &map, &b, &at(30.0, 0.0));
        assert!(steep.len() > 100);
        assert!(
            steep.instances("trees").all(|i| i.position[0] > 38.0),
            "slope_min"
        );

        let mut low = PaintedScatter::new();
        let b = ScatterBrush {
            height: [0.0, 12.0],
            ..brush(100.0, 0.5)
        };
        stroke(&mut low, &map, &b, &at(30.0, 0.0));
        assert!(low.instances("trees").all(|i| i.position[1] <= 12.0));
    }

    #[test]
    fn scale_range_multiplies_the_class_scale() {
        let map = flat();
        let mut store = PaintedScatter::new();
        let b = ScatterBrush {
            scale: [2.0, 3.0],
            ..brush(60.0, 0.5)
        };
        stroke(&mut store, &map, &b, &at(0.0, 0.0));
        let (mut lo, mut hi) = (f32::MAX, 0.0f32);
        for i in store.instances("trees") {
            let path = store.mesh_path("trees", i).expect("path");
            let (a, z) = if path == "tree_a.glb" {
                (0.8, 1.2)
            } else {
                (2.0, 2.0)
            };
            assert!(
                i.scale >= a * 2.0 - 1e-3 && i.scale <= z * 3.0 + 1e-3,
                "{}",
                i.scale
            );
            lo = lo.min(i.scale);
            hi = hi.max(i.scale);
        }
        assert!(hi > lo * 1.5, "{lo}..{hi}");
    }

    #[test]
    fn meshes_follow_their_weights() {
        let map = flat();
        let mut store = PaintedScatter::new();
        stroke(&mut store, &map, &brush(80.0, 1.0), &at(0.0, 0.0));
        let a = store
            .instances("trees")
            .filter(|i| store.mesh_path("trees", i) == Some("tree_a.glb"))
            .count() as f32;
        let share = a / store.len() as f32;
        assert!((share - 0.75).abs() < 0.03, "{share}");
    }

    #[test]
    fn min_distance_holds_across_strokes_and_classes_stay_apart() {
        let map = flat();
        let mut store = PaintedScatter::new();
        let b = ScatterBrush {
            min_distance: 3.0,
            ..brush(40.0, 1.0)
        };
        stroke(&mut store, &map, &b, &at(0.0, 0.0));
        stroke(
            &mut store,
            &map,
            &ScatterBrush { seed: 9, ..b.clone() },
            &at(5.0, 5.0),
        );
        let list: Vec<_> = store.instances("trees").copied().collect();
        assert!(list.len() > 50);
        for (n, a) in list.iter().enumerate() {
            for b in &list[n + 1..] {
                let d = (a.position[0] - b.position[0]).hypot(a.position[2] - b.position[2]);
                assert!(d >= 3.0 - 1e-3, "{d}");
            }
        }
        let other = ScatterBrush {
            class: Some("bushes".into()),
            ..b
        };
        let added = stroke(&mut store, &map, &other, &at(0.0, 0.0));
        assert!(added.len() > 50, "another class does not collide");
    }

    #[test]
    fn class_rules_apply_to_a_brush() {
        let map = flat();
        let mut with_zone = class("trees");
        with_zone.exclusions = vec![ScatterExclusion {
            points: vec![[0.0, 0.0]],
            radius: 20.0,
        }];
        let set = ScatterSet {
            classes: vec![with_zone],
            ..ScatterSet::default()
        };
        let holes = HolesMask::new(vec![crate::holes::HoleEllipse {
            center: [30.0, 0.0],
            radius: [8.0, 8.0],
            rotation_radians: 0.0,
        }])
        .expect("holes");
        let mut store = PaintedScatter::new();
        let input = ScatterPaintInputs {
            holes: Some(&holes),
            ..inputs(&map)
        };
        scatter_stroke(&mut store, &set, input, &brush(60.0, 1.0), &at(0.0, 0.0)).expect("stroke");
        assert!(store.len() > 100);
        for i in store.instances("trees") {
            let [x, _, z] = i.position;
            assert!(x.hypot(z) > 20.0, "exclusion");
            assert!(!holes.contains(x, z), "hole");
        }
    }

    #[test]
    fn erase_removes_what_the_brush_covers_and_filters_it() {
        let map = walled();
        let mut store = PaintedScatter::new();
        stroke(&mut store, &map, &brush(80.0, 0.5), &at(30.0, 0.0));
        let total = store.len();
        let eraser = ScatterBrush::eraser(Some("trees")).with_radius(20.0);
        stroke(&mut store, &map, &eraser, &at(0.0, 0.0));
        assert!(store.len() < total);
        assert!(
            store
                .instances("trees")
                .all(|i| i.position[0].hypot(i.position[2]) >= 20.0)
        );

        // A filtered eraser leaves the flat ground.
        let before = store.instances("trees").filter(|i| i.position[0] < 38.0).count();
        let steep = ScatterBrush {
            slope: [0.9, 1.5],
            ..ScatterBrush::eraser(None).with_radius(200.0)
        };
        stroke(&mut store, &map, &steep, &at(30.0, 0.0));
        assert_eq!(
            store.instances("trees").filter(|i| i.position[0] < 38.0).count(),
            before
        );
        assert!(store.instances("trees").all(|i| i.position[0] <= 42.0));
    }

    #[test]
    fn a_partial_eraser_thins() {
        let map = flat();
        let mut store = PaintedScatter::new();
        stroke(&mut store, &map, &brush(60.0, 1.0), &at(0.0, 0.0));
        let total = store.len() as f32;
        let eraser = ScatterBrush {
            strength: 0.5,
            ..ScatterBrush::eraser(None).with_radius(100.0)
        };
        stroke(&mut store, &map, &eraser, &at(0.0, 0.0));
        let ratio = store.len() as f32 / total;
        assert!((ratio - 0.5).abs() < 0.05, "{ratio}");
    }

    #[test]
    fn bad_input_changes_nothing() {
        let map = flat();
        let mut store = PaintedScatter::new();
        let run = |store: &mut PaintedScatter, b: &ScatterBrush, path: &[Vec2]| {
            scatter_stroke(store, &set(), inputs(&map), b, path)
        };
        assert!(matches!(
            run(&mut store, &brush(5.0, 1.0), &[]),
            Err(ScatterPaintError::BadPath)
        ));
        assert!(matches!(
            run(&mut store, &brush(5.0, 1.0), &[Vec2::new(f32::NAN, 0.0)]),
            Err(ScatterPaintError::BadPath)
        ));
        let bad = [
            ScatterBrush {
                density: 0.0,
                ..brush(5.0, 1.0)
            },
            ScatterBrush {
                density: 1e9,
                ..brush(5.0, 1.0)
            },
            ScatterBrush {
                density: f32::NAN,
                ..brush(5.0, 1.0)
            },
            ScatterBrush {
                scale: [2.0, 1.0],
                ..brush(5.0, 1.0)
            },
            ScatterBrush {
                scale: [0.0, 1.0],
                ..brush(5.0, 1.0)
            },
            ScatterBrush {
                slope: [1.0, 0.5],
                ..brush(5.0, 1.0)
            },
            ScatterBrush {
                height: [1.0, f32::INFINITY],
                ..brush(5.0, 1.0)
            },
            ScatterBrush {
                strength: 2.0,
                ..brush(5.0, 1.0)
            },
            ScatterBrush {
                min_distance: -1.0,
                ..brush(5.0, 1.0)
            },
            brush(1e7, 1.0),
            ScatterBrush {
                class: None,
                ..brush(5.0, 1.0)
            },
        ];
        for b in &bad {
            assert_eq!(
                run(&mut store, b, &at(0.0, 0.0)).expect_err("refused").code(),
                "scatter_paint_bad_brush",
                "{b:?}"
            );
        }
        // A host can hand the brush a set that was built in code and never validated.
        let meshless = ScatterSet {
            classes: vec![ScatterClass {
                name: "trees".into(),
                ..ScatterClass::default()
            }],
            ..ScatterSet::default()
        };
        assert_eq!(
            scatter_stroke(
                &mut store,
                &meshless,
                inputs(&map),
                &brush(5.0, 1.0),
                &at(0.0, 0.0)
            )
            .expect_err("refused")
            .code(),
            "scatter_paint_bad_brush"
        );
        let unknown = ScatterBrush {
            class: Some("nope".into()),
            ..brush(5.0, 1.0)
        };
        assert_eq!(
            run(&mut store, &unknown, &at(0.0, 0.0))
                .expect_err("refused")
                .code(),
            "scatter_paint_unknown_class"
        );
        let huge = brush(4000.0, 64.0);
        let big = TerrainMap::from_heights(65, 65, vec![0.0; 65 * 65], 8000.0, 1.0, 0.0).expect("map");
        let err =
            scatter_stroke(&mut store, &set(), inputs(&big), &huge, &at(0.0, 0.0)).expect_err("refused");
        assert_eq!(err.code(), "scatter_paint_too_large");
        assert_eq!(store, PaintedScatter::new());
        assert_eq!(store.generation(), 0, "nothing was touched");
    }

    /// `f32::MAX` times the larger mesh scales overflows; times the scales below 1 it does not.
    fn overflowing(seed: u32) -> ScatterBrush {
        ScatterBrush {
            scale: [f32::MAX, f32::MAX],
            seed,
            ..brush(20.0, 0.5)
        }
    }

    #[test]
    fn a_scale_past_the_float_range_is_dropped_and_the_file_stays_readable() {
        let map = flat();
        let mut store = PaintedScatter::new();
        assert!(!stroke(&mut store, &map, &overflowing(1), &at(0.0, 0.0)).is_empty());
        let bytes = store.to_bytes().expect("bytes");
        assert_eq!(PaintedScatter::from_bytes(&bytes).expect("still loads"), store);
    }

    #[test]
    fn a_dropped_candidate_holds_no_min_distance_spot() {
        let map = flat();
        for seed in 0..32 {
            let spaced = ScatterBrush {
                min_distance: 64.0,
                ..overflowing(seed)
            };
            let mut store = PaintedScatter::new();
            let placed = stroke(&mut store, &map, &spaced, &at(0.0, 0.0));
            assert_eq!(placed.len(), 1, "seed {seed}: the first usable candidate lands");
        }
    }

    #[test]
    fn strokes_off_the_map_place_nothing() {
        let map = flat();
        let mut store = PaintedScatter::new();
        let record = stroke(&mut store, &map, &brush(10.0, 1.0), &at(1000.0, 0.0));
        assert!(record.is_empty());
        // Half on: everything stays on the map.
        stroke(&mut store, &map, &brush(10.0, 1.0), &at(128.0, 128.0));
        assert!(!store.is_empty());
        assert!(
            store
                .instances("trees")
                .all(|i| i.position[0] < 128.0 && i.position[2] < 128.0)
        );
    }

    #[test]
    fn tiles_hold_their_instances_and_a_clone_shares_untouched_tiles() {
        let map = flat();
        let mut store = PaintedScatter::new();
        stroke(&mut store, &map, &brush(100.0, 0.2), &at(0.0, 0.0));
        for ((tx, tz), tile) in &store.class("trees").expect("class").tiles {
            for i in tile.iter() {
                assert_eq!(paint_tile(i.position[0], i.position[2]), (*tx, *tz));
            }
            assert!(tile.windows(2).all(|w| w[0].id < w[1].id), "sorted by id");
        }
        let snapshot = store.clone();
        assert!(store.changed_tiles(&snapshot, "trees").is_empty());
        stroke(
            &mut store,
            &map,
            &ScatterBrush {
                seed: 2,
                ..brush(5.0, 2.0)
            },
            &at(100.0, 100.0),
        );
        let changed = store.changed_tiles(&snapshot, "trees");
        assert!(!changed.is_empty());
        assert!(changed.iter().all(|&(x, z)| x >= 1 && z >= 1), "{changed:?}");
        assert!(snapshot.len() < store.len(), "the snapshot did not change");
    }

    #[test]
    fn a_sculpt_reseats_only_what_it_moved() {
        let mut map = flat();
        let mut store = PaintedScatter::new();
        let b = brush(100.0, 0.2);
        stroke(&mut store, &map, &b, &at(0.0, 0.0));
        let snapshot = store.clone();
        let generation = store.generation();
        assert_eq!(store.reseat(&map, None), 0);
        assert_eq!(store.generation(), generation, "nothing moved");

        let raised = crate::brush::stroke_heights(
            &mut map,
            &Brush {
                shape: BrushShape::Circle { radius: 16.0 },
                mode: crate::brush::BrushMode::Raise { amount: 5.0 },
                hardness: 1.0,
                ..Brush::default()
            },
            &at(32.0, 32.0),
            EDIT_CELL_SIZE,
        );
        let moved = store.reseat(&map, Some(raised.dirty));
        assert!(moved > 0);
        for i in store.instances("trees") {
            let [x, y, z] = i.position;
            assert_eq!(y, map.sample_bilinear(x, z));
            assert_eq!(i.normal, map.normal_at(x, z));
        }
        let changed = store.changed_tiles(&snapshot, "trees");
        assert!(changed.len() <= 4, "{changed:?}");

        // The undone sculpt puts every instance back, bit for bit.
        let dirty = raised.undo(&mut map);
        store.reseat(&map, Some(dirty));
        assert_eq!(store, snapshot);
    }

    #[test]
    fn flags_survive_a_save_and_an_undo() {
        let map = flat();
        let mut store = PaintedScatter::new();
        let mut record = stroke(&mut store, &map, &brush(20.0, 1.0), &at(0.0, 0.0));
        record.added[0].instances[0].flags = 0x8001;
        record.restore(&mut store, true);
        record.restore(&mut store, false);
        let loaded = PaintedScatter::from_bytes(&store.to_bytes().expect("save")).expect("load");
        assert_eq!(loaded, store);
        assert_eq!(loaded.instances("trees").filter(|i| i.flags == 0x8001).count(), 1);
    }

    #[test]
    fn the_file_round_trips_and_a_load_is_a_new_generation() {
        let map = flat();
        let mut store = PaintedScatter::new();
        stroke(&mut store, &map, &brush(40.0, 0.5), &at(-90.0, 90.0));
        stroke(
            &mut store,
            &map,
            &ScatterBrush {
                class: Some("bushes".into()),
                ..brush(40.0, 0.5)
            },
            &at(10.0, 0.0),
        );
        let bytes = store.to_bytes().expect("save");
        assert_eq!(&bytes[..4], SCATTER_PAINT_MAGIC);
        assert_eq!(bytes[4..8], SCATTER_PAINT_FORMAT_VERSION.to_le_bytes());
        let loaded = PaintedScatter::from_bytes(&bytes).expect("load");
        assert_eq!(loaded, store);
        assert_ne!(loaded.generation(), store.generation());
        assert_eq!(loaded.to_bytes().expect("again"), bytes);
        assert_eq!(loaded.report().len(), 2);
    }

    #[test]
    fn broken_files_are_refused() {
        let map = flat();
        let mut store = PaintedScatter::new();
        stroke(&mut store, &map, &brush(40.0, 0.5), &at(0.0, 0.0));
        let bytes = store.to_bytes().expect("save");
        assert!(matches!(
            PaintedScatter::from_bytes(&bytes[..3]),
            Err(ScatterPaintError::Decode(_))
        ));
        assert!(matches!(
            PaintedScatter::from_bytes(&bytes[..6]),
            Err(ScatterPaintError::Decode(_))
        ));
        assert!(matches!(
            PaintedScatter::from_bytes(&bytes[..bytes.len() - 3]),
            Err(ScatterPaintError::Decode(_))
        ));
        let mut magic = bytes.clone();
        magic[0] = b'X';
        assert!(matches!(
            PaintedScatter::from_bytes(&magic),
            Err(ScatterPaintError::Decode(_))
        ));
        let mut newer = bytes.clone();
        newer[4] = 9;
        assert!(matches!(
            PaintedScatter::from_bytes(&newer),
            Err(ScatterPaintError::BadVersion { found: 9, .. })
        ));

        let rewrite = |edit: &dyn Fn(&mut PaintFileTile)| {
            let mut file: PaintFile = postcard::from_bytes(&bytes[8..]).expect("body");
            edit(&mut file.classes[0].tiles[0]);
            let mut out = bytes[..8].to_vec();
            out.extend(postcard::to_allocvec(&file).expect("encode"));
            PaintedScatter::from_bytes(&out)
        };
        let cases: [&dyn Fn(&mut PaintFileTile); 6] = [
            &|t| t.instances[0].mesh = 99,
            &|t| t.instances[0].scale = f32::NAN,
            &|t| t.instances[0].position[0] = 1000.0,
            &|t| t.instances[0].shade = 2.0,
            &|t| t.instances.clear(),
            &|t| t.instances.push(t.instances[0]),
        ];
        for edit in cases {
            assert!(matches!(rewrite(edit), Err(ScatterPaintError::Decode(_))));
        }
        assert!(rewrite(&|_| {}).is_ok());
    }

    #[test]
    fn paths_name_the_map() {
        let path = |p: &str| scatter_paint_path(Path::new(p));
        assert_eq!(
            path("maps/arena.level.ron"),
            Path::new("maps/arena.scatterpaint.bin")
        );
        assert_eq!(path("maps/arena"), Path::new("maps/arena.scatterpaint.bin"));
    }

    #[test]
    fn a_missing_file_is_none_and_a_broken_one_an_error() {
        let dir = regia_test_utils::unique_temp_dir("scatter-paint-load");
        let file = dir.join("arena.scatterpaint.bin");
        assert!(load_scatter_paint(&file).expect("missing").is_none());
        std::fs::write(&file, b"junk").expect("write");
        assert_eq!(
            load_scatter_paint(&file).expect_err("junk").code(),
            "scatter_paint_decode"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
