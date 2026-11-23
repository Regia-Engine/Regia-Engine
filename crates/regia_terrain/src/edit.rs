// SPDX-License-Identifier: MPL-2.0
//! Terrain editing glue: brush strokes on the ECS [`Terrain`], the live GPU update, per-stroke
//! undo records and the incremental re-cook of the touched tiles.
//!
//! The flow for one stroke (editor tool or the `terrain.stroke` agent method):
//!
//! 1. [`apply_stroke`] (or batches from a [`crate::brush::BrushThread`]) edits the
//!    [`Terrain::map`] / [`TerrainPaint`] in place.
//! 2. [`TerrainEdits::note`] records the changed [`DirtyRect`] and cook tiles and refreshes the
//!    CPU [`crate::MinMaxPyramid`] over that rectangle.
//! 3. A linked draw gets the rectangle through the height-patch queue
//!    ([`crate::runtime_patch::forward_terrain_strokes`]). A host that owns a
//!    [`crate::gpu::TerrainGpu`] directly calls [`TerrainEdits::take_gpu_dirty`] each frame and
//!    passes it to [`crate::gpu::TerrainGpu::update_heights`]: the same dirty-rect re-upload
//!    craters use ([`crate::deform`]).
//! 4. On save, [`recook_tiles`] re-cooks only [`TerrainEdits::take_unsaved_tiles`] into the
//!    cooked tiled heightfield; writing the `.fckd` triggers the asset hot-reload watcher.
//!
//! The stroke's [`StrokeUndo`] restores the edited samples bit for bit.

use std::collections::BTreeSet;

use glam::Vec2;
use regia_app::prelude::*;
use regia_scene::streaming::{
    OriginOffset, RebaseBlockers, RebaseHolds, TERRAIN_STROKE_BLOCKER, acquire_rebase_hold,
    release_rebase_hold,
};

use crate::Terrain;
use crate::brush::{self, Brush, BrushStroke, BrushTarget, StrokeRecord, TileCoord, union_rect};
use crate::cdlod::MinMaxPyramid;
use crate::deform::DirtyRect;
use crate::layer_paint::LayerWeightMap;
use crate::sampler::TerrainMap;
use crate::scatter::painted::{ScatterStroke, TerrainScatterPaint};
use crate::stream::{CookedHeightfieldTile, CookedTiledHeightfield};

/// Edge length, in samples, of the edit cells strokes record as touched. Saves map cells to
/// cook tiles ([`recook_tiles`]); any cook tile size that is a multiple of this (Regia Engine's are
/// powers of two ≥ 32) re-cooks exactly the touched tiles.
pub const EDIT_CELL_SIZE: u32 = 32;

/// Painted layer weights of a terrain (the brush's layer-weight target).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct TerrainPaint(pub LayerWeightMap);

/// Edits waiting for the GPU upload and for the next save.
#[derive(Component, Debug, Clone, Default, PartialEq, Eq)]
pub struct TerrainEdits {
    /// Height samples changed since the last GPU upload.
    gpu_dirty: Option<DirtyRect>,
    /// Height samples a linked draw has not been sent yet.
    draw_dirty: Option<DirtyRect>,
    /// Painted weight samples changed since the last GPU upload.
    paint_dirty: Option<DirtyRect>,
    /// Edit cells ([`EDIT_CELL_SIZE`]) changed since the last save.
    unsaved_tiles: BTreeSet<TileCoord>,
    /// Painted weights changed since the last save.
    unsaved_paint: bool,
    /// Heights or painted instances changed since painted instances were last re-seated.
    scatter_reseat: Option<DirtyRect>,
    /// Painted instances changed since the last save.
    unsaved_scatter: bool,
    /// Bumped on every edit (cache keys for CPU-side meshes of the terrain).
    generation: u64,
    /// Height samples changed since the last [`take_height_changes`](Self::take_height_changes).
    /// Separate from `gpu_dirty`: the upload clears that one before readers of the CPU map look.
    height_changes: Option<DirtyRect>,
}

impl TerrainEdits {
    /// Records a height edit over `rect` touching edit cells `tiles` ([`EDIT_CELL_SIZE`]) and
    /// refreshes `minmax` there.
    pub fn note(
        &mut self,
        map: &TerrainMap,
        minmax: &mut MinMaxPyramid,
        rect: DirtyRect,
        tiles: &BTreeSet<TileCoord>,
    ) {
        if rect.is_empty() {
            return;
        }
        minmax.update_region(map, &rect);
        self.gpu_dirty = Some(self.gpu_dirty.map_or(rect, |d| union_rect(d, rect)));
        self.draw_dirty = Some(self.draw_dirty.map_or(rect, |d| union_rect(d, rect)));
        self.height_changes = Some(self.height_changes.map_or(rect, |d| union_rect(d, rect)));
        self.scatter_reseat = Some(self.scatter_reseat.map_or(rect, |d| union_rect(d, rect)));
        self.unsaved_tiles.extend(tiles.iter().copied());
        self.generation = self.generation.wrapping_add(1);
    }

    /// Records a painted-weight edit over `rect`.
    pub fn note_paint(&mut self, rect: DirtyRect) {
        if rect.is_empty() {
            return;
        }
        self.paint_dirty = Some(self.paint_dirty.map_or(rect, |d| union_rect(d, rect)));
        self.unsaved_paint = true;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Records a painted-instance edit over `rect` ([`ScatterStroke::dirty`]).
    pub fn note_scatter(&mut self, rect: DirtyRect) {
        self.scatter_reseat = Some(self.scatter_reseat.map_or(rect, |d| union_rect(d, rect)));
        self.mark_scatter_unsaved();
    }

    /// Marks the painted instances as changed since the last save.
    pub fn mark_scatter_unsaved(&mut self) {
        self.unsaved_scatter = true;
        self.generation = self.generation.wrapping_add(1);
    }

    /// The rectangle painted instances should be re-seated over.
    #[must_use]
    pub fn scatter_reseat(&self) -> Option<DirtyRect> {
        self.scatter_reseat
    }

    /// [`Self::scatter_reseat`], cleared.
    pub fn take_scatter_reseat(&mut self) -> Option<DirtyRect> {
        self.scatter_reseat.take()
    }

    /// Whether painted instances changed since the last save, cleared.
    pub fn take_unsaved_scatter(&mut self) -> bool {
        std::mem::take(&mut self.unsaved_scatter)
    }

    /// `true` when painted instances changed since the last [`Self::take_unsaved_scatter`].
    #[must_use]
    pub fn unsaved_scatter(&self) -> bool {
        self.unsaved_scatter
    }

    /// The height rectangle to re-upload, cleared (`None` when nothing changed).
    pub fn take_gpu_dirty(&mut self) -> Option<DirtyRect> {
        self.gpu_dirty.take()
    }

    /// The height rectangle a linked draw has not been sent, cleared.
    pub(crate) fn take_draw_dirty(&mut self) -> Option<DirtyRect> {
        self.draw_dirty.take()
    }

    /// The height rectangle changed since this was last called, cleared. For CPU-side readers of
    /// the map (procedural generators' staleness); the GPU upload does not touch it.
    pub fn take_height_changes(&mut self) -> Option<DirtyRect> {
        self.height_changes.take()
    }

    /// The painted-weight rectangle to re-upload, cleared.
    pub fn take_paint_dirty(&mut self) -> Option<DirtyRect> {
        self.paint_dirty.take()
    }

    /// Whether painted weights changed since the last save, cleared.
    pub fn take_unsaved_paint(&mut self) -> bool {
        std::mem::take(&mut self.unsaved_paint)
    }

    /// `true` when painted weights changed since the last [`Self::take_unsaved_paint`].
    #[must_use]
    pub fn unsaved_paint(&self) -> bool {
        self.unsaved_paint
    }

    /// Edit cells changed since the last save, cleared.
    pub fn take_unsaved_tiles(&mut self) -> BTreeSet<TileCoord> {
        std::mem::take(&mut self.unsaved_tiles)
    }

    /// Edit cells changed since the last save.
    pub fn unsaved_tiles(&self) -> &BTreeSet<TileCoord> {
        &self.unsaved_tiles
    }

    /// Edit counter.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// One stroke's undo entry.
#[derive(Debug, Clone, PartialEq)]
pub enum StrokeUndo {
    /// A heightfield stroke.
    Height(StrokeRecord<f32>),
    /// A painted-weight stroke on one layer.
    LayerWeight {
        /// Layer painted.
        layer: usize,
        /// The stroke.
        record: StrokeRecord<u8>,
    },
    /// A painted-instance stroke (ADR-1936).
    Scatter(ScatterStroke),
}

impl StrokeUndo {
    /// Samples (for a scatter stroke, instances) changed by the stroke.
    pub fn len(&self) -> usize {
        match self {
            StrokeUndo::Height(r) => r.samples.len(),
            StrokeUndo::LayerWeight { record, .. } => record.samples.len(),
            StrokeUndo::Scatter(stroke) => stroke.len(),
        }
    }

    /// `true` when the stroke changed nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bounds of the changed samples.
    pub fn dirty(&self) -> DirtyRect {
        match self {
            StrokeUndo::Height(r) => r.dirty,
            StrokeUndo::LayerWeight { record, .. } => record.dirty,
            StrokeUndo::Scatter(stroke) => stroke.dirty,
        }
    }

    /// Cook tiles holding changed samples.
    pub fn tiles(&self) -> &BTreeSet<TileCoord> {
        match self {
            StrokeUndo::Height(r) => &r.tiles,
            StrokeUndo::LayerWeight { record, .. } => &record.tiles,
            StrokeUndo::Scatter(stroke) => &stroke.tiles,
        }
    }

    /// The brush of the stroke.
    pub fn brush(&self) -> &Brush {
        match self {
            StrokeUndo::Height(r) => &r.brush,
            StrokeUndo::LayerWeight { record, .. } => &record.brush,
            StrokeUndo::Scatter(stroke) => &stroke.brush,
        }
    }

    /// Writes the values from before (`undo = true`) or after the stroke.
    pub fn restore(
        &self,
        terrain: &mut Terrain,
        paint: Option<&mut TerrainPaint>,
        scatter: Option<&mut TerrainScatterPaint>,
        edits: &mut TerrainEdits,
        undo: bool,
    ) {
        match self {
            StrokeUndo::Height(record) => {
                let rect = if undo {
                    record.undo(&mut terrain.map)
                } else {
                    record.redo(&mut terrain.map)
                };
                edits.note(&terrain.map, &mut terrain.minmax, rect, &record.tiles);
            }
            StrokeUndo::LayerWeight { layer, record } => {
                let Some(mut target) = paint.and_then(|p| p.0.layer_mut(*layer)) else {
                    tracing::warn!(target: "regia_terrain", "terrain stroke undo: layer {layer} is gone; the weights stay as they are");
                    return;
                };
                let rect = if undo {
                    record.undo(&mut target)
                } else {
                    record.redo(&mut target)
                };
                edits.note_paint(rect);
            }
            StrokeUndo::Scatter(stroke) => {
                let Some(scatter) = scatter else { return };
                stroke.restore(&mut scatter.0, undo);
                edits.note_scatter(stroke.dirty);
            }
        }
    }
}

/// Undo entries of strokes made outside the editor's own tool (the `terrain.stroke` agent
/// method). The editor drains them onto its undo stack each frame.
#[derive(Resource, Debug, Default)]
pub struct TerrainStrokeLog {
    /// `(terrain entity, stroke)` in the order they were made.
    pub strokes: Vec<(Entity, StrokeUndo)>,
    /// An editor drains this log: every entry will become an undo step, so none may be dropped.
    /// Without one, only the newest [`STROKE_LOG_CAP`] are kept ([`Self::push`]).
    pub drained: bool,
}

/// Entries a [`TerrainStrokeLog`] keeps while no editor drains it.
pub const STROKE_LOG_CAP: usize = 256;

impl TerrainStrokeLog {
    /// Appends a stroke's undo entry. Under an editor (`drained`) every entry stays, because each
    /// becomes an undo step; without one only the newest [`STROKE_LOG_CAP`] are kept.
    pub fn push(&mut self, entity: Entity, undo: StrokeUndo) {
        self.strokes.push((entity, undo));
        if !self.drained {
            let excess = self.strokes.len().saturating_sub(STROKE_LOG_CAP);
            self.strokes.drain(..excess);
        }
    }
}

/// Errors from editing a terrain.
#[derive(Debug, thiserror::Error)]
pub enum TerrainEditError {
    /// The stroke path is empty or not finite.
    #[error("a stroke needs at least one finite point")]
    BadPath,
    /// The brush parameters are not finite or out of range.
    #[error("invalid brush: {0}")]
    BadBrush(String),
    /// A layer-weight stroke on a terrain without [`TerrainPaint`] or with too few layers.
    #[error("layer {layer} cannot be painted: the terrain has {layers} painted layers")]
    NoSuchLayer {
        /// Requested layer.
        layer: usize,
        /// Painted layers available.
        layers: u32,
    },
    /// The painted layer-weight map's grid is not the heightfield's: a stroke would land on
    /// the wrong samples (painted weights are per heightfield sample).
    #[error("the painted layer weights are {paint:?} samples but the heightfield is {map:?}")]
    PaintSizeMismatch {
        /// `[columns, rows]` of the [`TerrainPaint`] map.
        paint: [u32; 2],
        /// `[columns, rows]` of the heightfield.
        map: [u32; 2],
    },
    /// A height patch whose rectangle leaves the map, is empty, or whose sample count or values
    /// are wrong ([`apply_height_patch`]).
    #[error("invalid height patch: {0}")]
    BadPatch(String),
}

/// Why [`TerrainStrokeSession::finish`] did not edit (ADR-2435).
#[derive(Debug, thiserror::Error)]
pub enum StrokeSessionError {
    /// The entity does not exist or has no [`Terrain`].
    #[error("entity {0:?} has no Terrain")]
    NoTerrain(Entity),
    /// The stroke itself was refused.
    #[error(transparent)]
    Edit(#[from] TerrainEditError),
}

/// Checks a brush for finite, sensible parameters.
///
/// # Errors
///
/// [`TerrainEditError::BadBrush`] naming the offending field.
pub fn validate_brush(brush: &Brush) -> Result<(), TerrainEditError> {
    let bad = |what: &str| Err(TerrainEditError::BadBrush(what.to_string()));
    match brush.shape {
        brush::BrushShape::Circle { radius } if !(radius.is_finite() && radius > 0.0) => {
            return bad("radius");
        }
        brush::BrushShape::Rectangle { width, length }
            if !(width.is_finite() && width > 0.0 && length.is_finite() && length > 0.0) =>
        {
            return bad("width/length");
        }
        _ => {}
    }
    match brush.mode {
        brush::BrushMode::Raise { amount } | brush::BrushMode::Lower { amount } if !amount.is_finite() => {
            return bad("amount");
        }
        brush::BrushMode::Assign { value } if !value.is_finite() => return bad("value"),
        brush::BrushMode::Smooth { kernel_radius } if kernel_radius > 64 => {
            return bad("kernel_radius (max 64)");
        }
        brush::BrushMode::Noise { amount, .. } if !amount.is_finite() => return bad("amount"),
        brush::BrushMode::Noise { wavelength, .. } if !(wavelength.is_finite() && wavelength >= 0.5) => {
            return bad("wavelength (at least 0.5 samples)");
        }
        brush::BrushMode::Ramp {
            start,
            end,
            start_value,
            end_value,
        } if !start
            .iter()
            .chain(end.iter())
            .chain([start_value, end_value].iter())
            .all(|v| v.is_finite()) =>
        {
            return bad("ramp (start, end and values must be finite)");
        }
        _ => {}
    }
    if !(0.0..=1.0).contains(&brush.hardness) {
        return bad("hardness (0..=1)");
    }
    if !(0.0..=1.0).contains(&brush.alpha) {
        return bad("alpha (0..=1)");
    }
    if brush.transform.iter().any(|v| !v.is_finite()) {
        return bad("transform");
    }
    Ok(())
}

/// A [`brush::BrushMode::Ramp`] from world point `from` to `to` (XZ, metres) on `map`: the ends
/// in sample coordinates, the heights `from_height` / `to_height` or, when `None`, the map's
/// bilinear height at that end (`docs/specs/82-terrain-editing.md` § 2).
pub fn ramp_mode(
    map: &TerrainMap,
    from: Vec2,
    to: Vec2,
    from_height: Option<f32>,
    to_height: Option<f32>,
) -> brush::BrushMode {
    let start = brush::world_to_sample(map, from.x, from.y);
    let end = brush::world_to_sample(map, to.x, to.y);
    brush::BrushMode::Ramp {
        start: start.to_array(),
        end: end.to_array(),
        start_value: from_height.unwrap_or_else(|| map.sample_bilinear(from.x, from.y)),
        end_value: to_height.unwrap_or_else(|| map.sample_bilinear(to.x, to.y)),
    }
}

/// Runs one stroke synchronously: a stamp at `path[0]` (world XZ, metres), smeared through the
/// rest, on the heightfield or one painted layer. Records the edit in `edits` (min/max pyramid,
/// GPU dirty rect, unsaved tiles) and returns the stroke's undo entry.
///
/// # Errors
///
/// [`TerrainEditError`] for a bad path, brush or layer.
pub fn apply_stroke(
    terrain: &mut Terrain,
    paint: Option<&mut TerrainPaint>,
    edits: &mut TerrainEdits,
    brush: &Brush,
    path: &[Vec2],
) -> Result<StrokeUndo, TerrainEditError> {
    let _span = tracing::info_span!(target: "regia_terrain", "terrain.apply_stroke").entered();
    if path.is_empty() || path.iter().any(|p| !p.is_finite()) {
        return Err(TerrainEditError::BadPath);
    }
    validate_brush(brush)?;
    match brush.target {
        BrushTarget::Height => {
            let record = brush::stroke_heights(&mut terrain.map, brush, path, EDIT_CELL_SIZE);
            edits.note(&terrain.map, &mut terrain.minmax, record.dirty, &record.tiles);
            Ok(StrokeUndo::Height(record))
        }
        BrushTarget::LayerWeight { layer } => {
            let layers = paint.as_ref().map_or(0, |p| p.0.layers());
            if let Some(p) = paint.as_ref() {
                let size = [p.0.width(), p.0.height()];
                let map_size = [terrain.map.width(), terrain.map.height()];
                if size != map_size {
                    return Err(TerrainEditError::PaintSizeMismatch {
                        paint: size,
                        map: map_size,
                    });
                }
            }
            let map = &terrain.map;
            let Some(mut target) = paint.and_then(|p| p.0.layer_mut(layer)) else {
                return Err(TerrainEditError::NoSuchLayer { layer, layers });
            };
            let scale = brush::map_scale(map);
            let mut stroke = BrushStroke::<u8>::new(*brush, EDIT_CELL_SIZE);
            let mut prev: Option<Vec2> = None;
            for p in path {
                let s = brush::world_to_sample(map, p.x, p.y);
                match prev {
                    None => stroke.stamp(&target, s, scale, 0.0),
                    Some(start) => stroke.smear(&target, start, s, scale, 0.0),
                }
                prev = Some(s);
            }
            let record = stroke.finish_into(&mut target);
            edits.note_paint(record.dirty);
            Ok(StrokeUndo::LayerWeight { layer, record })
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The floating origin (spec 78 § 12, ADR-2435)
// ---------------------------------------------------------------------------------------------
//
// A terrain stays world-anchored: its map, brushes and undo records use world meters. Gameplay,
// picking and the editor viewport work in the process's local space, which a floating-origin rebase
// moves. The `*_local` entry points take local points and go through the origin, so the same
// world point edits the same samples before and after a rebase.

/// The terrain (world) XZ of local point `local` (XZ, meters): `local + origin`, added in `f64`.
#[must_use]
pub fn local_to_terrain(origin: &OriginOffset, local: Vec2) -> Vec2 {
    Vec2::new(
        (f64::from(local.x) + origin.0[0]) as f32,
        (f64::from(local.y) + origin.0[2]) as f32,
    )
}

/// The local XZ of terrain point `world`: the inverse of [`local_to_terrain`].
#[must_use]
pub fn terrain_to_local(origin: &OriginOffset, world: Vec2) -> Vec2 {
    Vec2::new(
        (f64::from(world.x) - origin.0[0]) as f32,
        (f64::from(world.y) - origin.0[2]) as f32,
    )
}

/// The terrain height of local height `local_y` (meters).
#[must_use]
pub fn local_height_to_terrain(origin: &OriginOffset, local_y: f32) -> f32 {
    (f64::from(local_y) + origin.0[1]) as f32
}

/// [`apply_stroke`] with `path` in local XZ (gameplay, picking, the editor viewport), converted
/// through `origin`. The brush's own values (an `Assign` height, a `Ramp`'s heights and sample
/// ends) stay terrain values: build a ramp with [`ramp_mode_local`].
///
/// # Errors
///
/// As [`apply_stroke`].
pub fn apply_stroke_local(
    terrain: &mut Terrain,
    paint: Option<&mut TerrainPaint>,
    edits: &mut TerrainEdits,
    brush: &Brush,
    path: &[Vec2],
    origin: &OriginOffset,
) -> Result<StrokeUndo, TerrainEditError> {
    let world: Vec<Vec2> = path.iter().map(|p| local_to_terrain(origin, *p)).collect();
    apply_stroke(terrain, paint, edits, brush, &world)
}

/// [`ramp_mode`] from local points and local heights.
#[must_use]
pub fn ramp_mode_local(
    map: &TerrainMap,
    from: Vec2,
    to: Vec2,
    from_height: Option<f32>,
    to_height: Option<f32>,
    origin: &OriginOffset,
) -> brush::BrushMode {
    ramp_mode(
        map,
        local_to_terrain(origin, from),
        local_to_terrain(origin, to),
        from_height.map(|h| local_height_to_terrain(origin, h)),
        to_height.map(|h| local_height_to_terrain(origin, h)),
    )
}

/// [`brush_outline`] around local `center`, returned in local space (for gizmos).
#[must_use]
pub fn brush_outline_local(
    map: &TerrainMap,
    center: Vec2,
    shape: &brush::BrushShape,
    segments: u32,
    origin: &OriginOffset,
) -> Vec<glam::Vec3> {
    let offset = origin_vec3(origin);
    brush_outline(map, local_to_terrain(origin, center), shape, segments)
        .into_iter()
        .map(|p| p - offset)
        .collect()
}

/// [`raycast`] with the ray and the hit in local space.
#[must_use]
pub fn raycast_local(
    map: &TerrainMap,
    origin_local: glam::Vec3,
    direction: glam::Vec3,
    max_distance: f32,
    origin: &OriginOffset,
) -> Option<glam::Vec3> {
    let offset = origin_vec3(origin);
    raycast(map, origin_local + offset, direction, max_distance).map(|hit| hit - offset)
}

/// The origin as an `f32` vector (whole cells are exact).
fn origin_vec3(origin: &OriginOffset) -> glam::Vec3 {
    glam::Vec3::new(origin.0[0] as f32, origin.0[1] as f32, origin.0[2] as f32)
}

/// A brush stroke whose points arrive over several frames (an editor drag, a gameplay tool).
///
/// Each point is converted to terrain space when it arrives, with the origin of that moment
/// ([`Self::push_local`]), so the stroke edits the world points it was given even when the origin
/// moves under it. While a session made by [`Self::begin`] is open it holds
/// [`TERRAIN_STROKE_BLOCKER`] in `RebaseBlockers`, so the streaming runtime takes no rebase in the
/// middle of the stroke (ADR-2435): a host that converted a point itself, before the rebase, cannot
/// smear it to one converted after. End it with [`Self::finish`] or [`Self::cancel`]; a dropped
/// session keeps its hold until [`release_rebase_hold`] is called for it.
#[must_use = "a session holds a rebase blocker until it is finished or cancelled"]
#[derive(Debug, Clone, PartialEq)]
pub struct TerrainStrokeSession {
    brush: Brush,
    /// Points so far, terrain XZ meters.
    path: Vec<Vec2>,
    /// The session holds [`TERRAIN_STROKE_BLOCKER`].
    held: bool,
}

impl TerrainStrokeSession {
    /// Opens a stroke on `world`, holding [`TERRAIN_STROKE_BLOCKER`].
    pub fn begin(world: &mut World, brush: Brush) -> Self {
        acquire_rebase_hold(world, TERRAIN_STROKE_BLOCKER);
        Self {
            brush,
            path: Vec::new(),
            held: true,
        }
    }

    /// A session that holds no rebase blocker, for a host without a world or one that converts
    /// every point through this session.
    pub fn detached(brush: Brush) -> Self {
        Self {
            brush,
            path: Vec::new(),
            held: false,
        }
    }

    /// The brush.
    #[must_use]
    pub fn brush(&self) -> &Brush {
        &self.brush
    }

    /// The points so far, terrain XZ meters.
    #[must_use]
    pub fn path(&self) -> &[Vec2] {
        &self.path
    }

    /// The session holds [`TERRAIN_STROKE_BLOCKER`].
    #[must_use]
    pub fn holds_rebase(&self) -> bool {
        self.held
    }

    /// Adds a point in terrain XZ meters.
    pub fn push(&mut self, world: Vec2) {
        self.path.push(world);
    }

    /// Adds a point in local XZ, converted through `origin` as it is now.
    pub fn push_local(&mut self, local: Vec2, origin: &OriginOffset) {
        self.path.push(local_to_terrain(origin, local));
    }

    /// Applies the stroke ([`apply_stroke`]: a stamp at the first point, smeared through the rest)
    /// to `terrain`, for a system that holds the terrain's components and the two rebase
    /// resources. A session from [`Self::begin`] lets its hold go through `holds`; pass `None` for a
    /// [`Self::detached`] one.
    ///
    /// # Errors
    ///
    /// As [`apply_stroke`]; the hold is released either way.
    pub fn finish_with(
        self,
        terrain: &mut Terrain,
        paint: Option<&mut TerrainPaint>,
        edits: &mut TerrainEdits,
        holds: Option<(&mut RebaseHolds, &mut RebaseBlockers)>,
    ) -> Result<StrokeUndo, TerrainEditError> {
        let result = apply_stroke(terrain, paint, edits, &self.brush, &self.path);
        if self.held
            && let Some((holds, blockers)) = holds
        {
            holds.release(blockers, TERRAIN_STROKE_BLOCKER);
        }
        result
    }

    /// [`Self::finish_with`] for the terrain on `entity` of `world` (adds [`TerrainEdits`] when
    /// missing). The stroke is pushed onto [`TerrainStrokeLog`] when that resource exists.
    ///
    /// # Errors
    ///
    /// [`StrokeSessionError::NoTerrain`] when `entity` has no [`Terrain`]; otherwise
    /// [`StrokeSessionError::Edit`] as [`apply_stroke`]. The hold is released either way.
    pub fn finish(self, world: &mut World, entity: Entity) -> Result<StrokeUndo, StrokeSessionError> {
        let held = self.held;
        let result = self.apply_on_entity(world, entity);
        if held {
            release_rebase_hold(world, TERRAIN_STROKE_BLOCKER);
        }
        let undo = result?;
        if !undo.is_empty()
            && let Some(mut log) = world.get_resource_mut::<TerrainStrokeLog>()
        {
            log.push(entity, undo.clone());
        }
        Ok(undo)
    }

    fn apply_on_entity(&self, world: &mut World, entity: Entity) -> Result<StrokeUndo, StrokeSessionError> {
        {
            let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
                return Err(StrokeSessionError::NoTerrain(entity));
            };
            if !entity_mut.contains::<Terrain>() {
                return Err(StrokeSessionError::NoTerrain(entity));
            }
            if !entity_mut.contains::<TerrainEdits>() {
                entity_mut.insert(TerrainEdits::default());
            }
        }
        let mut query = world.query::<(&mut Terrain, Option<&mut TerrainPaint>, &mut TerrainEdits)>();
        let Ok((mut terrain, paint, mut edits)) = query.get_mut(world, entity) else {
            return Err(StrokeSessionError::NoTerrain(entity));
        };
        Ok(apply_stroke(
            &mut terrain,
            paint.map(Mut::into_inner),
            &mut edits,
            &self.brush,
            &self.path,
        )?)
    }

    /// Drops the stroke without editing and lets the hold go.
    pub fn cancel(self, world: &mut World) {
        if self.held {
            release_rebase_hold(world, TERRAIN_STROKE_BLOCKER);
        }
    }
}

/// The undo entry of one [`apply_height_patch`]: the rectangle with its samples from before and
/// after, row-major. Restoring writes them back bit for bit.
#[derive(Debug, Clone, PartialEq)]
pub struct HeightPatchUndo {
    /// The patched sample rectangle.
    pub rect: DirtyRect,
    /// Heights before the patch, metres, row-major over `rect`.
    pub before: Vec<f32>,
    /// Heights after the patch.
    pub after: Vec<f32>,
    /// Edit cells ([`EDIT_CELL_SIZE`]) the rectangle touches.
    pub tiles: BTreeSet<TileCoord>,
}

impl HeightPatchUndo {
    /// Writes the heights from before (`undo = true`) or after the patch and records the edit.
    pub fn restore(&self, terrain: &mut Terrain, edits: &mut TerrainEdits, undo: bool) {
        let values = if undo { &self.before } else { &self.after };
        write_patch(&mut terrain.map, self.rect, values);
        edits.note(&terrain.map, &mut terrain.minmax, self.rect, &self.tiles);
    }
}

fn write_patch(map: &mut TerrainMap, rect: DirtyRect, values: &[f32]) {
    let map_w = map.width() as usize;
    let map_h = map.height() as usize;
    // An empty or inverted rectangle has nothing to write (and `i % 0` would abort).
    let w = match rect.col_end.checked_sub(rect.col_start) {
        Some(w) if w > 0 && rect.row_end > rect.row_start => w as usize,
        _ => return,
    };
    let rows = (rect.row_end - rect.row_start) as usize;
    // A hand-built rect can run past the map. Samples that land off it are not written:
    // spilling into the next row corrupts neighbours, and a row past the end indexes off
    // the height buffer. [`apply_height_patch`] still rejects that rect up front.
    let n = values.len().min(w.saturating_mul(rows));
    for (i, v) in values.iter().take(n).enumerate() {
        let c = rect.col_start as usize + i % w;
        let r = rect.row_start as usize + i / w;
        if c < map_w && r < map_h {
            map.set_height(r * map_w + c, *v);
        }
    }
}

/// Writes a rectangle of height samples without recording an edit: for code that puts the samples
/// back before anything reads them (a capture that must not see an applied patch), and for a bare
/// [`TerrainMap`] with no terrain component. Samples that fall outside the map are skipped, and
/// values past the rectangle's area are ignored. [`apply_height_patch`] rejects a bad rectangle
/// instead of writing it.
pub fn write_height_samples(map: &mut TerrainMap, rect: DirtyRect, values: &[f32]) {
    write_patch(map, rect, values);
}

/// Writes a rectangle of new height samples (metres, row-major over `rect`) into the terrain:
/// the bake-time terrain edit of spec 64 § 7.3 (road flattening). Records the dirty rectangle
/// for the GPU re-upload and the edit cells for the next re-cook in `edits`, and returns the
/// undo entry holding the original samples.
///
/// # Errors
///
/// [`TerrainEditError::BadPatch`] when the rectangle is empty or leaves the map, the sample
/// count is not its area, or a sample is not finite. The terrain is untouched then.
pub fn apply_height_patch(
    terrain: &mut Terrain,
    edits: &mut TerrainEdits,
    rect: DirtyRect,
    samples: &[f32],
) -> Result<HeightPatchUndo, TerrainEditError> {
    let _span = tracing::info_span!(target: "regia_terrain", "terrain.apply_height_patch").entered();
    let map = &terrain.map;
    if rect.is_empty() || rect.col_end > map.width() || rect.row_end > map.height() {
        return Err(TerrainEditError::BadPatch(format!(
            "rectangle {rect:?} is empty or outside the {} x {} map",
            map.width(),
            map.height()
        )));
    }
    let (w, h) = (rect.col_end - rect.col_start, rect.row_end - rect.row_start);
    if samples.len() != (w as usize) * (h as usize) {
        return Err(TerrainEditError::BadPatch(format!(
            "{} samples for a {w} x {h} rectangle",
            samples.len()
        )));
    }
    if let Some(i) = samples.iter().position(|v| !v.is_finite()) {
        return Err(TerrainEditError::BadPatch(format!("sample {i} is not finite")));
    }
    let mut before = Vec::with_capacity(samples.len());
    for r in rect.row_start..rect.row_end {
        for c in rect.col_start..rect.col_end {
            before.push(map.height_at_sample(i64::from(c), i64::from(r)));
        }
    }
    let mut tiles = BTreeSet::new();
    for tr in rect.row_start / EDIT_CELL_SIZE..=(rect.row_end - 1) / EDIT_CELL_SIZE {
        for tc in rect.col_start / EDIT_CELL_SIZE..=(rect.col_end - 1) / EDIT_CELL_SIZE {
            tiles.insert((tc, tr));
        }
    }
    let undo = HeightPatchUndo {
        rect,
        before,
        after: samples.to_vec(),
        tiles,
    };
    write_patch(&mut terrain.map, rect, samples);
    edits.note(&terrain.map, &mut terrain.minmax, rect, &undo.tiles);
    Ok(undo)
}

impl TerrainMap {
    /// Quantises the metre heights back to 16-bit samples
    /// (`round((h - offset) / scale · 65535)`, clamped), the inverse of [`TerrainMap::new`].
    pub fn to_heightfield(&self) -> regia_asset::Heightfield {
        let scale = self.height_scale();
        let inv = if scale.abs() > f32::EPSILON {
            65535.0 / scale
        } else {
            0.0
        };
        let offset = self.height_offset();
        let samples = self
            .heights()
            .iter()
            .map(|h| ((h - offset) * inv).round().clamp(0.0, 65535.0) as u16)
            .collect();
        regia_asset::Heightfield {
            width: self.width(),
            height: self.height(),
            samples,
        }
    }
}

/// Largest cook tile edge accepted by the cook and re-cook functions.
pub const MAX_COOK_TILE_SIZE: u32 = 8192;

/// Cooks one tile of a heightfield exactly as the asset cooker does
/// (`regia_asset_pipeline::terrain_data::cook_tiled_heightfield`): samples copied verbatim,
/// the last row/column padded with the border sample, per-tile min/max.
///
/// # Errors
///
/// [`RecookError::BadTileSize`] when `tile_size` is 0 or above [`MAX_COOK_TILE_SIZE`]
/// (checked before any allocation).
pub fn cook_heightfield_tile(
    heightfield: &regia_asset::Heightfield,
    tile_size: u32,
    tx: u32,
    ty: u32,
) -> Result<CookedHeightfieldTile, RecookError> {
    if tile_size == 0 || tile_size > MAX_COOK_TILE_SIZE {
        return Err(RecookError::BadTileSize { tile_size });
    }
    let (w, h) = (heightfield.width.max(1), heightfield.height.max(1));
    let mut min = u16::MAX;
    let mut max = 0u16;
    let mut samples = Vec::with_capacity(
        (tile_size as usize)
            .saturating_mul(tile_size as usize)
            .min(MAX_COOK_TILE_SIZE as usize * MAX_COOK_TILE_SIZE as usize),
    );
    for row in 0..tile_size {
        let src_row = (ty * tile_size + row).min(h - 1);
        for col in 0..tile_size {
            let src_col = (tx * tile_size + col).min(w - 1);
            let v = heightfield
                .samples
                .get(src_row as usize * w as usize + src_col as usize)
                .copied()
                .unwrap_or(0);
            min = min.min(v);
            max = max.max(v);
            samples.push(v);
        }
    }
    Ok(CookedHeightfieldTile {
        x: tx,
        y: ty,
        min,
        max,
        samples,
    })
}

/// Re-cooks the cook tiles of `cooked` that overlap the edit `cells` (`cell_size` samples each,
/// [`EDIT_CELL_SIZE`] for strokes) from `map`, quantised with [`TerrainMap::to_heightfield`].
/// Returns how many cook tiles were re-cooked; every other tile is left untouched.
///
/// # Errors
///
/// [`RecookError`] when `map` and `cooked` differ in size or a touched tile is missing.
pub fn recook_tiles(
    cooked: &mut CookedTiledHeightfield,
    map: &TerrainMap,
    cells: &BTreeSet<TileCoord>,
    cell_size: u32,
) -> Result<usize, RecookError> {
    let _span =
        tracing::info_span!(target: "regia_terrain", "terrain.recook_tiles", cells = cells.len()).entered();
    if cooked.tile_size > MAX_COOK_TILE_SIZE {
        return Err(RecookError::BadTileSize {
            tile_size: cooked.tile_size,
        });
    }
    if cooked.width != map.width() || cooked.height != map.height() || cooked.tile_size == 0 {
        return Err(RecookError::SizeMismatch {
            cooked: [cooked.width, cooked.height],
            map: [map.width(), map.height()],
        });
    }
    let tile_size = cooked.tile_size;
    let cell_size = cell_size.max(1);
    let mut tiles = BTreeSet::new();
    for &(cx, cy) in cells {
        let (x0, y0) = (cx.saturating_mul(cell_size), cy.saturating_mul(cell_size));
        let (x1, y1) = (x0.saturating_add(cell_size - 1), y0.saturating_add(cell_size - 1));
        for ty in y0 / tile_size..=y1.min(map.height().saturating_sub(1)) / tile_size {
            for tx in x0 / tile_size..=x1.min(map.width().saturating_sub(1)) / tile_size {
                tiles.insert((tx, ty));
            }
        }
    }
    let heightfield = map.to_heightfield();
    let mut recooked = 0;
    for (tx, ty) in tiles {
        let Some(slot) = cooked.tiles.iter_mut().find(|t| t.x == tx && t.y == ty) else {
            return Err(RecookError::MissingTile { x: tx, y: ty });
        };
        *slot = cook_heightfield_tile(&heightfield, tile_size, tx, ty)?;
        recooked += 1;
    }
    Ok(recooked)
}

/// Cooks every tile of `map` (a fresh cooked heightfield in the cooker's tile order).
///
/// # Errors
///
/// [`RecookError::BadTileSize`] when `tile_size` is 0 or above [`MAX_COOK_TILE_SIZE`].
pub fn cook_all_tiles(map: &TerrainMap, tile_size: u32) -> Result<CookedTiledHeightfield, RecookError> {
    if tile_size == 0 || tile_size > MAX_COOK_TILE_SIZE {
        return Err(RecookError::BadTileSize { tile_size });
    }
    let heightfield = map.to_heightfield();
    let (tiles_x, tiles_y) = (map.width().div_ceil(tile_size), map.height().div_ceil(tile_size));
    let mut tiles = Vec::with_capacity((tiles_x * tiles_y) as usize);
    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            tiles.push(cook_heightfield_tile(&heightfield, tile_size, tx, ty)?);
        }
    }
    Ok(CookedTiledHeightfield {
        version: crate::stream::TILED_HEIGHTFIELD_FORMAT_VERSION,
        width: map.width(),
        height: map.height(),
        tile_size,
        tiles,
    })
}

/// Errors from the incremental re-cook.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecookError {
    /// The cooked heightfield and the map differ in size, or the cooked tile size is 0.
    #[error("cooked heightfield is {cooked:?}, the edited map {map:?}")]
    SizeMismatch {
        /// Cooked size.
        cooked: [u32; 2],
        /// Map size.
        map: [u32; 2],
    },
    /// The cook tile edge is 0 or above [`MAX_COOK_TILE_SIZE`].
    #[error("cook tile size {tile_size} is outside 1..={MAX_COOK_TILE_SIZE}")]
    BadTileSize {
        /// The rejected tile edge.
        tile_size: u32,
    },
    /// A touched tile is missing from the cooked heightfield.
    #[error("cooked heightfield has no tile ({x}, {y})")]
    MissingTile {
        /// Tile column.
        x: u32,
        /// Tile row.
        y: u32,
    },
}

/// The brush outline to draw this frame (editor tool or agent preview). Set `center` to
/// `None` to hide it.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct TerrainBrushPreview {
    /// Brush centre, world XZ metres.
    pub center: Option<Vec2>,
    /// Brush shape.
    pub shape: brush::BrushShape,
    /// Outline colour.
    pub color: regia_core::math::Color,
}

impl Default for TerrainBrushPreview {
    fn default() -> Self {
        Self {
            center: None,
            shape: brush::BrushShape::default(),
            color: regia_core::math::Color::rgb(1.0, 0.75, 0.2),
        }
    }
}

/// Lift of the outline above the surface, metres (keeps it out of the terrain's depth).
const OUTLINE_LIFT_M: f32 = 0.15;

/// A closed outline of `shape` at `center` that follows the terrain surface (`segments` points
/// for a circle; rectangles get points along every edge). The last point repeats the first.
pub fn brush_outline(
    map: &TerrainMap,
    center: Vec2,
    shape: &brush::BrushShape,
    segments: u32,
) -> Vec<glam::Vec3> {
    let segments = segments.clamp(8, 512);
    let at = |p: Vec2| glam::Vec3::new(p.x, map.sample_bilinear(p.x, p.y) + OUTLINE_LIFT_M, p.y);
    let mut out: Vec<glam::Vec3> = match *shape {
        brush::BrushShape::Circle { radius } => (0..segments)
            .map(|i| {
                let a = i as f32 / segments as f32 * std::f32::consts::TAU;
                at(center + Vec2::new(a.cos(), a.sin()) * radius)
            })
            .collect(),
        brush::BrushShape::Rectangle { width, length } => {
            let h = Vec2::new(width * 0.5, length * 0.5);
            let corners = [
                Vec2::new(-h.x, -h.y),
                Vec2::new(h.x, -h.y),
                Vec2::new(h.x, h.y),
                Vec2::new(-h.x, h.y),
            ];
            let per_edge = (segments / 4).max(2);
            (0..4)
                .flat_map(|e| {
                    let (a, b) = (corners[e], corners[(e + 1) % 4]);
                    (0..per_edge).map(move |i| a.lerp(b, i as f32 / per_edge as f32))
                })
                .map(|p| at(center + p))
                .collect()
        }
    };
    if let Some(first) = out.first().copied() {
        out.push(first);
    }
    out
}

/// First hit of the ray `origin + t · direction` with the terrain surface (the bilinear
/// heightfield), searched up to `max_distance` metres; `None` when it misses the map.
/// Marches at half a sample spacing, then bisects the crossing. Holes are not tested: see
/// [`crate::raycast_with_holes`] and [`crate::pick_terrain`].
pub fn raycast(
    map: &TerrainMap,
    origin: glam::Vec3,
    direction: glam::Vec3,
    max_distance: f32,
) -> Option<glam::Vec3> {
    crate::pick::raycast_with_holes(map, None, origin, direction, max_distance)
}

/// `Update`: draws [`TerrainBrushPreview`] over the first [`Terrain`] with gizmos.
/// The preview's `center` is a terrain (world) point; gizmos draw in local space, so the outline
/// goes through the floating origin (ADR-2435).
pub(crate) fn draw_brush_preview(
    preview: Res<TerrainBrushPreview>,
    terrains: Query<&Terrain>,
    origin: Option<Res<OriginOffset>>,
    mut gizmos: regia_render::gizmos::Gizmos,
) {
    let Some(center) = preview.center else { return };
    let Some(terrain) = terrains.iter().next() else {
        return;
    };
    let offset = origin.as_deref().map_or(glam::Vec3::ZERO, origin_vec3);
    let ring = brush_outline(&terrain.map, center, &preview.shape, 64);
    gizmos.linestrip(ring.into_iter().map(|p| p - offset), preview.color);
    // A short stalk at the centre shows where the stroke is anchored.
    let h = terrain.map.sample_bilinear(center.x, center.y);
    let reach = preview.shape.reach().max(0.5);
    gizmos.line(
        glam::Vec3::new(center.x, h, center.y) - offset,
        glam::Vec3::new(center.x, h + reach * 0.25, center.y) - offset,
        preview.color,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brush::{BrushMode, BrushShape};
    use crate::{CdlodConfig, TerrainLayers};

    fn terrain(n: u32, world: f32) -> Terrain {
        let heights = (0..n * n)
            .map(|i| 100.0 + ((i % n) as f32 * 0.07).sin() * 4.0 + ((i / n) as f32 * 0.05).cos() * 3.0)
            .collect();
        let map = TerrainMap::from_heights(n, n, heights, world, 400.0, 0.0).expect("map");
        let heightfield = map.to_heightfield();
        Terrain::new(
            &heightfield,
            world,
            400.0,
            0.0,
            CdlodConfig::default(),
            4,
            TerrainLayers::default(),
        )
        .expect("terrain")
    }

    #[test]
    fn recook_rejects_huge_decoded_tile_size() {
        let t = terrain(65, 64.0);
        let mut cooked = cook_all_tiles(&t.map, 32).expect("cook");
        cooked.tile_size = u32::MAX;
        let cells = BTreeSet::from([(0, 0)]);
        assert!(matches!(
            recook_tiles(&mut cooked, &t.map, &cells, EDIT_CELL_SIZE),
            Err(RecookError::BadTileSize { tile_size: u32::MAX })
        ));
    }

    #[test]
    fn quantise_round_trips() {
        let t = terrain(65, 64.0);
        let hf = t.map.to_heightfield();
        let back = TerrainMap::new(&hf, 64.0, 400.0, 0.0).expect("map");
        assert_eq!(back.to_heightfield(), hf);
    }

    #[test]
    fn stroke_updates_minmax_and_marks_tiles() {
        let mut t = terrain(129, 128.0);
        let mut edits = TerrainEdits::default();
        let brush = Brush {
            shape: BrushShape::Circle { radius: 6.0 },
            mode: BrushMode::Raise { amount: 30.0 },
            hardness: 1.0,
            ..Brush::default()
        };
        let undo = apply_stroke(&mut t, None, &mut edits, &brush, &[Vec2::new(10.0, -20.0)]).expect("stroke");
        assert!(!undo.is_empty());
        let rebuilt = MinMaxPyramid::build(&t.map, t.root_level).expect("pyramid");
        assert_eq!(
            format!("{:?}", t.minmax),
            format!("{rebuilt:?}"),
            "incremental min/max equals a rebuild"
        );
        let dirty = edits.take_gpu_dirty().expect("dirty");
        assert_eq!(dirty, undo.dirty());
        assert!(edits.take_gpu_dirty().is_none());
        assert_eq!(edits.unsaved_tiles(), undo.tiles());
        undo.restore(&mut t, None, None, &mut edits, true);
        let rebuilt = MinMaxPyramid::build(&t.map, t.root_level).expect("pyramid");
        assert_eq!(format!("{:?}", t.minmax), format!("{rebuilt:?}"));
    }

    #[test]
    fn bad_strokes_are_rejected() {
        let mut t = terrain(33, 32.0);
        let mut edits = TerrainEdits::default();
        let brush = Brush::default();
        assert!(matches!(
            apply_stroke(&mut t, None, &mut edits, &brush, &[]),
            Err(TerrainEditError::BadPath)
        ));
        let nan = Brush {
            shape: BrushShape::Circle { radius: f32::NAN },
            ..Brush::default()
        };
        assert!(apply_stroke(&mut t, None, &mut edits, &nan, &[Vec2::ZERO]).is_err());
        let paint = Brush {
            target: BrushTarget::LayerWeight { layer: 3 },
            ..Brush::default()
        };
        assert!(matches!(
            apply_stroke(&mut t, None, &mut edits, &paint, &[Vec2::ZERO]),
            Err(TerrainEditError::NoSuchLayer { layer: 3, layers: 0 })
        ));
    }

    #[test]
    fn cook_rejects_out_of_range_tile_size_before_allocating() {
        let t = terrain(33, 32.0);
        let hf = t.map.to_heightfield();
        for bad in [0, MAX_COOK_TILE_SIZE + 1, u32::MAX] {
            assert_eq!(
                cook_heightfield_tile(&hf, bad, 0, 0),
                Err(RecookError::BadTileSize { tile_size: bad })
            );
            assert_eq!(
                cook_all_tiles(&t.map, bad),
                Err(RecookError::BadTileSize { tile_size: bad })
            );
        }
        let mut cooked = cook_all_tiles(&t.map, 16).expect("cook");
        cooked.tile_size = u32::MAX;
        assert_eq!(
            recook_tiles(&mut cooked, &t.map, &BTreeSet::new(), 16),
            Err(RecookError::BadTileSize { tile_size: u32::MAX })
        );
    }

    #[test]
    fn height_changes_outlive_the_gpu_upload() {
        let mut t = terrain(129, 128.0);
        let mut edits = TerrainEdits::default();
        let rect = DirtyRect {
            col_start: 4,
            col_end: 6,
            row_start: 7,
            row_end: 8,
        };
        apply_height_patch(&mut t, &mut edits, rect, &[1.0, 2.0]).expect("patch");
        assert_eq!(edits.take_gpu_dirty(), Some(rect), "the upload ran first");
        assert_eq!(edits.take_height_changes(), Some(rect));
        assert_eq!(edits.take_height_changes(), None, "cleared");
    }

    #[test]
    fn one_tile_edit_recooks_only_that_tile() {
        let mut t = terrain(129, 128.0);
        let tile_size = 32;
        let mut cooked = cook_all_tiles(&t.map, tile_size).expect("cook");
        let before = cooked.clone();
        let mut edits = TerrainEdits::default();
        // Tile (1, 2) covers samples 32..64 × 64..96; its centre is sample (48, 80), world (-16, 16).
        let brush = Brush {
            shape: BrushShape::Circle { radius: 5.0 },
            mode: BrushMode::Raise { amount: 12.0 },
            ..Brush::default()
        };
        let record = brush::stroke_heights(&mut t.map, &brush, &[Vec2::new(-16.0, 16.0)], tile_size);
        edits.note(&t.map, &mut t.minmax, record.dirty, &record.tiles);
        let tiles = edits.take_unsaved_tiles();
        assert_eq!(tiles.iter().copied().collect::<Vec<_>>(), vec![(1, 2)]);
        let n = recook_tiles(&mut cooked, &t.map, &tiles, tile_size).expect("recook");
        assert_eq!(n, 1);
        let full = cook_all_tiles(&t.map, tile_size).expect("cook");
        assert_eq!(cooked, full, "incremental re-cook equals a full cook");
        let changed: Vec<(u32, u32)> = cooked
            .tiles
            .iter()
            .zip(&before.tiles)
            .filter(|(a, b)| a != b)
            .map(|(a, _)| (a.x, a.y))
            .collect();
        assert_eq!(changed, vec![(1, 2)]);
        assert!(!record.is_empty());
        // Cells past the map edge touch no tile; a tile missing from the cooked file is an error.
        assert_eq!(
            recook_tiles(&mut cooked, &t.map, &[(9, 9)].into_iter().collect(), tile_size),
            Ok(0)
        );
        cooked.tiles.retain(|tile| (tile.x, tile.y) != (1, 2));
        assert_eq!(
            recook_tiles(&mut cooked, &t.map, &tiles, tile_size),
            Err(RecookError::MissingTile { x: 1, y: 2 })
        );
    }

    #[test]
    fn layer_weight_stroke_and_undo() {
        let mut t = terrain(65, 64.0);
        let mut paint = TerrainPaint(LayerWeightMap::new(65, 65, 2).expect("map"));
        let mut edits = TerrainEdits::default();
        let brush = Brush {
            shape: BrushShape::Circle { radius: 6.0 },
            mode: BrushMode::Assign { value: 1.0 },
            target: BrushTarget::LayerWeight { layer: 1 },
            hardness: 1.0,
            ..Brush::default()
        };
        let undo = apply_stroke(&mut t, Some(&mut paint), &mut edits, &brush, &[Vec2::ZERO]).expect("paint");
        assert_eq!(paint.0.get(1, 32, 32), 255);
        assert!(edits.take_paint_dirty().is_some());
        assert!(
            edits.take_gpu_dirty().is_none(),
            "painting leaves the heights alone"
        );
        undo.restore(&mut t, Some(&mut paint), None, &mut edits, true);
        assert!(paint.0.is_blank());
        undo.restore(&mut t, Some(&mut paint), None, &mut edits, false);
        assert_eq!(paint.0.get(1, 32, 32), 255);
    }

    #[test]
    fn enormous_finite_brushes_and_far_points_change_nothing() {
        // Finite values the validator accepts but no map holds: no overflow, no edit.
        let mut t = terrain(33, 32.0);
        let before = t.map.heights().to_vec();
        let mut edits = TerrainEdits::default();
        for (shape, path) in [
            (BrushShape::Circle { radius: 1.0e12 }, vec![Vec2::ZERO]),
            (
                BrushShape::Rectangle {
                    width: 3.0e38,
                    length: 1.0,
                },
                vec![Vec2::ZERO],
            ),
            (
                BrushShape::Circle { radius: 2.0 },
                vec![Vec2::new(1.0e12, -1.0e12)],
            ),
            (
                BrushShape::Circle { radius: 2.0 },
                vec![Vec2::new(-3.0e38, 0.0), Vec2::new(3.0e38, 0.0)],
            ),
        ] {
            let brush = Brush {
                shape,
                mode: BrushMode::Raise { amount: 5.0 },
                ..Brush::default()
            };
            let undo = apply_stroke(&mut t, None, &mut edits, &brush, &path).expect("a valid brush");
            assert!(
                undo.is_empty(),
                "{shape:?} at {path:?} changed {} samples",
                undo.len()
            );
        }
        assert_eq!(t.map.heights(), before.as_slice());
    }

    #[test]
    fn a_paint_map_of_another_size_is_refused() {
        // Painted weights are per heightfield sample: a 33 × 33 map on a 65 × 65 heightfield
        // would take the stroke's sample coordinates for its own and paint the wrong place.
        let mut t = terrain(65, 64.0);
        let mut paint = TerrainPaint(LayerWeightMap::new(33, 33, 2).expect("map"));
        let mut edits = TerrainEdits::default();
        let brush = Brush {
            shape: BrushShape::Circle { radius: 6.0 },
            mode: BrushMode::Assign { value: 1.0 },
            target: BrushTarget::LayerWeight { layer: 1 },
            ..Brush::default()
        };
        let err = apply_stroke(&mut t, Some(&mut paint), &mut edits, &brush, &[Vec2::ZERO]);
        assert!(
            matches!(
                err,
                Err(TerrainEditError::PaintSizeMismatch {
                    paint: [33, 33],
                    map: [65, 65]
                })
            ),
            "{err:?}"
        );
        assert!(paint.0.is_blank(), "nothing was painted");
        assert!(edits.take_paint_dirty().is_none());
    }

    #[test]
    fn outline_follows_the_surface_and_closes() {
        let t = terrain(65, 64.0);
        let ring = brush_outline(
            &t.map,
            Vec2::new(3.0, -2.0),
            &BrushShape::Circle { radius: 5.0 },
            32,
        );
        assert_eq!(ring.len(), 33);
        assert_eq!(ring.first(), ring.last());
        for p in &ring {
            let r = Vec2::new(p.x - 3.0, p.z + 2.0).length();
            assert!((r - 5.0).abs() < 1e-3);
            assert!((p.y - t.map.sample_bilinear(p.x, p.z) - OUTLINE_LIFT_M).abs() < 1e-4);
        }
        let rect = brush_outline(
            &t.map,
            Vec2::ZERO,
            &BrushShape::Rectangle {
                width: 4.0,
                length: 2.0,
            },
            16,
        );
        assert_eq!(rect.len(), 17);
        assert!(
            rect.iter()
                .all(|p| p.x.abs() <= 2.0 + 1e-5 && p.z.abs() <= 1.0 + 1e-5)
        );
    }

    #[test]
    fn raycast_hits_the_surface() {
        let t = terrain(65, 64.0);
        let hit = raycast(
            &t.map,
            glam::Vec3::new(3.0, 400.0, -5.0),
            glam::Vec3::NEG_Y,
            1000.0,
        )
        .expect("hit");
        assert!((hit.x - 3.0).abs() < 1e-4 && (hit.z + 5.0).abs() < 1e-4);
        assert!((hit.y - t.map.sample_bilinear(3.0, -5.0)).abs() < 1e-2);
        assert!(raycast(&t.map, glam::Vec3::new(3.0, 400.0, -5.0), glam::Vec3::Y, 1000.0).is_none());
        assert!(
            raycast(
                &t.map,
                glam::Vec3::new(500.0, 400.0, 0.0),
                glam::Vec3::NEG_Y,
                1000.0
            )
            .is_none()
        );
    }

    #[test]
    fn height_patch_changes_only_its_rect_and_restores_bit_for_bit() {
        let mut t = terrain(97, 96.0);
        let original = t.map.heights().to_vec();
        let mut edits = TerrainEdits::default();
        let rect = DirtyRect {
            col_start: 30,
            col_end: 40,
            row_start: 5,
            row_end: 9,
        };
        let samples: Vec<f32> = (0..40).map(|i| 50.0 + i as f32 * 0.125).collect();
        let undo = apply_height_patch(&mut t, &mut edits, rect, &samples).expect("patch");
        assert_eq!(edits.take_gpu_dirty(), Some(rect));
        assert_eq!(undo.tiles, BTreeSet::from([(0, 0), (1, 0)]));
        let w = t.map.width() as usize;
        for (i, (now, was)) in t.map.heights().iter().zip(&original).enumerate() {
            let (c, r) = ((i % w) as u32, (i / w) as u32);
            let inside = (30..40).contains(&c) && (5..9).contains(&r);
            if inside {
                assert_eq!(*now, samples[((r - 5) * 10 + (c - 30)) as usize]);
            } else {
                assert_eq!(now.to_bits(), was.to_bits(), "sample {c},{r} changed");
            }
        }
        undo.restore(&mut t, &mut edits, true);
        assert!(
            t.map
                .heights()
                .iter()
                .zip(&original)
                .all(|(a, b)| a.to_bits() == b.to_bits())
        );
        undo.restore(&mut t, &mut edits, false);
        assert_eq!(t.map.height_at_sample(31, 6), samples[11]);
    }

    #[test]
    fn bad_height_patches_leave_the_terrain_alone() {
        let mut t = terrain(33, 32.0);
        let original = t.map.heights().to_vec();
        let mut edits = TerrainEdits::default();
        let rect = |c0, c1, r0, r1| DirtyRect {
            col_start: c0,
            col_end: c1,
            row_start: r0,
            row_end: r1,
        };
        for (r, n) in [
            (rect(0, 0, 0, 2), 0),
            (rect(30, 34, 0, 1), 4),
            (rect(0, 2, 0, 2), 3),
        ] {
            let samples = vec![1.0; n];
            assert!(matches!(
                apply_height_patch(&mut t, &mut edits, r, &samples),
                Err(TerrainEditError::BadPatch(_))
            ));
        }
        let nan = [1.0, f32::NAN, 1.0, 1.0];
        assert!(apply_height_patch(&mut t, &mut edits, rect(0, 2, 0, 2), &nan).is_err());
        assert_eq!(t.map.heights(), &original[..]);
        assert_eq!(edits.take_gpu_dirty(), None);
    }

    /// ADR-2435: a 10 km rebase in the middle of a stroke. The local points before it and after
    /// it name the same world points through the origin, so the stroke edits the same samples,
    /// bit for bit, as one made with no rebase.
    #[test]
    fn a_stroke_across_a_rebase_edits_the_same_texels() {
        let brush = Brush {
            shape: BrushShape::Circle { radius: 6.0 },
            mode: BrushMode::Raise { amount: 2.0 },
            ..Brush::default()
        };
        let world = [
            Vec2::new(-20.0, 5.0),
            Vec2::new(4.5, 12.25),
            Vec2::new(22.0, -18.0),
        ];
        let mut reference = terrain(129, 128.0);
        let mut reference_edits = TerrainEdits::default();
        let expected =
            apply_stroke(&mut reference, None, &mut reference_edits, &brush, &world).expect("stroke");

        let before = OriginOffset::default();
        let after = OriginOffset([10_240.0, 0.0, -5_120.0]);
        let mut session = TerrainStrokeSession::detached(brush);
        session.push_local(terrain_to_local(&before, world[0]), &before);
        session.push_local(terrain_to_local(&before, world[1]), &before);
        // The rebase: local positions move by -shift.
        let moved = Vec2::new(world[2].x - 10_240.0, world[2].y + 5_120.0);
        session.push_local(moved, &after);
        assert_eq!(session.path(), world);
        let mut edited = terrain(129, 128.0);
        let mut edits = TerrainEdits::default();
        let undo = session
            .finish_with(&mut edited, None, &mut edits, None)
            .expect("stroke");
        assert_eq!(
            edited.map.heights(),
            reference.map.heights(),
            "same samples, same values"
        );
        assert_eq!(undo.dirty(), expected.dirty());
        assert_eq!(undo.tiles(), expected.tiles());

        // The same local points without the origin edit somewhere else (or nothing at all).
        let mut wrong = terrain(129, 128.0);
        let mut wrong_edits = TerrainEdits::default();
        let raw = [world[0], world[1], moved];
        apply_stroke(&mut wrong, None, &mut wrong_edits, &brush, &raw).expect("stroke");
        assert_ne!(wrong.map.heights(), reference.map.heights());

        // apply_stroke_local is the same conversion in one call.
        let mut local = terrain(129, 128.0);
        let mut local_edits = TerrainEdits::default();
        let path: Vec<Vec2> = world.iter().map(|p| terrain_to_local(&after, *p)).collect();
        apply_stroke_local(&mut local, None, &mut local_edits, &brush, &path, &after).expect("stroke");
        assert_eq!(local.map.heights(), reference.map.heights());
    }

    /// ADR-2435: an open session holds `terrain_brush_stroke` in `RebaseBlockers`; finishing,
    /// cancelling or failing lets it go, and two sessions keep it until both end.
    #[test]
    fn an_open_stroke_session_blocks_a_rebase_until_it_ends() {
        let mut world = World::new();
        let entity = world.spawn(terrain(65, 64.0)).id();
        let blocked = |world: &World| {
            world
                .get_resource::<RebaseBlockers>()
                .is_some_and(|b| b.0.contains(TERRAIN_STROKE_BLOCKER))
        };
        assert!(!blocked(&world));
        let brush = Brush {
            shape: BrushShape::Circle { radius: 4.0 },
            mode: BrushMode::Raise { amount: 1.0 },
            ..Brush::default()
        };
        let mut first = TerrainStrokeSession::begin(&mut world, brush);
        let second = TerrainStrokeSession::begin(&mut world, brush);
        assert!(first.holds_rebase() && blocked(&world));
        first.push_local(Vec2::new(1.0, 2.0), &OriginOffset::default());
        let undo = first.finish(&mut world, entity).expect("stroke");
        assert!(!undo.is_empty());
        assert!(world.get::<TerrainEdits>(entity).is_some(), "edits added");
        assert!(blocked(&world), "the second session still holds it");
        second.cancel(&mut world);
        assert!(!blocked(&world));

        // A failed finish lets go too.
        let failing = TerrainStrokeSession::begin(&mut world, brush);
        let nobody = world.spawn_empty().id();
        assert!(matches!(
            failing.finish(&mut world, nobody),
            Err(StrokeSessionError::NoTerrain(_))
        ));
        assert!(!blocked(&world));
        assert!(!TerrainStrokeSession::detached(brush).holds_rebase());
    }

    /// deep-zai-terrain-stream #5: with no editor draining the log, session strokes keep only the
    /// newest `STROKE_LOG_CAP` entries; under an editor every one stays.
    #[test]
    fn session_strokes_keep_the_stroke_log_capped_without_an_editor() {
        for (drained, kept) in [(false, 256usize), (true, 300)] {
            let mut world = World::new();
            world.insert_resource(TerrainStrokeLog {
                drained,
                ..TerrainStrokeLog::default()
            });
            let entity = world.spawn(terrain(65, 64.0)).id();
            for i in 0..300 {
                // Alternate the direction so every stroke changes samples.
                let amount = if i % 2 == 0 { 1.0 } else { -1.0 };
                let brush = Brush {
                    shape: BrushShape::Circle { radius: 4.0 },
                    mode: BrushMode::Raise { amount },
                    ..Brush::default()
                };
                let mut session = TerrainStrokeSession::begin(&mut world, brush);
                session.push_local(Vec2::new(1.0, 2.0), &OriginOffset::default());
                let undo = session.finish(&mut world, entity).expect("stroke");
                assert!(!undo.is_empty());
            }
            assert_eq!(
                world.resource::<TerrainStrokeLog>().strokes.len(),
                kept,
                "drained = {drained}"
            );
        }
    }

    #[test]
    fn local_raycasts_and_outlines_go_through_the_origin() {
        let t = terrain(65, 64.0);
        let origin = OriginOffset([2_048.0, 0.0, -1_024.0]);
        let offset = glam::Vec3::new(2_048.0, 0.0, -1_024.0);
        let eye = glam::Vec3::new(3.0, 300.0, -4.0);
        let down = glam::Vec3::NEG_Y;
        let hit = raycast(&t.map, eye, down, 1_000.0).expect("hit");
        let local = raycast_local(&t.map, eye - offset, down, 1_000.0, &origin).expect("local hit");
        assert_eq!(local, hit - offset);
        let shape = BrushShape::Circle { radius: 5.0 };
        let ring = brush_outline(&t.map, Vec2::new(3.0, -4.0), &shape, 16);
        let local_ring = brush_outline_local(
            &t.map,
            Vec2::new(3.0 - 2_048.0, -4.0 + 1_024.0),
            &shape,
            16,
            &origin,
        );
        assert_eq!(local_ring, ring.iter().map(|p| *p - offset).collect::<Vec<_>>());
        assert_eq!(
            local_height_to_terrain(&OriginOffset([0.0, 512.0, 0.0]), 3.0),
            515.0
        );
        assert_eq!(
            local_to_terrain(&origin, terrain_to_local(&origin, Vec2::new(7.25, -9.5))),
            Vec2::new(7.25, -9.5)
        );
    }

    /// A rectangle wider than the map writes the samples that land on it. The ones past
    /// the last column used to spill into the next row.
    #[test]
    fn a_patch_wider_than_the_map_does_not_spill() {
        let mut map = TerrainMap::from_heights(4, 4, vec![1.0; 16], 10.0, 400.0, 0.0).expect("map");
        let wide = DirtyRect {
            col_start: 3,
            col_end: 6,
            row_start: 0,
            row_end: 1,
        };
        write_height_samples(&mut map, wide, &[9.0, 9.0, 9.0]);
        let mut expect = [1.0f32; 16];
        expect[3] = 9.0;
        for (i, h) in expect.iter().enumerate() {
            assert_eq!(map.height_at_index(i), *h, "sample {i}");
        }
    }

    /// Rows past the map are not samples. Writing them used to index off the end of the
    /// height buffer.
    #[test]
    fn a_patch_past_the_last_row_does_not_panic() {
        let mut map = TerrainMap::from_heights(4, 4, vec![1.0; 16], 10.0, 400.0, 0.0).expect("map");
        let past = DirtyRect {
            col_start: 0,
            col_end: 2,
            row_start: 3,
            row_end: 6,
        };
        write_height_samples(&mut map, past, &[8.0; 6]);
        let mut expect = [1.0f32; 16];
        expect[12] = 8.0;
        expect[13] = 8.0;
        for (i, h) in expect.iter().enumerate() {
            assert_eq!(map.height_at_index(i), *h, "sample {i}");
        }
    }

    /// An empty or inverted rectangle writes nothing (`i % 0` would abort in release).
    #[test]
    fn writing_an_empty_rectangle_is_a_no_op() {
        let mut map = TerrainMap::from_heights(4, 4, vec![1.0; 16], 10.0, 400.0, 0.0).expect("map");
        let empty = DirtyRect {
            col_start: 0,
            col_end: 0,
            row_start: 0,
            row_end: 1,
        };
        write_height_samples(&mut map, empty, &[5.0]);
        let inverted = DirtyRect {
            col_start: 3,
            col_end: 1,
            row_start: 0,
            row_end: 1,
        };
        write_height_samples(&mut map, inverted, &[5.0]);
        assert!((0..16).all(|i| map.height_at_index(i) == 1.0));
    }
}
