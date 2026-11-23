// SPDX-License-Identifier: MPL-2.0
//! The viewport's terrain tool: sculpt and paint a [`Terrain`] with brush strokes.
//!
//! Interaction design after Fyrox's editor terrain mode (`editor/src/interaction/terrain.rs`,
//! MIT; design only, no code): pressing the left button over the terrain starts a stroke on a
//! background brush thread ([`regia_terrain::brush::BrushThread`]), dragging smears the brush
//! from the last position, releasing ends the stroke. Shift inverts the brush (Raise ↔ Lower,
//! Assign `v` ↔ `1 - v`). Flatten picks the height under the cursor when the stroke starts.
//! Computed samples come back in batches and are written into the terrain each frame; the
//! renderer re-uploads only their dirty rectangle. When the stroke ends its
//! [`regia_terrain::StrokeRecord`] becomes **one** [`TerrainStrokeCommand`] on the
//! [`UndoStack`]; undo restores the samples bit for bit. Strokes from the agent method
//! `terrain.stroke` arrive through [`TerrainStrokeLog`] and get the same undo entries.
//!
//! Hotkeys while the viewport has focus: `T` toggles the tool; with it on, `[` / `]` shrink /
//! grow the brush, `-` / `=` lower / raise its strength, `1` Raise, `2` Lower, `3` Flatten,
//! `4` Smooth, `5` paint the current layer, `6` Noise (a new seed each stroke), `7` Ramp (press at
//! the start, release at the end: the ramp runs between the heights under both points, ADR-1853),
//! `8` Hole (press and release cut a round hole of the brush's size, a drag cuts a slot along it,
//! Shift removes the named hole under the release point), `9` Scatter (paint instances of a scatter
//! class at `-` / `=` density, Shift erases, one undo step per drag, ADR-1936), `,` / `.` previous /
//! next layer (in Scatter, class).
//!
//! The brush outline follows the terrain surface ([`regia_terrain::brush_outline`]): the
//! viewport paints it as an overlay, and [`TerrainBrushPreview`] carries it to the runtime
//! gizmo pass (`regia_terrain::TerrainPlugin` draws it with `Gizmos` when a renderer runs).
//! On save, [`save_terrain_edits`] re-cooks only the cook tiles the strokes touched and writes
//! the `.fckd`, which the asset hot-reload watcher picks up; [`save_terrain_files`] adds the holes
//! (`<map>.holes.ron`), the painted weights (`<map>.weights.bin`) and the painted instances
//! (`<map>.scatterpaint.bin`) next to the level, and every save a user asks for runs it
//! ([`crate::level_io::save_level_and_terrain`]). The painted instances load with the level.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use regia_asset::AssetId;
use regia_core::math::{IVec2, Vec2, Vec3};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_render::LabVertex;
use regia_terrain::brush::{
    BrushSender, BrushThread, BrushThreadEvent, BrushValue, GridSnapshot, StrokeBatch, StrokeRecord,
    StrokeSample, StrokeSource, TileCoord, map_scale, world_to_sample,
};
use regia_terrain::{
    Brush, BrushMode, BrushShape, BrushTarget, DEFAULT_TILE_SIZE, EDIT_CELL_SIZE, HoleEllipse, HoleShape,
    HolesError, HolesMask, MAX_PAINT_DENSITY, NamedHole, PaintedScatter, ScatterBrush, ScatterBrushMode,
    ScatterSet, ScatterStroke, StrokeUndo, Terrain, TerrainBrushPreview, TerrainEdits, TerrainHoles,
    TerrainPaint, TerrainScatterPaint, TerrainStrokeLog, apply_scatter_stroke, holes_ron_path,
    layer_weights_path, load_holes_ron, load_scatter_paint, paintable_scatter_set, reseat_terrain_scatter,
    scatter_paint_path,
};

use crate::command::{EditorCommand, UndoStack};

/// The terrain tool's state (a world resource; the viewport inserts it on first use).
#[derive(Resource)]
pub struct TerrainTool {
    /// The tool is on: left-drag sculpts instead of panning/picking.
    pub active: bool,
    /// The brush the next stroke uses.
    pub brush: Brush,
    /// Terrain point under the cursor, world XZ.
    pub hover: Option<Vec2>,
    stroke: Option<ActiveStroke>,
    /// The Hole mode (`8`): a drag cuts a named hole instead of running the brush.
    pub cut_holes: bool,
    /// A Ramp or Hole drag in progress (applied on release).
    drag: Option<Drag>,
    /// The terrains whose holes changed since the last [`save_terrain_holes`].
    holes_dirty: BTreeSet<Entity>,
    /// The terrains whose `<map>.holes.ron` this session has already brought in.
    holes_loaded: BTreeSet<Entity>,
    /// The Scatter mode (`9`): the brush paints instances of a scatter class.
    pub paint_scatter: bool,
    /// The class the Scatter mode paints, an index into the scatter set (wraps).
    pub scatter_class: usize,
    /// The Scatter mode's target density, instances per square meter.
    pub scatter_density: f32,
    /// A Scatter drag in progress.
    scatter_drag: Option<ScatterDrag>,
    /// The level and terrain whose `<map>.scatterpaint.bin` was brought in.
    scatter_loaded: Option<(PathBuf, Entity)>,
    /// That file exists and cannot be read: scatter edits are refused, so it is never replaced.
    scatter_refused: bool,
    /// The Grass density mode (`0`, ADR-2411): the brush moves the painted grass density toward
    /// [`Self::density_target`] (Shift: toward the baked density).
    pub paint_density: bool,
    /// The density the Grass density mode paints toward, `0..=1` of the baked density.
    pub density_target: f32,
    /// A Grass density drag in progress.
    density_drag: Option<DensityDrag>,
    heights: Option<Mutex<BrushThread<f32>>>,
    weights: Option<Mutex<BrushThread<u8>>>,
    /// What the active stroke started from and what its batches wrote.
    partial: Option<PartialStroke>,
    /// Input side effects run once even when egui repeats a layout pass.
    input_frame: Option<u64>,
    input_panes: HashSet<egui::Id>,
    /// The brush keys ran this egui frame (once per frame, from the first pane drawn).
    keys_done: bool,
}

impl Default for TerrainTool {
    fn default() -> Self {
        Self {
            active: false,
            brush: Brush {
                shape: BrushShape::Circle { radius: 8.0 },
                mode: BrushMode::Raise { amount: 1.0 },
                hardness: 0.25,
                ..Brush::default()
            },
            hover: None,
            stroke: None,
            cut_holes: false,
            drag: None,
            holes_dirty: BTreeSet::new(),
            holes_loaded: BTreeSet::new(),
            paint_scatter: false,
            scatter_class: 0,
            scatter_density: 0.1,
            scatter_drag: None,
            scatter_loaded: None,
            scatter_refused: false,
            paint_density: false,
            density_target: 0.0,
            density_drag: None,
            heights: None,
            weights: None,
            partial: None,
            input_frame: None,
            input_panes: HashSet::new(),
            keys_done: false,
        }
    }
}

impl std::fmt::Debug for TerrainTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerrainTool")
            .field("active", &self.active)
            .field("brush", &self.brush)
            .field("hover", &self.hover)
            .field("stroking", &self.stroking())
            .finish()
    }
}

/// A Scatter drag: each piece of the path is applied at once and merged into one undo entry.
struct ScatterDrag {
    entity: Entity,
    brush: ScatterBrush,
    set: Arc<ScatterSet>,
    last: Vec2,
    record: Option<ScatterStroke>,
}

/// A Grass density drag: each piece is painted at once; the pieces are one undo entry.
struct DensityDrag {
    entity: Entity,
    stroke: regia_terrain::GrassDensityStroke,
    last: Vec2,
    pieces: Vec<regia_terrain::GrassDensityUndo>,
}

/// What a press-drag-release does when the button is released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragKind {
    /// A Ramp stroke from `from` to `to`.
    Ramp,
    /// A named hole along `from` .. `to`.
    Hole,
}

/// A drag applied on release, from `from` to `to` (world XZ).
#[derive(Debug, Clone, Copy)]
struct Drag {
    kind: DragKind,
    entity: Entity,
    from: Vec2,
    to: Vec2,
    invert: bool,
}

/// The stroke in progress (or ended and waiting for its record).
#[derive(Debug, Clone, Copy)]
struct ActiveStroke {
    entity: Entity,
    brush: Brush,
    /// Last brush position, sample coordinates.
    last: Vec2,
    /// Flatten target.
    value: f32,
    /// The end message was sent; the record is on its way.
    ended: bool,
    /// The end message could not be sent: no record will come.
    orphaned: bool,
}

/// The grid a threaded stroke started from.
#[derive(Debug)]
enum StrokeStart {
    Height(Arc<GridSnapshot<f32>>),
    Weight {
        layer: usize,
        plane: Arc<GridSnapshot<u8>>,
    },
}

/// Enough of a threaded stroke to build its undo entry when the worker dies before the record:
/// batches already written stay on the map, and a save would keep them.
#[derive(Debug)]
struct PartialStroke {
    start: StrokeStart,
    tiles: BTreeSet<TileCoord>,
    heights: WriteLedger<f32>,
    weights: WriteLedger<u8>,
}

impl PartialStroke {
    fn new(start: StrokeStart) -> Self {
        Self {
            start,
            tiles: BTreeSet::new(),
            heights: WriteLedger::default(),
            weights: WriteLedger::default(),
        }
    }

    fn note(&mut self, tiles: &BTreeSet<TileCoord>) {
        self.tiles.extend(tiles.iter().copied());
    }

    /// The undo entry of what landed, `None` when nothing did. Built from the write ledger, so it
    /// holds only samples this stroke wrote, with the values they had before its first write.
    fn undo(self, stroke: &ActiveStroke) -> Option<StrokeUndo> {
        let undo = match self.start {
            StrokeStart::Height(_) => StrokeUndo::Height(self.heights.record(stroke.brush, self.tiles)),
            StrokeStart::Weight { layer, .. } => StrokeUndo::LayerWeight {
                layer,
                record: self.weights.record(stroke.brush, self.tiles),
            },
        };
        (!undo.is_empty()).then_some(undo)
    }
}

/// A brush value whose change can land on top of a value someone else wrote.
trait MergeValue: Copy + PartialEq {
    /// `live` moved by the brush's change from `from` to `to`.
    fn merged(live: Self, from: Self, to: Self) -> Self;
}

impl MergeValue for f32 {
    fn merged(live: Self, from: Self, to: Self) -> Self {
        live + (to - from)
    }
}

impl MergeValue for u8 {
    fn merged(live: Self, from: Self, to: Self) -> Self {
        (i16::from(live) + i16::from(to) - i16::from(from)).clamp(0, 255) as u8
    }
}

/// One sample a threaded stroke wrote.
#[derive(Debug, Clone, Copy)]
struct LedgerSample<V> {
    /// The live value before the stroke's first write.
    before: V,
    /// The value the stroke last wrote.
    written: V,
    /// The value the brush last asked for (computed from the stroke-start snapshot).
    wanted: V,
}

/// The samples a threaded stroke wrote. The worker computes every batch from the snapshot taken
/// at stroke start; an edit made while the drag is open (an agent `terrain.stroke`, a height
/// patch) would be overwritten by the next batch and reverted again by the undo entry. The ledger
/// lands each batch on the live value instead and keeps the live value before the first write as
/// the undo `before` (deep-zai-terrain-stream #3). With no such edit every batch lands unchanged
/// and the worker's own record is the undo entry, bit for bit.
#[derive(Debug)]
struct WriteLedger<V> {
    samples: HashMap<IVec2, LedgerSample<V>>,
    /// Another writer changed a sample this stroke owns.
    foreign: bool,
}

impl<V> Default for WriteLedger<V> {
    fn default() -> Self {
        Self {
            samples: HashMap::new(),
            foreign: false,
        }
    }
}

impl<V: MergeValue> WriteLedger<V> {
    /// `batch` as it should land on the live grid read through `live`. `start` is the snapshot
    /// the worker reads.
    fn rebase(
        &mut self,
        batch: &StrokeBatch<V>,
        start: &impl StrokeSource<V>,
        live: impl Fn(IVec2) -> Option<V>,
    ) -> StrokeBatch<V> {
        let mut values = Vec::with_capacity(batch.values.len());
        for &(p, wanted) in &batch.values {
            let (Some(now), Some(known)) = (
                live(p),
                self.samples.get(&p).copied().or_else(|| {
                    start.read(p).map(|first| LedgerSample {
                        before: live(p).unwrap_or(first),
                        written: first,
                        wanted: first,
                    })
                }),
            ) else {
                // Off the grid: the write is ignored anyway.
                values.push((p, wanted));
                continue;
            };
            if now != known.written {
                self.foreign = true;
            }
            let out = if now == known.wanted {
                wanted
            } else {
                V::merged(now, known.wanted, wanted)
            };
            self.samples.insert(
                p,
                LedgerSample {
                    before: known.before,
                    written: out,
                    wanted,
                },
            );
            values.push((p, out));
        }
        StrokeBatch {
            values,
            dirty: batch.dirty,
            tiles: batch.tiles.clone(),
        }
    }

    /// The worker's `record` when nothing else wrote a sample of this stroke; otherwise the
    /// record of what this stroke wrote over the live values.
    fn finish(&self, record: StrokeRecord<V>) -> StrokeRecord<V> {
        if self.foreign {
            self.record(record.brush, record.tiles)
        } else {
            record
        }
    }

    /// Every sample this stroke changed: `before` its first write, `after` its last.
    fn record(&self, brush: Brush, tiles: BTreeSet<TileCoord>) -> StrokeRecord<V> {
        let mut samples: Vec<StrokeSample<V>> = self
            .samples
            .iter()
            .filter(|(_, s)| s.before != s.written)
            .map(|(p, s)| StrokeSample {
                position: *p,
                before: s.before,
                after: s.written,
            })
            .collect();
        samples.sort_unstable_by_key(|s| (s.position.y, s.position.x));
        let mut dirty = regia_terrain::DirtyRect {
            col_start: 0,
            col_end: 0,
            row_start: 0,
            row_end: 0,
        };
        for (i, s) in samples.iter().enumerate() {
            let (col, row) = (s.position.x.max(0) as u32, s.position.y.max(0) as u32);
            if i == 0 {
                dirty = regia_terrain::DirtyRect {
                    col_start: col,
                    col_end: col + 1,
                    row_start: row,
                    row_end: row + 1,
                };
            } else {
                dirty.col_start = dirty.col_start.min(col);
                dirty.col_end = dirty.col_end.max(col + 1);
                dirty.row_start = dirty.row_start.min(row);
                dirty.row_end = dirty.row_end.max(row + 1);
            }
        }
        StrokeRecord {
            brush,
            samples,
            dirty,
            tiles,
        }
    }
}

/// A height batch landed through the stroke's ledger (see [`WriteLedger`]).
fn rebase_height_batch(
    world: &World,
    entity: Entity,
    partial: Option<&mut PartialStroke>,
    batch: StrokeBatch<f32>,
) -> StrokeBatch<f32> {
    let Some(PartialStroke {
        start: StrokeStart::Height(start),
        heights,
        ..
    }) = partial
    else {
        return batch;
    };
    let Some(terrain) = world.get::<Terrain>(entity) else {
        return batch;
    };
    heights.rebase(&batch, &**start, |p| StrokeSource::read(&terrain.map, p))
}

/// A layer-weight batch landed through the stroke's ledger (see [`WriteLedger`]).
fn rebase_weight_batch(
    world: &World,
    entity: Entity,
    layer: usize,
    partial: Option<&mut PartialStroke>,
    batch: StrokeBatch<u8>,
) -> StrokeBatch<u8> {
    let Some(PartialStroke {
        start: StrokeStart::Weight { plane: start, .. },
        weights,
        ..
    }) = partial
    else {
        return batch;
    };
    let Some(paint) = world.get::<TerrainPaint>(entity) else {
        return batch;
    };
    let (width, height) = (paint.0.width(), paint.0.height());
    let Some(plane) = paint.0.plane(layer as u32) else {
        return batch;
    };
    weights.rebase(&batch, &**start, |p| {
        if p.x < 0 || p.y < 0 || p.x as u32 >= width || p.y as u32 >= height {
            return None;
        }
        plane.get(p.y as usize * width as usize + p.x as usize).copied()
    })
}

impl TerrainTool {
    /// `true` while a stroke is being drawn or its record has not arrived yet.
    pub fn stroking(&self) -> bool {
        self.stroke.is_some()
            || self.drag.is_some()
            || self.scatter_drag.is_some()
            || self.density_drag.is_some()
    }

    /// One-line status for the viewport ("Raise · r 8.0 m · 1.00").
    pub fn status(&self) -> String {
        let size = match self.brush.shape {
            BrushShape::Circle { radius } => format!("r {radius:.1} m"),
            BrushShape::Rectangle { width, length } => format!("{width:.1} × {length:.1} m"),
        };
        if self.paint_density {
            return format!(
                "Terrain grass density · {size} · target {:.2}   [ ] size  - = target  Shift restore",
                self.density_target
            );
        }
        if self.paint_scatter {
            return format!(
                "Terrain scatter · {size} · {:.3}/m² · class {}   [ ] size  - = density  , . class  Shift erase",
                self.scatter_density, self.scatter_class
            );
        }
        let strength = match self.brush.mode {
            BrushMode::Raise { amount } | BrushMode::Lower { amount } | BrushMode::Noise { amount, .. } => {
                format!("{amount:.2} m")
            }
            _ => format!("{:.2}", self.brush.alpha),
        };
        let target = match self.brush.target {
            BrushTarget::Height => String::new(),
            BrushTarget::LayerWeight { layer } => format!(" · layer {layer}"),
        };
        format!(
            "Terrain {} · {size} · {strength}{target}   [ ] size  - = strength  1-9 mode  Shift invert",
            if self.cut_holes {
                "hole"
            } else {
                self.brush.mode.name()
            }
        )
    }

    /// Scales the brush size by `factor`.
    pub fn scale_size(&mut self, factor: f32) {
        self.brush.shape.scale(factor);
    }

    /// Scales the strength: metres for Raise/Lower/Noise, the density for Scatter, opacity (0..1)
    /// otherwise.
    pub fn scale_strength(&mut self, factor: f32) {
        if self.paint_density {
            // `=` raises the target, `-` lowers it, in steps of 0.1 (factor > 1 is up).
            let step = if factor > 1.0 { 0.1 } else { -0.1 };
            self.density_target = (self.density_target + step).clamp(0.0, 1.0);
            return;
        }
        if self.paint_scatter {
            self.scatter_density = (self.scatter_density * factor).clamp(0.001, MAX_PAINT_DENSITY);
            return;
        }
        match &mut self.brush.mode {
            BrushMode::Raise { amount } | BrushMode::Lower { amount } | BrushMode::Noise { amount, .. } => {
                *amount = (*amount * factor).clamp(0.01, 1000.0)
            }
            _ => self.brush.alpha = (self.brush.alpha * factor).clamp(0.01, 1.0),
        }
    }

    /// Selects mode `n` (1 Raise, 2 Lower, 3 Flatten, 4 Smooth, 5 paint the current layer,
    /// 6 Noise, 7 Ramp, 8 Hole, 9 Scatter, 0 Grass density).
    pub fn select_mode(&mut self, n: u32) {
        if n == 0 || n == 8 || n == 9 {
            self.cut_holes = n == 8;
            self.paint_scatter = n == 9;
            self.paint_density = n == 0;
            return;
        }
        let amount = match self.brush.mode {
            BrushMode::Raise { amount } | BrushMode::Lower { amount } | BrushMode::Noise { amount, .. } => {
                amount.abs()
            }
            _ => 1.0,
        };
        let layer = match self.brush.target {
            BrushTarget::LayerWeight { layer } => layer,
            BrushTarget::Height => 0,
        };
        let (mode, target) = match n {
            1 => (BrushMode::Raise { amount }, BrushTarget::Height),
            2 => (BrushMode::Lower { amount }, BrushTarget::Height),
            3 => (BrushMode::Flatten, BrushTarget::Height),
            4 => (BrushMode::Smooth { kernel_radius: 2 }, BrushTarget::Height),
            5 => (
                BrushMode::Assign { value: 1.0 },
                BrushTarget::LayerWeight { layer },
            ),
            6 => (
                BrushMode::Noise {
                    amount,
                    wavelength: 8.0,
                    seed: 0,
                },
                BrushTarget::Height,
            ),
            // The ends and heights are filled in when the drag is released (`end_stroke`).
            7 => (
                BrushMode::Ramp {
                    start: [0.0; 2],
                    end: [0.0; 2],
                    start_value: 0.0,
                    end_value: 0.0,
                },
                BrushTarget::Height,
            ),
            _ => return,
        };
        self.cut_holes = false;
        self.paint_scatter = false;
        self.paint_density = false;
        self.brush.mode = mode;
        self.brush.target = target;
    }

    /// Moves the painted layer (the Scatter mode: class) by `delta`.
    pub fn step_layer(&mut self, delta: i32) {
        if self.paint_scatter {
            self.scatter_class = (self.scatter_class as i64 + i64::from(delta)).max(0) as usize;
            return;
        }
        if let BrushTarget::LayerWeight { layer } = &mut self.brush.target {
            *layer =
                (*layer as i64 + i64::from(delta)).clamp(0, regia_terrain::MAX_LAYERS as i64 - 1) as usize;
        }
    }
}

/// One stroke on the undo stack.
pub struct TerrainStrokeCommand {
    entity: Entity,
    undo: StrokeUndo,
    /// The edit-layer half of a layered stroke (ADR-7825).
    layer: Option<regia_terrain::LayerStrokeUndo>,
}

impl TerrainStrokeCommand {
    /// A command for a stroke already applied to `entity`.
    pub fn new(entity: Entity, undo: StrokeUndo) -> Self {
        Self {
            entity,
            undo,
            layer: None,
        }
    }

    /// A command for a layered stroke: the composite record and the edit-layer record (ADR-7825).
    pub fn with_layer(entity: Entity, undo: StrokeUndo, layer: regia_terrain::LayerStrokeUndo) -> Self {
        Self {
            entity,
            undo,
            layer: Some(layer),
        }
    }

    /// The stroke.
    pub fn stroke(&self) -> &StrokeUndo {
        &self.undo
    }

    fn restore(&self, world: &mut World, undo: bool) {
        // A terrain deleted and restored by undo has a new id; follow it.
        let Some(target) = crate::command::resolve_entity(world, self.entity.to_bits()) else {
            tracing::warn!(target: "regia_editor", "terrain stroke undo: entity {} is gone", self.entity.to_bits());
            return;
        };
        if self.layer.is_some()
            && let Err(err) = regia_terrain::edit_layers::reconcile_world_base(world, target)
        {
            tracing::warn!(target: "regia_editor", "terrain stroke undo: {err}");
            return;
        }
        if let Some(layer) = &self.layer
            && let Some(mut stack) = world.get_mut::<regia_terrain::TerrainEditLayers>(target)
            && !layer.restore(&mut stack, undo)
        {
            tracing::warn!(target: "regia_editor", "terrain stroke undo: its edit layer is gone");
        }
        let mut entity = world.entity_mut(target);
        if !entity.contains::<TerrainEdits>() {
            entity.insert(TerrainEdits::default());
        }
        if matches!(self.undo, StrokeUndo::Scatter(_)) && !entity.contains::<TerrainScatterPaint>() {
            entity.insert(TerrainScatterPaint::default());
        }
        let mut query = world.query::<(
            &mut Terrain,
            Option<&mut TerrainPaint>,
            Option<&mut TerrainScatterPaint>,
            &mut TerrainEdits,
            Option<&mut regia_terrain::TerrainEditLayers>,
        )>();
        if let Ok((mut terrain, paint, scatter, mut edits, stack)) = query.get_mut(world, target) {
            self.undo.restore(
                &mut terrain,
                paint.map(Mut::into_inner),
                scatter.map(Mut::into_inner),
                &mut edits,
                undo,
            );
            if self.layer.is_some()
                && let StrokeUndo::Height(record) = &self.undo
                && let Some(mut stack) = stack
                && let Err(err) = stack.recompute_terrain(&mut terrain, &mut edits, Some(record.dirty))
            {
                tracing::warn!(target: "regia_editor", "terrain stroke undo recompute: {err}");
            }
        }
    }
}

impl EditorCommand for TerrainStrokeCommand {
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
        if let StrokeUndo::Scatter(stroke) = &self.undo {
            return match stroke.mode {
                ScatterBrushMode::Paint => "Terrain scatter paint".to_owned(),
                ScatterBrushMode::Erase => "Terrain scatter erase".to_owned(),
            };
        }
        match self.undo.brush().target {
            BrushTarget::Height => format!("Terrain {}", self.undo.brush().mode.name()),
            BrushTarget::LayerWeight { layer } => format!("Terrain paint layer {layer}"),
        }
    }
}

/// The terrain the tool edits: the primary selection when it is a terrain, else the first one.
pub fn target_terrain(world: &mut World) -> Option<Entity> {
    let primary = world
        .get_resource::<crate::selection::Selection>()
        .and_then(crate::selection::Selection::primary);
    if let Some(e) = primary.filter(|e| world.get::<Terrain>(*e).is_some()) {
        return Some(e);
    }
    let mut query = world.query_filtered::<Entity, With<Terrain>>();
    query.iter(world).next()
}

/// The terrain point hit by a ray, world XYZ. A ray through a hole passes the cut
/// ([`regia_pick::raycast_terrain`]), as the drawn terrain does, and a floating-origin rebase
/// (`OriginOffset`) is honoured like in a game.
pub fn pick_terrain(world: &mut World, origin: Vec3, direction: Vec3) -> Option<(Entity, Vec3)> {
    let entity = target_terrain(world)?;
    let reach = world
        .get::<Terrain>(entity)
        .map(|terrain| terrain.map.world_width() * 4.0 + (origin.y - terrain.map.min_max_height().0).abs())?;
    let hit = regia_pick::raycast_terrain_entity(world, entity, origin, direction, reach)?;
    // The hit point is ray-local; the brush addresses the map in its world-anchored space.
    let offset = world
        .get_resource::<regia_scene::streaming::OriginOffset>()
        .map_or(Vec3::ZERO, |o| Vec3::from_array(o.0.map(|axis| axis as f32)));
    Some((entity, hit.point + offset))
}

/// Sends a stamp (no `prev`) or a smear to the brush thread.
fn send_brush<V>(
    sender: &BrushSender<V>,
    brush: &Brush,
    prev: Option<Vec2>,
    at: Vec2,
    scale: Vec2,
    value: f32,
) {
    let mut alive = true;
    let mut draw = |p, a| alive &= sender.draw_pixel(p, a, value);
    match prev {
        None => brush.stamp(at, scale, &mut draw),
        Some(start) => brush.smear(start, at, scale, &mut draw),
    }
    if !alive {
        tracing::warn!(target: "regia_editor", "terrain brush thread is gone; stroke dropped");
    }
}

fn thread<V: BrushValue>(slot: &mut Option<Mutex<BrushThread<V>>>) -> Option<&Mutex<BrushThread<V>>> {
    if slot.is_none() {
        match BrushThread::<V>::spawn() {
            Ok(t) => *slot = Some(Mutex::new(t)),
            Err(err) => {
                tracing::error!(target: "regia_editor", "terrain brush thread failed to start: {err}");
                return None;
            }
        }
    }
    slot.as_ref()
}

/// Starts a stroke on `entity` at world XZ `at`. `invert` is the Shift modifier.
/// Returns `false` (and does nothing) when the terrain or the painted layer is missing.
pub fn begin_stroke(world: &mut World, entity: Entity, at: Vec2, invert: bool) -> bool {
    if world.get_resource::<TerrainTool>().is_none() {
        world.init_resource::<TerrainTool>();
    }
    if let Ok(mut e) = world.get_entity_mut(entity)
        && e.contains::<Terrain>()
        && !e.contains::<TerrainEdits>()
    {
        e.insert(TerrainEdits::default());
    }
    world.resource_scope::<TerrainTool, bool>(|world, mut tool| {
        if tool.stroke.is_some() || tool.scatter_drag.is_some() || tool.density_drag.is_some() {
            return false;
        }
        if world.get::<Terrain>(entity).is_none() {
            return false;
        }
        if tool.paint_density {
            return begin_density(world, &mut tool, entity, at, invert);
        }
        if tool.paint_scatter {
            return begin_scatter(world, &mut tool, entity, at, invert);
        }
        // A height stroke writes the active edit layer, which must take it (ADR-7825).
        if !tool.cut_holes
            && tool.brush.target == BrushTarget::Height
            && let Err(err) = crate::terrain_layers::check_stroke_target(world, entity)
        {
            tracing::warn!(target: "regia_editor", "terrain stroke refused: {err}");
            return false;
        }
        let drag = if tool.cut_holes {
            Some(DragKind::Hole)
        } else if matches!(tool.brush.mode, BrushMode::Ramp { .. })
            && tool.brush.target == BrushTarget::Height
        {
            Some(DragKind::Ramp)
        } else {
            None
        };
        if let Some(kind) = drag {
            tool.drag = Some(Drag {
                kind,
                entity,
                from: at,
                to: at,
                invert,
            });
            return true;
        }
        // Each noise stroke draws a new field, so repeated strokes do not stack one pattern.
        if let BrushMode::Noise { seed, .. } = &mut tool.brush.mode {
            *seed = seed.wrapping_add(1);
        }
        let Some(terrain) = world.get::<Terrain>(entity) else {
            return false;
        };
        let mut brush = tool.brush;
        if invert {
            brush.mode = brush.mode.inverted();
        }
        let map = &terrain.map;
        let scale = map_scale(map);
        let s = world_to_sample(map, at.x, at.y);
        let value = map.sample_bilinear(at.x, at.y);
        let start = match brush.target {
            BrushTarget::Height => {
                let snapshot = Arc::new(GridSnapshot::capture(map));
                let Some(t) = thread(&mut tool.heights) else {
                    return false;
                };
                let t = t.lock().unwrap_or_else(PoisonError::into_inner);
                if !t.sender().start_stroke(brush, Arc::clone(&snapshot), EDIT_CELL_SIZE) {
                    return false;
                }
                send_brush(t.sender(), &brush, None, s, scale, value);
                StrokeStart::Height(snapshot)
            }
            BrushTarget::LayerWeight { layer } => {
                let Some(plane) = world
                    .get::<TerrainPaint>(entity)
                    .filter(|p| p.0.width() == map.width() && p.0.height() == map.height())
                    .and_then(|p| p.0.plane(layer as u32).map(<[u8]>::to_vec))
                else {
                    tracing::warn!(target: "regia_editor", "terrain has no painted layer {layer} of its size");
                    return false;
                };
                let size = regia_core::math::UVec2::new(map.width(), map.height());
                let Some(snapshot) = GridSnapshot::new(size, plane) else {
                    return false;
                };
                let snapshot = Arc::new(snapshot);
                let Some(t) = thread(&mut tool.weights) else {
                    return false;
                };
                let t = t.lock().unwrap_or_else(PoisonError::into_inner);
                if !t.sender().start_stroke(brush, Arc::clone(&snapshot), EDIT_CELL_SIZE) {
                    return false;
                }
                send_brush(t.sender(), &brush, None, s, scale, value);
                StrokeStart::Weight {
                    layer,
                    plane: snapshot,
                }
            }
        };
        tool.partial = Some(PartialStroke::new(start));
        tool.stroke = Some(ActiveStroke {
            entity,
            brush,
            last: s,
            value,
            ended: false,
            orphaned: false,
        });
        true
    })
}

/// Starts a Scatter drag: the class's brush at the tool's size and density, an eraser with Shift.
fn begin_scatter(world: &mut World, tool: &mut TerrainTool, entity: Entity, at: Vec2, invert: bool) -> bool {
    if tool.scatter_refused {
        tracing::warn!(target: "regia_editor", "the level's painted scatter file cannot be read; scatter edits are off");
        return false;
    }
    let Some(set) = paintable_scatter_set(world).filter(|set| !set.classes.is_empty()) else {
        tracing::warn!(target: "regia_editor", "no scatter set to paint from");
        return false;
    };
    let class = &set.classes[tool.scatter_class % set.classes.len()];
    let mut brush = if invert {
        ScatterBrush::eraser(Some(&class.name))
    } else {
        ScatterBrush::for_class(class)
    };
    brush.shape = tool.brush.shape;
    brush.hardness = tool.brush.hardness;
    brush.strength = tool.brush.alpha;
    brush.density = tool.scatter_density;
    if !world.entity(entity).contains::<TerrainScatterPaint>() {
        world.entity_mut(entity).insert(TerrainScatterPaint::default());
    }
    let mut drag = ScatterDrag {
        entity,
        brush,
        set,
        last: at,
        record: None,
    };
    paint_scatter_piece(world, &mut drag, &[at]);
    tool.scatter_drag = Some(drag);
    true
}

/// Starts a Grass density drag: the tool's size and hardness, its opacity as the strength, toward
/// the target (Shift: toward the baked density, 1).
fn begin_density(world: &mut World, tool: &mut TerrainTool, entity: Entity, at: Vec2, invert: bool) -> bool {
    let radius = match tool.brush.shape {
        BrushShape::Circle { radius } => radius,
        BrushShape::Rectangle { width, length } => width.max(length) * 0.5,
    };
    let stroke = regia_terrain::GrassDensityStroke {
        path: vec![[at.x, at.y]],
        radius_m: radius,
        hardness: tool.brush.hardness.clamp(0.0, 1.0),
        strength: tool.brush.alpha.clamp(0.0, 1.0),
        target: if invert { 1.0 } else { tool.density_target },
    };
    let mut drag = DensityDrag {
        entity,
        stroke,
        last: at,
        pieces: Vec::new(),
    };
    paint_density_piece(world, &mut drag, &[at]);
    tool.density_drag = Some(drag);
    true
}

/// Applies one piece of a Grass density drag and keeps its record.
fn paint_density_piece(world: &mut World, drag: &mut DensityDrag, path: &[Vec2]) {
    let mut stroke = drag.stroke.clone();
    stroke.path = path.iter().map(|p| [p.x, p.y]).collect();
    match crate::terrain_panel::paint_grass_density_piece(world, drag.entity, &stroke) {
        Ok(piece) if piece.changed > 0 => drag.pieces.push(piece),
        Ok(_) => {}
        Err(err) => tracing::warn!(target: "regia_editor", "grass density stroke refused: {err}"),
    }
}

/// Applies one piece of a Scatter drag and merges its record.
fn paint_scatter_piece(world: &mut World, drag: &mut ScatterDrag, path: &[Vec2]) {
    let mut query = world.query::<(&Terrain, &mut TerrainScatterPaint, &mut TerrainEdits)>();
    let Ok((terrain, mut paint, mut edits)) = query.get_mut(world, drag.entity) else {
        return;
    };
    match apply_scatter_stroke(terrain, &mut paint, &mut edits, &drag.set, &drag.brush, path) {
        Ok(StrokeUndo::Scatter(piece)) => match &mut drag.record {
            Some(record) => record.merge(piece),
            None => drag.record = Some(piece),
        },
        Ok(_) => {}
        Err(err) => tracing::warn!(target: "regia_editor", "scatter stroke refused: {err}"),
    }
}

/// Drags the active stroke to world XZ `at`.
pub fn continue_stroke(world: &mut World, at: Vec2) {
    let density = world
        .get_resource_mut::<TerrainTool>()
        .and_then(|mut t| t.density_drag.take());
    if let Some(mut drag) = density {
        if drag.last.distance_squared(at) > 1e-4 {
            let last = drag.last;
            paint_density_piece(world, &mut drag, &[last, at]);
            drag.last = at;
        }
        world.resource_mut::<TerrainTool>().density_drag = Some(drag);
        return;
    }
    let scatter = world
        .get_resource_mut::<TerrainTool>()
        .and_then(|mut t| t.scatter_drag.take());
    if let Some(mut drag) = scatter {
        if drag.last.distance_squared(at) > 1e-4 {
            let last = drag.last;
            paint_scatter_piece(world, &mut drag, &[last, at]);
            drag.last = at;
        }
        world.resource_mut::<TerrainTool>().scatter_drag = Some(drag);
        return;
    }
    if let Some(mut tool) = world.get_resource_mut::<TerrainTool>()
        && let Some(drag) = tool.drag.as_mut()
    {
        drag.to = at;
        return;
    }
    let Some(stroke) = world
        .get_resource::<TerrainTool>()
        .and_then(|t| t.stroke)
        .filter(|s| !s.ended)
    else {
        return;
    };
    let Some(terrain) = world.get::<Terrain>(stroke.entity) else {
        return;
    };
    let scale = map_scale(&terrain.map);
    let s = world_to_sample(&terrain.map, at.x, at.y);
    if s.distance_squared(stroke.last) < 0.01 {
        return;
    }
    let Some(mut tool) = world.get_resource_mut::<TerrainTool>() else {
        return;
    };
    match stroke.brush.target {
        BrushTarget::Height => {
            if let Some(t) = &tool.heights {
                let t = t.lock().unwrap_or_else(PoisonError::into_inner);
                send_brush(
                    t.sender(),
                    &stroke.brush,
                    Some(stroke.last),
                    s,
                    scale,
                    stroke.value,
                );
            }
        }
        BrushTarget::LayerWeight { .. } => {
            if let Some(t) = &tool.weights {
                let t = t.lock().unwrap_or_else(PoisonError::into_inner);
                send_brush(
                    t.sender(),
                    &stroke.brush,
                    Some(stroke.last),
                    s,
                    scale,
                    stroke.value,
                );
            }
        }
    }
    if let Some(active) = tool.stroke.as_mut() {
        active.last = s;
    }
}

/// Ends the active stroke (its undo entry arrives with a later [`pump`]; a Ramp or a Hole is
/// applied here, as one undo entry).
pub fn end_stroke(world: &mut World) {
    let density = world
        .get_resource_mut::<TerrainTool>()
        .and_then(|mut t| t.density_drag.take());
    if let Some(drag) = density {
        if !drag.pieces.is_empty() {
            world
                .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                .push(Box::new(crate::terrain_panel::GrassDensityCommand::new(
                    drag.entity,
                    drag.pieces,
                )));
        }
        return;
    }
    let scatter = world
        .get_resource_mut::<TerrainTool>()
        .and_then(|mut t| t.scatter_drag.take());
    if let Some(drag) = scatter {
        if let Some(record) = drag.record.filter(|r| !r.is_empty()) {
            world
                .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                .push(Box::new(TerrainStrokeCommand::new(
                    drag.entity,
                    StrokeUndo::Scatter(record),
                )));
        }
        return;
    }
    let drag = world.get_resource_mut::<TerrainTool>().and_then(|mut t| {
        let brush = t.brush;
        t.drag.take().map(|d| (d, brush))
    });
    if let Some((drag, brush)) = drag {
        match drag.kind {
            DragKind::Ramp => apply_ramp(world, drag, brush),
            DragKind::Hole => apply_hole(world, drag, brush),
        }
        return;
    }
    let Some(mut tool) = world.get_resource_mut::<TerrainTool>() else {
        return;
    };
    let Some(stroke) = tool.stroke.filter(|s| !s.ended) else {
        return;
    };
    let sent = match stroke.brush.target {
        BrushTarget::Height => tool.heights.as_ref().is_some_and(|t| {
            t.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .sender()
                .end_stroke()
        }),
        BrushTarget::LayerWeight { .. } => tool.weights.as_ref().is_some_and(|t| {
            t.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .sender()
                .end_stroke()
        }),
    };
    if let Some(active) = tool.stroke.as_mut() {
        active.ended = true;
        // The next pump keeps what landed as an undo entry.
        active.orphaned = !sent;
    }
}

/// Runs a released Ramp drag synchronously and pushes its undo entry.
fn apply_ramp(world: &mut World, ramp: Drag, mut brush: Brush) {
    if ramp.from.distance_squared(ramp.to) < 1e-6 {
        return;
    }
    if let Ok(mut e) = world.get_entity_mut(ramp.entity)
        && e.contains::<Terrain>()
        && !e.contains::<TerrainEdits>()
    {
        e.insert(TerrainEdits::default());
    }
    let mut query = world.query::<(&mut Terrain, Option<&mut TerrainPaint>, &mut TerrainEdits)>();
    let Ok((mut terrain, paint, mut edits)) = query.get_mut(world, ramp.entity) else {
        return;
    };
    brush.mode = regia_terrain::ramp_mode(&terrain.map, ramp.from, ramp.to, None, None);
    if ramp.invert {
        brush.mode = brush.mode.inverted();
    }
    let undo = match regia_terrain::apply_stroke(
        &mut terrain,
        paint.map(Mut::into_inner),
        &mut edits,
        &brush,
        &[ramp.from, ramp.to],
    ) {
        Ok(undo) => undo,
        Err(err) => {
            tracing::warn!(target: "regia_editor", "terrain ramp refused: {err}");
            return;
        }
    };
    if let Some(command) = crate::terrain_layers::stroke_command(world, ramp.entity, undo) {
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .push(command);
    }
}

/// One named hole added or removed in the editor (`8`), as an undo entry.
#[derive(Debug, Clone)]
pub struct TerrainHoleCommand {
    entity: Entity,
    id: String,
    before: Option<NamedHole>,
    after: Option<NamedHole>,
}

impl TerrainHoleCommand {
    /// The hole's id.
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl EditorCommand for TerrainHoleCommand {
    fn apply(&mut self, world: &mut World) {
        write_hole(world, self.entity, &self.id, self.after.as_ref());
    }

    fn revert(&mut self, world: &mut World) {
        write_hole(world, self.entity, &self.id, self.before.as_ref());
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        match self.after {
            Some(_) => format!("Terrain hole {}", self.id),
            None => format!("Remove terrain hole {}", self.id),
        }
    }
}

/// The draw's holes handle, if a draw is linked (its mask is then the truth).
fn linked_holes(world: &World) -> Option<TerrainHoles> {
    world
        .get_resource::<TerrainHoles>()
        .filter(|h| h.state().linked)
        .cloned()
}

/// The mask `entity` cuts now: the linked draw's next one (pending edits included), else the
/// terrain's own.
fn current_holes(world: &World, entity: Entity) -> HolesMask {
    if let Some(handle) = linked_holes(world) {
        let state = handle.state();
        return state.requested.unwrap_or(state.applied);
    }
    world
        .get::<Terrain>(entity)
        .and_then(|t| t.holes.clone())
        .unwrap_or_default()
}

/// Replaces the mask `entity` cuts: through the linked draw, else on the terrain itself.
fn edit_holes(
    world: &mut World,
    entity: Entity,
    edit: impl FnOnce(&mut HolesMask) -> Result<(), HolesError>,
) -> Result<(), HolesError> {
    if let Some(handle) = linked_holes(world) {
        return handle.edit(edit);
    }
    let Some(mut terrain) = world.get_mut::<Terrain>(entity) else {
        return Err(HolesError::NoTerrain);
    };
    let mut mask = terrain.holes.clone().unwrap_or_default();
    edit(&mut mask)?;
    mask.validate()?;
    terrain.holes = Some(mask);
    Ok(())
}

/// Sets (`Some`) or removes (`None`) the named hole `id` and marks the holes unsaved. `true` when
/// it landed.
fn write_hole(world: &mut World, entity: Entity, id: &str, hole: Option<&NamedHole>) -> bool {
    // A terrain deleted and restored by undo has a new id; follow it.
    let Some(target) = crate::command::resolve_entity(world, entity.to_bits()) else {
        tracing::warn!(target: "regia_editor", "terrain hole edit: entity {} is gone", entity.to_bits());
        return false;
    };
    let result = edit_holes(world, target, |mask| match hole {
        Some(hole) => mask.set_named(hole.clone()).map(|_| ()),
        None => {
            mask.remove_named(id);
            Ok(())
        }
    });
    if let Err(err) = result {
        tracing::warn!(target: "regia_editor", "terrain hole {id} refused: {err}");
        return false;
    }
    if let Some(mut tool) = world.get_resource_mut::<TerrainTool>() {
        tool.holes_dirty.insert(target);
    }
    true
}

/// Adds what `saved` holds to `mask`: named holes it does not have, and the unnamed footprints
/// when it has none of its own.
fn merge_saved_holes(mask: &mut HolesMask, saved: HolesMask) -> Result<(), HolesError> {
    if mask.holes.is_empty() && mask.shapes.is_empty() {
        mask.holes = saved.holes;
        mask.shapes = saved.shapes;
    }
    for hole in saved.named {
        if mask.named_hole(&hole.id).is_none() {
            mask.set_named(hole)?;
        }
    }
    Ok(())
}

/// Brings the level's saved holes in, once a session, before the first hole edit: the editor
/// does not load `<map>.holes.ron` with the level, and a save must not drop what the file holds.
/// `false` when the file exists and cannot be used (the edit is refused, so the file is never
/// replaced).
fn load_saved_holes(world: &mut World, entity: Entity) -> bool {
    if world
        .get_resource::<TerrainTool>()
        .is_some_and(|t| t.holes_loaded.contains(&entity))
    {
        return true;
    }
    let level = world
        .get_resource::<crate::level_io::OpenLevel>()
        .and_then(|open| open.path.clone());
    if let Some(file) =
        level.map(|level| holes_ron_path(&crate::terrain_panel::side_file_base(world, &level)))
    {
        let loaded = load_holes_ron(&file)
            .map_err(|err| err.to_string())
            .and_then(|saved| match saved {
                Some(saved) => edit_holes(world, entity, |mask| merge_saved_holes(mask, saved))
                    .map_err(|err| err.to_string()),
                None => Ok(()),
            });
        if let Err(err) = loaded {
            tracing::warn!(target: "regia_editor", "{}: {err}; hole edits are off", file.display());
            return false;
        }
    }
    if let Some(mut tool) = world.get_resource_mut::<TerrainTool>() {
        tool.holes_loaded.insert(entity);
    }
    true
}

/// The lowest free `editor_hole_<n>` id.
fn next_hole_id(mask: &HolesMask) -> String {
    let mut n = 1u32;
    loop {
        let id = format!("editor_hole_{n}");
        if mask.named_hole(&id).is_none() {
            return id;
        }
        n += 1;
    }
}

/// A released Hole drag: a slot along the drag as wide as the brush, a round hole of the brush's
/// size for a click, or (Shift) the removal of the named hole under the release point.
fn apply_hole(world: &mut World, drag: Drag, brush: Brush) {
    if !load_saved_holes(world, drag.entity) {
        return;
    }
    let mask = current_holes(world, drag.entity);
    let command = if drag.invert {
        let Some(hole) = mask
            .named
            .iter()
            .find(|h| h.open && h.shape.contains(drag.to.x, drag.to.y))
        else {
            return;
        };
        TerrainHoleCommand {
            entity: drag.entity,
            id: hole.id.clone(),
            before: Some(hole.clone()),
            after: None,
        }
    } else {
        let radius = match brush.shape {
            BrushShape::Circle { radius } => radius,
            BrushShape::Rectangle { width, length } => width.max(length) * 0.5,
        };
        let shape = if drag.from.distance(drag.to) < radius * 0.25 {
            HoleShape::Ellipse(HoleEllipse {
                center: drag.from.to_array(),
                radius: [radius, radius],
                rotation_radians: 0.0,
            })
        } else {
            HoleShape::segment(drag.from.to_array(), drag.to.to_array(), radius * 2.0)
        };
        let id = next_hole_id(&mask);
        TerrainHoleCommand {
            entity: drag.entity,
            id: id.clone(),
            before: None,
            after: Some(NamedHole {
                id,
                open: true,
                shape,
            }),
        }
    };
    if write_hole(world, drag.entity, &command.id, command.after.as_ref()) {
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .push(Box::new(command));
    }
}

/// Writes the holes edited since the last save to `<map>.holes.ron` next to `level`, atomically.
/// Without a hole edit nothing is touched, so a file the session never loaded stays as it is.
/// Returns the file written.
///
/// # Errors
///
/// The mask cannot be serialized or the file cannot be written, or holes of two terrains are
/// unsaved: a level has one holes file, and the second write would silently lose the first's.
pub fn save_terrain_holes(world: &mut World, level: &Path) -> Result<Option<PathBuf>, String> {
    save_terrain_holes_inner(world, level, true)
}

/// Writes the holes file. `commit` clears the holes-dirty set when the write lands.
/// The orchestrated save passes `false` and clears only from [`commit_terrain_save`], after the
/// editor has stored the path the user asked for.
fn save_terrain_holes_inner(
    world: &mut World,
    level: &Path,
    commit: bool,
) -> Result<Option<PathBuf>, String> {
    let Some(dirty) = world.get_resource::<TerrainTool>().map(|t| t.holes_dirty.clone()) else {
        return Ok(None);
    };
    // A terrain that is gone took its edits with it (the level was replaced).
    let dirty: BTreeSet<Entity> = dirty
        .into_iter()
        .filter(|e| world.get::<Terrain>(*e).is_some())
        .collect();
    if dirty.len() > 1 {
        return Err(format!(
            "{} terrains have unsaved holes and a level holds one holes file; undo the holes of all but one",
            dirty.len()
        ));
    }
    world.resource_mut::<TerrainTool>().holes_dirty.clone_from(&dirty);
    let Some(&edited) = dirty.first() else {
        return Ok(None);
    };
    let text = current_holes(world, edited)
        .to_holes_ron()
        .map_err(|e| e.to_string())?;
    let file = holes_ron_path(&crate::terrain_panel::side_file_base(world, level));
    crate::level_io::write_atomic(&file, text.as_bytes())?;
    if commit {
        world.resource_mut::<TerrainTool>().holes_dirty.clear();
    }
    Ok(Some(file))
}

/// Writes the painted weights changed since the last save to `<map>.weights.bin` next to `level`,
/// atomically. Without a paint edit nothing is touched. Returns the file written.
///
/// # Errors
///
/// The weights cannot be serialized or the file cannot be written, or two terrains have unsaved
/// paint: a level has one weights file, and the second write would silently lose the first's.
pub fn save_terrain_paint(world: &mut World, level: &Path) -> Result<Option<PathBuf>, String> {
    save_terrain_paint_inner(world, level, true).map(|written| written.map(|(file, _)| file))
}

/// Writes the weights file. `commit` clears the paint dirty flag of the terrain that was written.
fn save_terrain_paint_inner(
    world: &mut World,
    level: &Path,
    commit: bool,
) -> Result<Option<(PathBuf, Entity)>, String> {
    let mut query = world.query::<(Entity, &TerrainPaint, &TerrainEdits)>();
    let unsaved: Vec<_> = query
        .iter(world)
        .filter(|(_, _, edits)| edits.unsaved_paint())
        .map(|(entity, paint, _)| (entity, paint))
        .collect();
    if unsaved.len() > 1 {
        return Err(format!(
            "{} terrains have unsaved paint and a level holds one weights file; undo the paint of all but one",
            unsaved.len()
        ));
    }
    let Some(&(entity, paint)) = unsaved.first() else {
        return Ok(None);
    };
    let bytes = paint.0.to_bytes().map_err(|e| e.to_string())?;
    let file = layer_weights_path(&crate::terrain_panel::side_file_base(world, level));
    crate::level_io::write_atomic(&file, &bytes)?;
    if commit && let Some(mut edits) = world.get_mut::<TerrainEdits>(entity) {
        edits.take_unsaved_paint();
    }
    Ok(Some((file, entity)))
}

/// Brings the open level's `<map>.scatterpaint.bin` in, once per level and terrain: the instances
/// should draw with the level, and a save must not drop what the file holds. A level without a
/// file starts empty, so a terrain the host keeps across levels does not carry the last level's
/// instances over. A file that cannot be read refuses scatter edits, so it is never replaced.
fn load_level_scatter(world: &mut World) {
    let Some(level) = world
        .get_resource::<crate::level_io::OpenLevel>()
        .and_then(|open| open.path.clone())
    else {
        return;
    };
    if let Some((path, entity)) = world
        .get_resource::<TerrainTool>()
        .and_then(|t| t.scatter_loaded.as_ref())
        && *path == level
        && world.get::<Terrain>(*entity).is_some()
    {
        return;
    }
    let mut query = world.query_filtered::<Entity, With<Terrain>>();
    let Some(entity) = query.iter(world).min_by_key(|e| e.to_bits()) else {
        return;
    };
    let switched = world
        .get_resource::<TerrainTool>()
        .is_some_and(|t| t.scatter_loaded.is_some());
    let file = scatter_paint_path(&crate::terrain_panel::side_file_base(world, &level));
    let refused = match load_scatter_paint(&file) {
        Ok(Some(mut store)) => {
            let moved = world
                .get::<Terrain>(entity)
                .map_or(0, |terrain| store.reseat(&terrain.map, None));
            let mut target = world.entity_mut(entity);
            if !target.contains::<TerrainEdits>() {
                target.insert(TerrainEdits::default());
            }
            target.insert(TerrainScatterPaint(store));
            if moved > 0
                && let Some(mut edits) = target.get_mut::<TerrainEdits>()
            {
                edits.mark_scatter_unsaved();
            }
            false
        }
        Ok(None) => {
            let unsaved = world
                .get::<TerrainEdits>(entity)
                .is_some_and(TerrainEdits::unsaved_scatter);
            if switched
                && !unsaved
                && let Some(mut paint) = world.get_mut::<TerrainScatterPaint>(entity)
            {
                paint.0 = PaintedScatter::new();
            }
            false
        }
        Err(err) => {
            tracing::warn!(target: "regia_editor", "{}: {err}; scatter edits are off", file.display());
            true
        }
    };
    let mut tool = world.get_resource_or_insert_with::<TerrainTool>(TerrainTool::default);
    tool.scatter_loaded = Some((level, entity));
    tool.scatter_refused = refused;
}

/// Writes the painted instances to `<map>.scatterpaint.bin` next to `level`, atomically, when they
/// changed since the last save or the level has no such file yet (a Save As). Returns the file
/// written. One level has one file.
///
/// # Errors
///
/// The instances cannot be serialized or the file cannot be written, or two terrains have
/// instances to write: the second write would silently lose the first's.
pub fn save_terrain_scatter(world: &mut World, level: &Path) -> Result<Option<PathBuf>, String> {
    save_terrain_scatter_inner(world, level, true).map(|written| written.map(|(file, _)| file))
}

/// Writes the scatter file. `commit` clears the scatter dirty flag and records which level the
/// file belongs to. A refusal (`Ok(None)`) writes nothing and clears nothing.
fn save_terrain_scatter_inner(
    world: &mut World,
    level: &Path,
    commit: bool,
) -> Result<Option<(PathBuf, Entity)>, String> {
    let file = scatter_paint_path(&crate::terrain_panel::side_file_base(world, level));
    if world
        .get_resource::<TerrainTool>()
        .is_some_and(|t| t.scatter_refused)
    {
        tracing::warn!(target: "regia_editor", "{} cannot be read; it is left as it is", file.display());
        return Ok(None);
    }
    let missing = !file.exists();
    let mut query = world.query::<(Entity, &TerrainScatterPaint, Option<&TerrainEdits>)>();
    let candidates: Vec<(Entity, &TerrainScatterPaint)> = query
        .iter(world)
        .filter(|(_, paint, edits)| {
            edits.is_some_and(TerrainEdits::unsaved_scatter) || (missing && !paint.0.is_empty())
        })
        .map(|(entity, paint, _)| (entity, paint))
        .collect();
    // One level holds one file: a second terrain's write would replace the first's instances and
    // mark both saved (audit wave 5, terrain-g2b M), so refuse as paint and holes do.
    if candidates.len() > 1 {
        return Err(format!(
            "{} terrains have unsaved scatter and a level holds one scatter file; undo the scatter of all but one",
            candidates.len()
        ));
    }
    let Some(&(entity, paint)) = candidates.first() else {
        return Ok(None);
    };
    let bytes = paint.0.to_bytes().map_err(|e| e.to_string())?;
    crate::level_io::write_atomic(&file, &bytes)?;
    if commit {
        if let Some(mut edits) = world.get_mut::<TerrainEdits>(entity) {
            edits.take_unsaved_scatter();
        }
        if let Some(mut tool) = world.get_resource_mut::<TerrainTool>() {
            tool.scatter_loaded = Some((level.to_path_buf(), entity));
        }
    }
    Ok(Some((file, entity)))
}

/// What [`save_terrain_files`] wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerrainFilesSaved {
    /// Cook tiles re-cooked across every sculpted terrain.
    pub tiles_recooked: usize,
    /// The `<map>.holes.ron` written, when holes changed.
    pub holes_file: Option<PathBuf>,
    /// The `<map>.weights.bin` written, when painted weights changed.
    pub paint_file: Option<PathBuf>,
    /// The `<map>.scatterpaint.bin` written, when painted instances changed.
    pub scatter_file: Option<PathBuf>,
    /// The `<map>.grassdensity.bin` written, when the painted grass density changed (ADR-2411).
    pub grass_density_file: Option<PathBuf>,
}

/// Dirty flags a successful [`save_terrain_files`] wrote but has not cleared yet.
///
/// The clear waits until the caller has stored the path the user asked for
/// ([`commit_terrain_save`]). A later step that fails drops this record, so the flags stay
/// set and a retry rewrites the side files next to the path that is still open.
#[derive(Resource)]
struct TerrainSavePending {
    tiles: Vec<Entity>,
    holes: Vec<Entity>,
    paint: Option<Entity>,
    scatter: Option<Entity>,
    scatter_level: PathBuf,
    grass: Option<Entity>,
    /// The edit-layers sidecar was written (ADR-7825).
    layers: bool,
}

/// Saves what the terrain tool edited for the level at `level`: the re-cooked tiles
/// ([`save_terrain_edits`]), the holes ([`save_terrain_holes`]), the painted weights
/// ([`save_terrain_paint`]) and the painted instances ([`save_terrain_scatter`]).
///
/// The files are written here. Dirty flags stay set until [`commit_terrain_save`], which the
/// editor calls only after it has stored `level` as the open path. A direct
/// [`save_terrain_holes`] (and the other single-file savers) still clears its own flag.
///
/// # Errors
///
/// The first file that cannot be written. Nothing this call wrote is marked saved, and any
/// pending clear from an earlier attempt is dropped.
pub fn save_terrain_files(world: &mut World, level: &Path) -> Result<TerrainFilesSaved, String> {
    world.remove_resource::<TerrainSavePending>();
    let saved = write_terrain_files(world, level)?;
    Ok(saved)
}

fn write_terrain_files(world: &mut World, level: &Path) -> Result<TerrainFilesSaved, String> {
    let saves =
        save_terrain_edits_inner(world, false).map_err(|err| format!("terrain re-cook failed: {err}"))?;
    let holes_file = save_terrain_holes_inner(world, level, false)
        .map_err(|err| format!("terrain holes save failed: {err}"))?;
    let paint = save_terrain_paint_inner(world, level, false)
        .map_err(|err| format!("terrain paint save failed: {err}"))?;
    let scatter = save_terrain_scatter_inner(world, level, false)
        .map_err(|err| format!("terrain scatter save failed: {err}"))?;
    let grass = crate::terrain_panel::save_grass_density_writing(world, level, false)
        .map_err(|err| format!("terrain grass density save failed: {err}"))?;
    let layers = crate::terrain_layers::save_edit_layers(world, level, false)
        .map_err(|err| format!("terrain edit layers save failed: {err}"))?;
    let holes = if holes_file.is_some() {
        world
            .get_resource::<TerrainTool>()
            .map(|tool| tool.holes_dirty.iter().copied().collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    world.insert_resource(TerrainSavePending {
        tiles: saves.iter().map(|save| save.entity).collect(),
        holes,
        paint: paint.as_ref().map(|(_, entity)| *entity),
        scatter: scatter.as_ref().map(|(_, entity)| *entity),
        scatter_level: level.to_path_buf(),
        grass: grass.as_ref().map(|(_, entity)| *entity),
        layers: layers.is_some(),
    });
    Ok(TerrainFilesSaved {
        tiles_recooked: saves.iter().map(|s| s.tiles_recooked).sum(),
        holes_file,
        paint_file: paint.map(|(file, _)| file),
        scatter_file: scatter.map(|(file, _)| file),
        grass_density_file: grass.map(|(file, _)| file),
    })
}

/// Clears the terrain dirty flags recorded by the last successful [`save_terrain_files`].
///
/// Call this only after the open level's path is the path that save wrote. A failed
/// orchestrated save leaves no record, so the flags stay dirty. Forgetting the call is safe:
/// the next save rewrites the same files.
pub fn commit_terrain_save(world: &mut World) {
    let Some(pending) = world.remove_resource::<TerrainSavePending>() else {
        return;
    };
    if let Some(mut tool) = world.get_resource_mut::<TerrainTool>() {
        for entity in &pending.holes {
            tool.holes_dirty.remove(entity);
        }
        if let Some(entity) = pending.scatter {
            tool.scatter_loaded = Some((pending.scatter_level, entity));
        }
    }
    if let Some(entity) = pending.paint
        && let Some(mut edits) = world.get_mut::<TerrainEdits>(entity)
    {
        edits.take_unsaved_paint();
    }
    if let Some(entity) = pending.scatter
        && let Some(mut edits) = world.get_mut::<TerrainEdits>(entity)
    {
        edits.take_unsaved_scatter();
    }
    for entity in pending.tiles {
        if let Some(mut edits) = world.get_mut::<TerrainEdits>(entity) {
            edits.take_unsaved_tiles();
        }
    }
    if let Some(entity) = pending.grass
        && let Some(mut paint) = world.get_mut::<regia_terrain::GrassDensityPaint>(entity)
    {
        paint.mark_saved();
    }
    if pending.layers {
        crate::terrain_layers::commit_edit_layers_save(world);
    }
}

/// Writes the brush thread's batches into the terrain and turns finished strokes into undo
/// entries. With `block`, waits until the active stroke's record has arrived (tests, save).
pub fn pump(world: &mut World, block: bool) {
    let _span = tracing::info_span!(target: "regia_editor", "terrain_tool.pump").entered();
    if world.get_resource::<TerrainTool>().is_none() {
        return;
    }
    world.resource_scope::<TerrainTool, ()>(|world, mut tool| {
        let tool = &mut *tool;
        loop {
            let Some(stroke) = tool.stroke else { return };
            let mut finished = None;
            // The worker is gone and nothing is left in its channel: no record will come.
            let gone = match stroke.brush.target {
                BrushTarget::Height => match &tool.heights {
                    None => true,
                    Some(t) => {
                        let t = t.lock().unwrap_or_else(PoisonError::into_inner);
                        let (events, dead) = next_events(&t, &stroke, block);
                        for event in events {
                            match event {
                                BrushThreadEvent::Batch(batch) => {
                                    let batch =
                                        rebase_height_batch(world, stroke.entity, tool.partial.as_mut(), batch);
                                    apply_height_batch(world, stroke.entity, &batch);
                                    if let Some(partial) = &mut tool.partial {
                                        partial.note(&batch.tiles);
                                    }
                                }
                                BrushThreadEvent::Finished(record) => {
                                    let record = match &tool.partial {
                                        Some(partial) => partial.heights.finish(record),
                                        None => record,
                                    };
                                    finished = Some(StrokeUndo::Height(record));
                                }
                            }
                        }
                        dead
                    }
                },
                BrushTarget::LayerWeight { layer } => match &tool.weights {
                    None => true,
                    Some(t) => {
                        let t = t.lock().unwrap_or_else(PoisonError::into_inner);
                        let (events, dead) = next_events(&t, &stroke, block);
                        for event in events {
                            match event {
                                BrushThreadEvent::Batch(batch) => {
                                    let batch = rebase_weight_batch(
                                        world,
                                        stroke.entity,
                                        layer,
                                        tool.partial.as_mut(),
                                        batch,
                                    );
                                    apply_weight_batch(world, stroke.entity, layer, &batch);
                                    if let Some(partial) = &mut tool.partial {
                                        partial.note(&batch.tiles);
                                    }
                                }
                                BrushThreadEvent::Finished(record) => {
                                    let record = match &tool.partial {
                                        Some(partial) => partial.weights.finish(record),
                                        None => record,
                                    };
                                    finished = Some(StrokeUndo::LayerWeight { layer, record });
                                }
                            }
                        }
                        dead
                    }
                },
            };
            if let Some(undo) = finished {
                tool.stroke = None;
                tool.partial = None;
                if let Some(command) = crate::terrain_layers::stroke_command(world, stroke.entity, undo) {
                    world
                        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                        .push(command);
                }
                return;
            }
            if gone {
                tracing::warn!(target: "regia_editor", "terrain brush thread died; keeping the part of the stroke that landed as an undo step");
                tool.stroke = None;
                // The next stroke spawns a new worker.
                match stroke.brush.target {
                    BrushTarget::Height => tool.heights = None,
                    BrushTarget::LayerWeight { .. } => tool.weights = None,
                }
                if let Some(undo) = tool.partial.take().and_then(|p| p.undo(&stroke))
                    && let Some(command) = crate::terrain_layers::stroke_command(world, stroke.entity, undo)
                {
                    world
                        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                        .push(command);
                }
                return;
            }
            if !(block && stroke.ended) {
                return;
            }
        }
    });
}

/// What the worker has for `stroke` now, and whether it is gone with nothing left to read. With
/// `block`, an ended stroke waits for the next event. An unended stroke stays until its release.
fn next_events<V: BrushValue>(
    thread: &BrushThread<V>,
    stroke: &ActiveStroke,
    block: bool,
) -> (Vec<BrushThreadEvent<V>>, bool) {
    let (mut events, closed) = thread.poll_closed();
    if !events.is_empty() || !stroke.ended {
        return (events, false);
    }
    if closed || stroke.orphaned {
        return (events, true);
    }
    if block {
        match thread.wait() {
            Some(event) => events.push(event),
            None => return (events, true),
        }
    }
    (events, false)
}

fn apply_height_batch(world: &mut World, entity: Entity, batch: &regia_terrain::brush::StrokeBatch<f32>) {
    let mut query = world.query::<(&mut Terrain, &mut TerrainEdits)>();
    if let Ok((mut terrain, mut edits)) = query.get_mut(world, entity) {
        let terrain = &mut *terrain;
        let flush = batch.apply(&mut terrain.map);
        edits.note(&terrain.map, &mut terrain.minmax, flush.dirty, &flush.tiles);
    }
}

fn apply_weight_batch(
    world: &mut World,
    entity: Entity,
    layer: usize,
    batch: &regia_terrain::brush::StrokeBatch<u8>,
) {
    let mut query = world.query::<(&mut TerrainPaint, &mut TerrainEdits)>();
    if let Ok((mut paint, mut edits)) = query.get_mut(world, entity)
        && let Some(mut target) = paint.0.layer_mut(layer)
    {
        let flush = batch.apply(&mut target);
        edits.note_paint(flush.dirty);
    }
}

/// Moves agent strokes ([`TerrainStrokeLog`]) onto the undo stack.
pub fn drain_stroke_log(world: &mut World) {
    let Some(mut log) = world.get_resource_mut::<TerrainStrokeLog>() else {
        return;
    };
    log.drained = true;
    if log.strokes.is_empty() {
        return;
    }
    let strokes = std::mem::take(&mut log.strokes);
    for (entity, undo) in strokes {
        // A height edit of a terrain with edit layers lands in its active layer (ADR-7825).
        let command: Box<dyn EditorCommand> = if undo.is_empty() {
            Box::new(TerrainStrokeCommand::new(entity, undo))
        } else {
            match crate::terrain_layers::stroke_command(world, entity, undo) {
                Some(command) => command,
                None => continue,
            }
        };
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .push(command);
    }
}

/// Re-seats painted instances after height edits. `TerrainPlugin` does it in `Update`, but the editor
/// does not add that plugin itself; where a host did, the second pass finds nothing to do.
fn reseat_scatter(world: &mut World) {
    let mut query = world.query::<(&Terrain, &mut TerrainScatterPaint, &mut TerrainEdits)>();
    for (terrain, mut paint, mut edits) in query.iter_mut(world) {
        if edits.scatter_reseat().is_some() {
            reseat_terrain_scatter(terrain, &mut paint, &mut edits);
        }
    }
}

/// Per-frame bookkeeping: the level's painted instances, stroke batches, agent strokes, re-seating,
/// the gizmo brush preview.
pub fn frame_effects(world: &mut World) {
    crate::terrain_panel::load_open_level_terrain(world);
    crate::terrain_panel::load_level_grass_density(world);
    load_level_scatter(world);
    crate::terrain_layers::load_level_edit_layers(world);
    // Agent strokes were written during the frame: they go on the stack before a threaded stroke
    // that finishes now, so undo walks the edits in the order they landed.
    drain_stroke_log(world);
    crate::terrain_layers::drain_layer_log(world);
    pump(world, false);
    reseat_scatter(world);
    let preview = world
        .get_resource::<TerrainTool>()
        .map(|tool| TerrainBrushPreview {
            center: tool.active.then_some(tool.hover).flatten(),
            shape: tool.brush.shape,
            color: tool_color(tool),
        });
    if let Some(preview) = preview {
        world.insert_resource(preview);
    }
}

/// Outline colour by mode (linear).
/// The outline color of the tool's current mode.
fn tool_color(tool: &TerrainTool) -> regia_core::math::Color {
    if tool.cut_holes {
        regia_core::math::Color::rgb(1.0, 0.25, 0.1)
    } else if tool.paint_density {
        regia_core::math::Color::rgb(0.35, 0.8, 0.95)
    } else if tool.paint_scatter {
        regia_core::math::Color::rgb(0.55, 0.9, 0.25)
    } else {
        outline_color(&tool.brush)
    }
}

fn outline_color(brush: &Brush) -> regia_core::math::Color {
    use regia_core::math::Color;
    match (brush.target, brush.mode) {
        (BrushTarget::LayerWeight { .. }, _) => Color::rgb(0.3, 1.0, 0.4),
        (_, BrushMode::Raise { .. }) => Color::rgb(1.0, 0.75, 0.2),
        (_, BrushMode::Lower { .. }) => Color::rgb(0.3, 0.6, 1.0),
        (_, BrushMode::Flatten) => Color::rgb(0.9, 0.9, 0.9),
        (_, BrushMode::Smooth { .. }) => Color::rgb(0.8, 0.5, 1.0),
        (_, BrushMode::Assign { .. }) => Color::rgb(1.0, 0.4, 0.4),
        (_, BrushMode::Noise { .. }) => Color::rgb(0.95, 0.55, 0.85),
        (_, BrushMode::Ramp { .. }) => Color::rgb(0.4, 0.95, 0.95),
    }
}

/// Viewport input for the tool: hotkeys, hover, stroke start/drag/end. Returns `true` when the
/// tool owns the primary button this frame (the viewport then skips panning, picking and the
/// transform gizmo).
pub fn viewport_input(
    ui: &egui::Ui,
    world: &mut World,
    response: &egui::Response,
    camera: &crate::viewport::EditorCamera,
    size: Vec2,
) -> bool {
    if world.get_resource::<TerrainTool>().is_none() {
        world.init_resource::<TerrainTool>();
    }
    let frame = ui.ctx().cumulative_frame_nr();
    let region_dragging = crate::terrain_region::dragging(world, response.id);
    // Whether this pane handles the pointer this frame: hovered, focused or mid-stroke, once per
    // pane even when egui repeats a layout pass.
    let here = {
        let mut tool = world.resource_mut::<TerrainTool>();
        if tool.input_frame != Some(frame) {
            tool.input_panes.clear();
            tool.input_frame = Some(frame);
            tool.hover = None;
            tool.keys_done = false;
        }
        (response.hovered() || response.has_focus() || tool.stroking() || region_dragging)
            && tool.input_panes.insert(response.id)
    };
    // The pane's own keyboard focus (Tab into the viewport, FU-9) does not count as egui wanting
    // the keyboard: egui reports that whenever any widget has focus, which left a keyboard user's
    // brush keys dead; a focused text field still keeps them (ADR-8870). `T` toggles the tool
    // over the hovered pane or the focused one.
    let keyboard_free = !ui.ctx().egui_wants_keyboard_input() || response.has_focus();
    // The brush keys run whenever the keyboard is free, wherever the pointer is (ADR-8870); `T`
    // needs the pane hovered or focused.
    let pressed = |k: egui::Key| keyboard_free && ui.input(|i| i.key_pressed(k));
    if here && pressed(egui::Key::T) && (response.hovered() || response.has_focus()) {
        let active = {
            let mut tool = world.resource_mut::<TerrainTool>();
            tool.active = !tool.active;
            tool.active
        };
        if !active && let Some(mut region) = world.get_resource_mut::<crate::terrain_region::TerrainRegion>()
        {
            region.cancel_drag();
        }
    }
    if !world.resource::<TerrainTool>().active {
        return false;
    }
    if world
        .get_resource::<crate::terrain_region::TerrainRegion>()
        .is_some_and(|s| s.active)
    {
        if here {
            crate::terrain_region::viewport_input(ui, world, response, camera, size);
        }
        return true;
    }
    // Once per egui frame, from the first pane drawn (review sprint-w12-rev-g8 #2: the pointer may be
    // over another panel).
    let keys_now = {
        let mut tool = world.resource_mut::<TerrainTool>();
        !std::mem::replace(&mut tool.keys_done, true)
    };
    if keys_now {
        let mut tool = world.resource_mut::<TerrainTool>();
        if pressed(egui::Key::OpenBracket) {
            tool.scale_size(1.0 / 1.15);
        }
        if pressed(egui::Key::CloseBracket) {
            tool.scale_size(1.15);
        }
        if pressed(egui::Key::Minus) {
            tool.scale_strength(1.0 / 1.25);
        }
        if pressed(egui::Key::Equals) || pressed(egui::Key::Plus) {
            tool.scale_strength(1.25);
        }
        for (key, n) in [
            (egui::Key::Num0, 0),
            (egui::Key::Num1, 1),
            (egui::Key::Num2, 2),
            (egui::Key::Num3, 3),
            (egui::Key::Num4, 4),
            (egui::Key::Num5, 5),
            (egui::Key::Num6, 6),
            (egui::Key::Num7, 7),
            (egui::Key::Num8, 8),
            (egui::Key::Num9, 9),
        ] {
            if pressed(key) {
                tool.select_mode(n);
            }
        }
        if pressed(egui::Key::Comma) {
            tool.step_layer(-1);
        }
        if pressed(egui::Key::Period) {
            tool.step_layer(1);
        }
    }
    if !here {
        return true;
    }

    let (alt, shift, primary_pressed, primary_down, primary_released) = ui.input(|i| {
        (
            i.modifiers.alt,
            i.modifiers.shift,
            i.pointer.primary_pressed(),
            i.pointer.primary_down(),
            i.pointer.primary_released(),
        )
    });
    let hit = response.hover_pos().and_then(|pointer| {
        let local = pointer - response.rect.left_top();
        let (origin, direction) = crate::viewport::pick_ray(camera, Vec2::new(local.x, local.y), size);
        pick_terrain(world, origin, direction)
    });
    world.resource_mut::<TerrainTool>().hover = hit.map(|(_, p)| Vec2::new(p.x, p.z));

    let stroking = world.resource::<TerrainTool>().stroking();
    if primary_pressed && !alt && response.hovered() {
        if let Some((entity, p)) = hit {
            begin_stroke(world, entity, Vec2::new(p.x, p.z), shift);
        }
    } else if primary_down
        && stroking
        && let Some((_, p)) = hit
    {
        continue_stroke(world, Vec2::new(p.x, p.z));
    }
    if primary_released && stroking {
        end_stroke(world);
    }
    !alt && (primary_down || primary_pressed || primary_released)
}

/// Paints the brush outline and the tool status over the viewport.
pub fn paint_overlay(ui: &egui::Ui, world: &World, camera: &crate::viewport::EditorCamera, rect: egui::Rect) {
    let Some(tool) = world.get_resource::<TerrainTool>().filter(|t| t.active) else {
        return;
    };
    if world
        .get_resource::<crate::terrain_region::TerrainRegion>()
        .is_some_and(|s| s.active)
    {
        crate::terrain_region::paint_overlay(ui, world, camera, rect);
        return;
    }
    let painter = ui.painter_at(rect);
    painter.text(
        rect.left_bottom() + egui::vec2(8.0, -8.0),
        egui::Align2::LEFT_BOTTOM,
        tool.status(),
        egui::FontId::proportional(12.0),
        egui::Color32::from_rgb(240, 220, 160),
    );
    let Some(center) = tool.hover else { return };
    let selected = world
        .get_resource::<crate::selection::Selection>()
        .and_then(crate::selection::Selection::primary);
    let terrain = selected
        .and_then(|e| world.get::<Terrain>(e))
        .or_else(|| world.iter_entities().find_map(|e| e.get::<Terrain>()));
    let Some(terrain) = terrain else { return };
    let ring = regia_terrain::brush_outline(&terrain.map, center, &tool.brush.shape, 64);
    let size = Vec2::new(rect.width(), rect.height());
    let c = tool_color(tool).to_srgb_u8();
    let colour = egui::Color32::from_rgb(c[0], c[1], c[2]);
    let to_screen = |p: Vec3| {
        crate::viewport::project_to_screen(camera, p, size).map(|s| rect.left_top() + egui::vec2(s.x, s.y))
    };
    for pair in ring.windows(2) {
        if let (Some(a), Some(b)) = (to_screen(pair[0]), to_screen(pair[1])) {
            painter.line_segment([a, b], egui::Stroke::new(2.0, colour));
        }
    }
    // The plateau ends at hardness * radius. Use the same surface sampler as the outer ring.
    let mut inner_shape = tool.brush.shape;
    let hardness = tool.brush.hardness.clamp(0.0, 1.0);
    match &mut inner_shape {
        BrushShape::Circle { radius } => *radius *= hardness,
        BrushShape::Rectangle { width, length } => {
            let shoulder = width.min(*length) * (1.0 - hardness);
            *width -= shoulder;
            *length -= shoulder;
        }
    }
    if hardness > 0.0 && hardness < 1.0 {
        let inner = regia_terrain::brush_outline(&terrain.map, center, &inner_shape, 64);
        for pair in inner.windows(2) {
            if let (Some(a), Some(b)) = (to_screen(pair[0]), to_screen(pair[1])) {
                painter.line_segment(
                    [a, b],
                    egui::Stroke::new(1.0, crate::theme::tokens(world).overlay.bounds),
                );
            }
        }
    }
    let h = terrain.map.sample_bilinear(center.x, center.y);
    if let Some(dot) = to_screen(Vec3::new(center.x, h, center.y)) {
        painter.circle_filled(dot, 3.0, colour);
    }
}

/// Where a terrain's cooked heightfield lives, so a save can re-cook it in place.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct TerrainCookTarget {
    /// The map's asset id (tile and heightfield ids derive from it).
    pub map_id: AssetId,
    /// The cooked cache directory the asset watcher observes.
    pub cache_dir: PathBuf,
}

/// What [`save_terrain_edits`] did for one terrain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerrainSave {
    /// The terrain entity.
    pub entity: Entity,
    /// Tiles re-cooked (only the touched ones).
    pub tiles_recooked: usize,
    /// Tiles in the cooked heightfield.
    pub tiles_total: usize,
    /// The `.fckd` written.
    pub path: PathBuf,
}

/// Re-cooks the touched tiles of every edited terrain with a [`TerrainCookTarget`] and writes the
/// cooked heightfield (the asset watcher reloads it). Terrains without unsaved tiles are skipped.
///
/// # Errors
///
/// Returns a message for the first terrain whose cooked file cannot be read, re-cooked or written
/// (its unsaved tiles stay recorded for the next save).
pub fn save_terrain_edits(world: &mut World) -> Result<Vec<TerrainSave>, String> {
    save_terrain_edits_inner(world, true)
}

fn save_terrain_edits_inner(world: &mut World, commit: bool) -> Result<Vec<TerrainSave>, String> {
    pump(world, true);
    crate::terrain_layers::reconcile_before_save(world)?;
    // Two terrains whose targets name one cooked file would each write it, the later replacing
    // the earlier's tiles, and both be marked saved (audit wave 5, terrain-g2b Low): refuse.
    let mut targets = world.query::<(&TerrainEdits, &TerrainCookTarget)>();
    let mut paths = std::collections::BTreeSet::new();
    for (edits, target) in targets.iter(world) {
        if edits.unsaved_tiles().is_empty() {
            continue;
        }
        let id = regia_asset_pipeline::terrain_data::heightfield_asset_id(target.map_id);
        let path = target.cache_dir.join(regia_asset::cooked_file_name(id));
        if !paths.insert(path.clone()) {
            return Err(format!(
                "two terrains with unsaved edits cook to {}; give each its own map id",
                path.display()
            ));
        }
    }
    let mut query = world.query::<(Entity, &Terrain, &mut TerrainEdits, &TerrainCookTarget)>();
    let mut saves = Vec::new();
    for (entity, terrain, mut edits, target) in query.iter_mut(world) {
        if edits.unsaved_tiles().is_empty() {
            continue;
        }
        let tiles = edits.unsaved_tiles().clone();
        let save = save_one(entity, &terrain.map, &tiles, target)?;
        if commit {
            edits.take_unsaved_tiles();
        }
        saves.push(save);
    }
    Ok(saves)
}

fn save_one(
    entity: Entity,
    map: &regia_terrain::TerrainMap,
    tiles: &std::collections::BTreeSet<regia_terrain::brush::TileCoord>,
    target: &TerrainCookTarget,
) -> Result<TerrainSave, String> {
    let id = regia_asset_pipeline::terrain_data::heightfield_asset_id(target.map_id);
    let path = target.cache_dir.join(regia_asset::cooked_file_name(id));
    let (cooked, recooked) = match std::fs::read(&path) {
        Ok(bytes) => {
            let mut cooked = regia_terrain::decode_cooked_tiled_heightfield(&bytes)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            let n = regia_terrain::recook_tiles(&mut cooked, map, tiles, EDIT_CELL_SIZE)
                .map_err(|e| e.to_string())?;
            (cooked, n)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let cooked = regia_terrain::cook_all_tiles(map, DEFAULT_TILE_SIZE).map_err(|e| e.to_string())?;
            let n = cooked.tiles.len();
            (cooked, n)
        }
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
    let total = cooked.tiles.len();
    write_cooked(target.map_id, cooked, &target.cache_dir)?;
    Ok(TerrainSave {
        entity,
        tiles_recooked: recooked,
        tiles_total: total,
        path,
    })
}

fn write_cooked(
    map_id: AssetId,
    cooked: regia_terrain::CookedTiledHeightfield,
    dir: &Path,
) -> Result<(), String> {
    use regia_asset_pipeline::terrain_data::{
        HeightfieldTile, TiledHeightfield, write_tiled_heightfield_cooked,
    };
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
    write_tiled_heightfield_cooked(map_id, &tiled, dir)
        .map(|_| ())
        .map_err(|e| format!("writing the cooked heightfield: {e}"))
}

/// A preview mesh of a terrain for the viewport: at most `max_side` vertices per side, world
/// space, vertex colours from the layer stack blended with the painted weights.
pub fn preview_mesh(
    terrain: &Terrain,
    paint: Option<&TerrainPaint>,
    max_side: u32,
) -> (Vec<LabVertex>, Vec<u32>) {
    let map = &terrain.map;
    let (w, h) = (map.width(), map.height());
    let step = w.max(h).div_ceil(max_side.max(2)).max(1);
    let cols: Vec<u32> = (0..w)
        .step_by(step as usize)
        .chain(((w - 1) % step != 0).then_some(w - 1))
        .collect();
    let rows: Vec<u32> = (0..h)
        .step_by(step as usize)
        .chain(((h - 1) % step != 0).then_some(h - 1))
        .collect();
    let mut vertices = Vec::with_capacity(cols.len() * rows.len());
    for &r in &rows {
        for &c in &cols {
            let [x, z] = map.sample_to_world(c, r);
            let y = map.height_at_sample(i64::from(c), i64::from(r));
            let n = map.normal_at(x, z);
            let slope = n[1].clamp(-1.0, 1.0).acos();
            let painted = paint.map(|p| p.0.sample(c, r)).unwrap_or_default();
            let weights = terrain.layers.blend_painted(slope, y, &painted);
            let mut albedo = [0.0f32; 3];
            for (layer, wgt) in terrain.layers.layers.iter().zip(&weights) {
                for (a, l) in albedo.iter_mut().zip(layer.albedo) {
                    *a += l * wgt;
                }
            }
            let mut v = LabVertex::from_pos_normal_uv(
                [x, y, z],
                n,
                [c as f32 / (w - 1) as f32, r as f32 / (h - 1) as f32],
            );
            v.color = [albedo[0], albedo[1], albedo[2], 1.0];
            vertices.push(v);
        }
    }
    let nc = cols.len() as u32;
    let mut indices = Vec::with_capacity((cols.len() - 1) * (rows.len() - 1) * 6);
    for r in 0..rows.len() as u32 - 1 {
        for c in 0..nc - 1 {
            let i = r * nc + c;
            indices.extend_from_slice(&[i, i + nc, i + 1, i + 1, i + nc, i + nc + 1]);
        }
    }
    (vertices, indices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_terrain::{CdlodConfig, LayerWeightMap, TerrainLayers, TerrainMap};

    /// Review sprint-w12-rev-g8 #2 (ADR-8870): with the sculpt tool on and the pointer over another
    /// panel, `]` still grows the brush, once per frame however many panes and passes draw.
    #[test]
    fn brush_keys_work_with_the_pointer_outside_the_viewport_once_per_frame() {
        let mut world = World::new();
        world.insert_resource(TerrainTool {
            active: true,
            ..TerrainTool::default()
        });
        let radius = |world: &World| match world.resource::<TerrainTool>().brush.shape {
            BrushShape::Circle { radius } => radius,
            _ => f32::NAN,
        };
        let before = radius(&world);
        let ctx = egui::Context::default();
        let camera = crate::viewport::EditorCamera::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            events: vec![
                egui::Event::PointerMoved(egui::pos2(700.0, 550.0)),
                egui::Event::Key {
                    key: egui::Key::CloseBracket,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            // Two panes, neither under the pointer, each drawn twice (a repeated pass).
            for pane in 0..2 {
                let rect = egui::Rect::from_min_size(
                    egui::pos2(10.0 + 200.0 * pane as f32, 10.0),
                    egui::vec2(150.0, 150.0),
                );
                let response =
                    ui.interact(rect, egui::Id::new(("pane", pane)), egui::Sense::click_and_drag());
                for _ in 0..2 {
                    viewport_input(ui, &mut world, &response, &camera, Vec2::new(150.0, 150.0));
                }
            }
        });
        // No renderer here: the first frame's font atlas delta is dropped on purpose (epaint
        // panics on a dropped delta that was never handled or cleared).
        output.textures_delta.clear();
        let after = radius(&world);
        assert!(
            (after - before * 1.15).abs() < 1e-4,
            "{before} -> {after}: one step, pointer elsewhere"
        );
    }

    fn world_with_terrain() -> (World, Entity) {
        let mut world = World::new();
        world.init_resource::<UndoStack>();
        let e = spawn_terrain(&mut world);
        (world, e)
    }

    fn spawn_terrain(world: &mut World) -> Entity {
        let n = 129u32;
        let heights = (0..n * n)
            .map(|i| 20.0 + ((i % n) as f32 * 0.09).sin() * 2.0 + ((i / n) as f32 * 0.07).cos() * 1.5)
            .collect();
        let map = TerrainMap::from_heights(n, n, heights, 128.0, 100.0, 0.0).expect("map");
        let terrain = Terrain::new(
            &map.to_heightfield(),
            128.0,
            100.0,
            0.0,
            CdlodConfig::default(),
            4,
            TerrainLayers::default(),
        )
        .expect("terrain");
        let paint = TerrainPaint(LayerWeightMap::new(n, n, 1).expect("paint"));
        world.spawn((terrain, paint)).id()
    }

    /// Starts a threaded raise stroke and writes one batch of it, the way `pump` does.
    fn half_a_stroke(world: &mut World, e: Entity) -> Vec<u32> {
        world.init_resource::<TerrainTool>();
        let before = heights(world, e);
        assert!(begin_stroke(world, e, Vec2::new(0.0, 0.0), false));
        let dirty = regia_terrain::DirtyRect {
            col_start: 10,
            col_end: 12,
            row_start: 20,
            row_end: 21,
        };
        let batch = regia_terrain::brush::StrokeBatch {
            values: vec![
                (regia_core::math::IVec2::new(10, 20), 90.0),
                (regia_core::math::IVec2::new(11, 20), 91.0),
            ],
            dirty,
            tiles: BTreeSet::from([(0, 0)]),
        };
        world.resource_scope::<TerrainTool, ()>(|world, mut tool| {
            let partial = tool.partial.as_mut().expect("a stroke");
            let batch = rebase_height_batch(world, e, Some(&mut *partial), batch);
            apply_height_batch(world, e, &batch);
            partial.note(&batch.tiles);
        });
        before
    }

    /// The sample a reference stroke changed most, with its value before and after.
    fn reference_stroke(brush: Brush, at: Vec2) -> (usize, f32, f32) {
        let (mut world, e) = world_with_terrain();
        world.init_resource::<TerrainTool>();
        world.resource_mut::<TerrainTool>().brush = brush;
        let before: Vec<f32> = world.get::<Terrain>(e).expect("terrain").map.heights().to_vec();
        assert!(begin_stroke(&mut world, e, at, false));
        end_stroke(&mut world);
        pump(&mut world, true);
        let after: Vec<f32> = world.get::<Terrain>(e).expect("terrain").map.heights().to_vec();
        let (i, _) = before
            .iter()
            .zip(&after)
            .map(|(b, a)| (a - b).abs())
            .enumerate()
            .fold((0, 0.0f32), |best, (i, d)| if d > best.1 { (i, d) } else { best });
        (i, before[i], after[i])
    }

    /// deep-zai-terrain-stream #3: an edit made while a threaded editor stroke is open (an agent
    /// stroke on the same samples) is kept: the brush lands its change on top of it, and undoing the
    /// editor stroke goes back to the agent's value, not to the stroke-start snapshot.
    #[test]
    fn an_edit_made_during_a_threaded_stroke_is_kept_and_survives_its_undo() {
        let brush = Brush {
            shape: BrushShape::Circle { radius: 6.0 },
            mode: BrushMode::Raise { amount: 3.0 },
            hardness: 0.5,
            ..Brush::default()
        };
        let at = Vec2::new(4.0, -3.0);
        let (index, start, stroked) = reference_stroke(brush, at);
        let delta = stroked - start;
        assert!(delta > 0.5, "the reference stroke raised a sample: {delta}");

        let (mut world, e) = world_with_terrain();
        world.init_resource::<TerrainTool>();
        world.resource_mut::<TerrainTool>().brush = brush;
        assert!(begin_stroke(&mut world, e, at, false));
        // A foreign edit lands after the stroke's snapshot, before its batches.
        let foreign = start - 7.25;
        let width = world.get::<Terrain>(e).expect("terrain").map.width() as usize;
        let p = regia_core::math::IVec2::new((index % width) as i32, (index / width) as i32);
        {
            let mut query = world.query::<(&mut Terrain, Option<&mut TerrainEdits>)>();
            let (mut terrain, _) = query.get_mut(&mut world, e).expect("terrain");
            regia_terrain::brush::StrokeTarget::write(&mut terrain.map, p, foreign);
        }
        let others_before = heights(&world, e);
        end_stroke(&mut world);
        pump(&mut world, true);
        let landed = world.get::<Terrain>(e).expect("terrain").map.heights()[index];
        assert!(
            (landed - (foreign + delta)).abs() < 1e-3,
            "the brush lands on the foreign edit: {landed} vs {}",
            foreign + delta
        );
        world.resource_scope::<UndoStack, _>(|world, mut stack| assert!(stack.undo(world)));
        assert_eq!(
            heights(&world, e),
            others_before,
            "undo restores the foreign edit and every other sample bit for bit"
        );
    }

    #[test]
    fn a_worker_lost_before_the_end_message_leaves_an_undoable_stroke() {
        let (mut world, e) = world_with_terrain();
        let before = half_a_stroke(&mut world, e);
        world.resource_mut::<TerrainTool>().heights = Some(Mutex::new(BrushThread::without_worker()));
        end_stroke(&mut world);
        pump(&mut world, false);
        assert!(world.resource::<TerrainTool>().stroke.is_none());
        assert!(world.resource::<TerrainTool>().partial.is_none());
        assert_ne!(heights(&world, e), before);
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        world.resource_scope::<UndoStack, _>(|world, mut stack| assert!(stack.undo(world)));
        assert_eq!(
            heights(&world, e),
            before,
            "undo restores the samples that landed"
        );
        // The dead handle is gone: the next stroke gets a worker.
        assert!(world.resource::<TerrainTool>().heights.is_none());
        assert!(begin_stroke(&mut world, e, Vec2::ZERO, false));
    }

    #[test]
    fn a_save_does_not_spin_when_the_worker_died_after_the_end_message() {
        let (mut world, e) = world_with_terrain();
        let before = half_a_stroke(&mut world, e);
        end_stroke(&mut world);
        assert!(
            world
                .resource::<TerrainTool>()
                .stroke
                .is_some_and(|s| s.ended && !s.orphaned)
        );
        world.resource_mut::<TerrainTool>().heights = Some(Mutex::new(BrushThread::without_worker()));
        pump(&mut world, true);
        assert!(world.resource::<TerrainTool>().stroke.is_none());
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        world.resource_scope::<UndoStack, _>(|world, mut stack| assert!(stack.undo(world)));
        assert_eq!(heights(&world, e), before);
    }

    #[test]
    fn a_frame_notices_a_worker_that_died_after_the_end_message() {
        let (mut world, e) = world_with_terrain();
        let before = half_a_stroke(&mut world, e);
        end_stroke(&mut world);
        world.resource_mut::<TerrainTool>().heights = Some(Mutex::new(BrushThread::without_worker()));
        // The editor's per-frame pump, no save involved.
        pump(&mut world, false);
        assert!(
            world.resource::<TerrainTool>().stroke.is_none(),
            "the next stroke can start"
        );
        world.resource_scope::<UndoStack, _>(|world, mut stack| assert!(stack.undo(world)));
        assert_eq!(heights(&world, e), before);
    }

    #[test]
    fn a_stroke_with_no_worker_handle_left_also_ends() {
        let (mut world, e) = world_with_terrain();
        let before = half_a_stroke(&mut world, e);
        world.resource_mut::<TerrainTool>().heights = None;
        end_stroke(&mut world);
        pump(&mut world, true);
        assert!(world.resource::<TerrainTool>().stroke.is_none());
        world.resource_scope::<UndoStack, _>(|world, mut stack| assert!(stack.undo(world)));
        assert_eq!(heights(&world, e), before);
    }

    #[test]
    fn two_terrains_with_unsaved_paint_or_holes_refuse_to_share_one_file() {
        let (mut world, a) = world_with_terrain();
        let second = spawn_terrain(&mut world);
        let rect = regia_terrain::DirtyRect {
            col_start: 0,
            col_end: 1,
            row_start: 0,
            row_end: 1,
        };
        for e in [a, second] {
            let mut edits = TerrainEdits::default();
            edits.note_paint(rect);
            world.entity_mut(e).insert(edits);
        }
        let dir = regia_test_utils::unique_temp_dir("terrain_two_paints");
        std::fs::create_dir_all(&dir).expect("dir");
        let level = dir.join("map.ron");
        let err = save_terrain_paint(&mut world, &level).expect_err("two terrains");
        assert!(err.contains("2 terrains"), "{err}");
        assert!(!layer_weights_path(&level).exists());
        assert!(world.get::<TerrainEdits>(a).expect("edits").unsaved_paint());

        world.init_resource::<TerrainTool>();
        let hole = |id: &str, x: f32| NamedHole {
            id: id.into(),
            open: true,
            shape: HoleShape::Ellipse(HoleEllipse {
                center: [x, 0.0],
                radius: [2.0, 2.0],
                rotation_radians: 0.0,
            }),
        };
        assert!(write_hole(&mut world, a, "a", Some(&hole("a", 0.0))));
        assert!(save_terrain_holes(&mut world, &level).is_ok());
        assert!(write_hole(&mut world, a, "b", Some(&hole("b", 5.0))));
        assert!(write_hole(&mut world, second, "c", Some(&hole("c", 9.0))));
        let err = save_terrain_holes(&mut world, &level).expect_err("two terrains");
        assert!(err.contains("2 terrains"), "{err}");
        assert_eq!(world.resource::<TerrainTool>().holes_dirty.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A Save As that writes holes and then refuses the paint must leave the holes dirty and
    /// the open path alone, so a retry rewrites the holes beside the level that is still open.
    #[test]
    fn save_as_keeps_holes_dirty_when_a_later_step_fails() {
        use regia_app::App;

        let dir = regia_test_utils::unique_temp_dir("terrain_save_as_partial");
        let open_path = dir.join("town.level.ron");
        let copy_path = dir.join("town-copy.level.ron");
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let mut open = crate::level_io::new_level(app.world_mut());
        open.path = Some(open_path.clone());
        app.world_mut().insert_resource(open);
        let world = app.world_mut();
        world.init_resource::<TerrainTool>();
        let first = spawn_terrain(world);
        let second = spawn_terrain(world);
        let hole = NamedHole {
            id: "gap".into(),
            open: true,
            shape: HoleShape::Ellipse(HoleEllipse {
                center: [1.0, 2.0],
                radius: [2.0, 2.0],
                rotation_radians: 0.0,
            }),
        };
        assert!(write_hole(world, first, "gap", Some(&hole)));
        let rect = regia_terrain::DirtyRect {
            col_start: 0,
            col_end: 1,
            row_start: 0,
            row_end: 1,
        };
        for entity in [first, second] {
            let mut edits = TerrainEdits::default();
            edits.note_paint(rect);
            world.entity_mut(entity).insert(edits);
        }
        let open = world.resource::<crate::level_io::OpenLevel>().clone();
        let err = crate::level_io::save_level_and_terrain(world, &open, &copy_path)
            .expect_err("two terrains of paint");
        assert!(
            err.contains("unsaved paint"),
            "the failure is the paint step, after holes were written: {err}"
        );
        assert!(
            holes_ron_path(&copy_path).is_file(),
            "the holes file was written before the paint refusal"
        );
        assert!(
            world.resource::<TerrainTool>().holes_dirty.contains(&first),
            "holes stay dirty so a retry rewrites them beside the path that is still open"
        );
        assert_eq!(
            world.resource::<crate::level_io::OpenLevel>().path.as_deref(),
            Some(open_path.as_path())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A save that finishes, followed by [`commit_terrain_save`] after the path is stored, clears
    /// the holes that the save wrote.
    #[test]
    fn commit_clears_holes_only_after_the_save_has_finished() {
        use regia_app::App;

        let dir = regia_test_utils::unique_temp_dir("terrain_save_commit");
        let level = dir.join("town.level.ron");
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let mut open = crate::level_io::new_level(app.world_mut());
        open.path = Some(level.clone());
        app.world_mut().insert_resource(open.clone());
        let world = app.world_mut();
        world.init_resource::<TerrainTool>();
        let entity = spawn_terrain(world);
        let hole = NamedHole {
            id: "gap".into(),
            open: true,
            shape: HoleShape::Ellipse(HoleEllipse {
                center: [1.0, 2.0],
                radius: [2.0, 2.0],
                rotation_radians: 0.0,
            }),
        };
        assert!(write_hole(world, entity, "gap", Some(&hole)));
        crate::level_io::save_level_and_terrain(world, &open, &level).expect("save");
        assert!(
            world.resource::<TerrainTool>().holes_dirty.contains(&entity),
            "the orchestrated save leaves the holes dirty until the path is stored"
        );
        open.path = Some(level.clone());
        world.insert_resource(open);
        commit_terrain_save(world);
        assert!(world.resource::<TerrainTool>().holes_dirty.is_empty());
        assert_eq!(save_terrain_holes(world, &level), Ok(None));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Audit wave 5 (terrain-g2b M): two terrains with unsaved scatter refuse to share the
    /// level's one scatter file; nothing is written and both stay unsaved.
    #[test]
    fn two_terrains_with_unsaved_scatter_refuse_to_share_one_file() {
        let (mut world, a) = world_with_terrain();
        let second = spawn_terrain(&mut world);
        for e in [a, second] {
            let mut edits = TerrainEdits::default();
            edits.mark_scatter_unsaved();
            world
                .entity_mut(e)
                .insert((TerrainScatterPaint::default(), edits));
        }
        let dir = regia_test_utils::unique_temp_dir("terrain_two_scatters");
        std::fs::create_dir_all(&dir).expect("dir");
        let level = dir.join("map.level.ron");
        let err = save_terrain_scatter(&mut world, &level).expect_err("two terrains");
        assert!(err.contains("2 terrains"), "{err}");
        assert!(!scatter_paint_path(&level).exists());
        for e in [a, second] {
            assert!(world.get::<TerrainEdits>(e).expect("edits").unsaved_scatter());
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_hole_on_an_entity_without_a_terrain_is_refused() {
        let mut world = World::new();
        world.init_resource::<TerrainTool>();
        let plain = world.spawn_empty().id();
        let hole = NamedHole {
            id: "h".into(),
            open: true,
            shape: HoleShape::Ellipse(HoleEllipse {
                center: [0.0, 0.0],
                radius: [1.0, 1.0],
                rotation_radians: 0.0,
            }),
        };
        assert!(!write_hole(&mut world, plain, "h", Some(&hole)));
        assert!(world.resource::<TerrainTool>().holes_dirty.is_empty());
    }

    #[test]
    fn a_transposed_paint_map_starts_no_stroke() {
        let (mut world, e) = world_with_terrain();
        world.init_resource::<TerrainTool>();
        // As many samples as the 129 × 129 map, so only the shape check can refuse it.
        world
            .entity_mut(e)
            .insert(TerrainPaint(LayerWeightMap::new(43, 387, 1).expect("paint")));
        world.resource_mut::<TerrainTool>().brush.target = BrushTarget::LayerWeight { layer: 0 };
        assert!(!begin_stroke(&mut world, e, Vec2::ZERO, false));
    }

    #[test]
    fn draining_the_log_marks_it_as_kept_by_an_editor() {
        let (mut world, e) = world_with_terrain();
        world.init_resource::<TerrainStrokeLog>();
        drain_stroke_log(&mut world);
        assert!(world.resource::<TerrainStrokeLog>().drained);
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
        assert!(world.get::<Terrain>(e).is_some());
    }

    fn heights(world: &World, e: Entity) -> Vec<u32> {
        world
            .get::<Terrain>(e)
            .map(|t| t.map.heights().iter().map(|h| h.to_bits()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn a_dragged_stroke_is_one_undo_step_that_restores_bit_exact() {
        let (mut world, e) = world_with_terrain();
        let original = heights(&world, e);
        world.init_resource::<TerrainTool>();
        world.resource_mut::<TerrainTool>().brush = Brush {
            shape: BrushShape::Circle { radius: 6.0 },
            mode: BrushMode::Raise { amount: 3.0 },
            hardness: 0.5,
            ..Brush::default()
        };
        assert!(begin_stroke(&mut world, e, Vec2::new(-20.0, 0.0), false));
        for i in 0..20 {
            continue_stroke(
                &mut world,
                Vec2::new(-20.0 + i as f32 * 2.0, (i as f32 * 0.4).sin() * 5.0),
            );
            pump(&mut world, false);
        }
        end_stroke(&mut world);
        pump(&mut world, true);
        assert!(!world.resource::<TerrainTool>().stroking());
        let edited = heights(&world, e);
        assert_ne!(edited, original);
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            1,
            "one stroke, one undo entry"
        );
        world.resource_scope::<UndoStack, _>(|world, mut stack| assert!(stack.undo(world)));
        assert_eq!(
            heights(&world, e),
            original,
            "undo restores every height bit for bit"
        );
        world.resource_scope::<UndoStack, _>(|world, mut stack| assert!(stack.redo(world)));
        assert_eq!(heights(&world, e), edited);
        let edits = world.get::<TerrainEdits>(e).expect("edits");
        assert!(!edits.unsaved_tiles().is_empty());
    }

    /// Ramp (`7`) applies on release between the heights under both ends, as one undo step;
    /// Noise (`6`) draws a new seed each stroke (ADR-1853).
    #[test]
    fn ramp_and_noise_strokes_in_the_tool() {
        let (mut world, e) = world_with_terrain();
        let original = heights(&world, e);
        world.init_resource::<TerrainTool>();
        {
            let mut tool = world.resource_mut::<TerrainTool>();
            tool.brush.shape = BrushShape::Circle { radius: 5.0 };
            tool.brush.hardness = 1.0;
            tool.select_mode(7);
        }
        let (from, to) = (Vec2::new(-20.0, 0.0), Vec2::new(20.0, 0.0));
        let (h0, h1) = {
            let t = world.get::<Terrain>(e).expect("terrain");
            (
                t.map.sample_bilinear(from.x, from.y),
                t.map.sample_bilinear(to.x, to.y),
            )
        };
        assert!(begin_stroke(&mut world, e, from, false));
        assert!(world.resource::<TerrainTool>().stroking());
        continue_stroke(&mut world, Vec2::new(0.0, 3.0));
        continue_stroke(&mut world, to);
        assert_eq!(heights(&world, e), original, "a ramp waits for the release");
        end_stroke(&mut world);
        assert!(!world.resource::<TerrainTool>().stroking());
        let mid = world
            .get::<Terrain>(e)
            .expect("terrain")
            .map
            .sample_bilinear(0.0, 0.0);
        assert!(
            (mid - (h0 + h1) * 0.5).abs() < 0.05,
            "{mid} between {h0} and {h1}"
        );
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        world.resource_scope::<UndoStack, _>(|world, mut stack| assert!(stack.undo(world)));
        assert_eq!(heights(&world, e), original);

        world.resource_mut::<TerrainTool>().select_mode(6);
        assert_eq!(world.resource::<TerrainTool>().brush.mode.name(), "noise");
        assert!(begin_stroke(&mut world, e, Vec2::ZERO, false));
        end_stroke(&mut world);
        pump(&mut world, true);
        let first = heights(&world, e);
        assert_ne!(first, original);
        let BrushMode::Noise { seed, .. } = world.resource::<TerrainTool>().brush.mode else {
            panic!("noise mode");
        };
        assert_eq!(seed, 1, "the first noise stroke used seed 1");
        assert!(world.resource::<TerrainTool>().status().contains("noise"));
    }

    /// Spec 82 § 6: each Noise stroke takes the next seed, so a repeated stroke at one spot adds
    /// a different field instead of doubling the first; the status line names Noise and Ramp.
    #[test]
    fn repeated_noise_strokes_draw_new_seeds_and_the_status_names_the_modes() {
        let (mut world, e) = world_with_terrain();
        let original = heights(&world, e);
        world.init_resource::<TerrainTool>();
        {
            let mut tool = world.resource_mut::<TerrainTool>();
            tool.brush.shape = BrushShape::Circle { radius: 6.0 };
            tool.brush.hardness = 1.0;
            tool.select_mode(7);
            assert!(tool.status().contains("ramp"), "{}", tool.status());
            tool.select_mode(6);
            assert!(tool.status().contains("noise"), "{}", tool.status());
        }
        let stroke = |world: &mut World| -> Vec<f32> {
            let before = heights(world, e);
            assert!(begin_stroke(world, e, Vec2::ZERO, false));
            end_stroke(world);
            pump(world, true);
            heights(world, e)
                .iter()
                .zip(&before)
                .map(|(a, b)| f32::from_bits(*a) - f32::from_bits(*b))
                .collect()
        };
        let first = stroke(&mut world);
        let second = stroke(&mut world);
        let seed = match world.resource::<TerrainTool>().brush.mode {
            BrushMode::Noise { seed, .. } => seed,
            other => panic!("noise mode, got {other:?}"),
        };
        assert_eq!(seed, 2, "two strokes, two seeds");
        assert!(
            first.iter().any(|d| d.abs() > 1e-4),
            "the first stroke moved heights"
        );
        assert!(
            second.iter().any(|d| d.abs() > 1e-4),
            "the second stroke moved heights"
        );
        assert!(
            first.iter().zip(&second).any(|(a, b)| (a - b).abs() > 1e-3),
            "the second stroke is another field, not the first again"
        );
        // Each stroke is its own undo entry; undoing both restores the map.
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 2);
        world.resource_scope::<UndoStack, _>(|world, mut stack| {
            assert!(stack.undo(world));
            assert!(stack.undo(world));
        });
        assert_eq!(heights(&world, e), original);
    }

    /// The drag Hole mode (`8`) applies: `from` to `to`, `invert` for Shift.
    fn hole_drag(world: &mut World, e: Entity, from: Vec2, to: Vec2, invert: bool) {
        assert!(begin_stroke(world, e, from, invert));
        assert!(world.resource::<TerrainTool>().stroking());
        continue_stroke(world, to);
        end_stroke(world);
        assert!(!world.resource::<TerrainTool>().stroking());
    }

    fn hole_tool(world: &mut World) {
        world.init_resource::<TerrainTool>();
        let mut tool = world.resource_mut::<TerrainTool>();
        tool.brush.shape = BrushShape::Circle { radius: 3.0 };
        tool.select_mode(8);
        assert!(tool.cut_holes);
        assert!(tool.status().contains("hole"));
    }

    fn cuts(world: &World, e: Entity, x: f32, z: f32) -> bool {
        current_holes(world, e).contains(x, z)
    }

    #[test]
    fn hole_mode_cuts_undoes_redoes_and_removes() {
        let (mut world, e) = world_with_terrain();
        hole_tool(&mut world);

        // A click: a round hole of the brush's size.
        hole_drag(&mut world, e, Vec2::new(10.0, 0.0), Vec2::new(10.0, 0.0), false);
        assert!(cuts(&world, e, 12.5, 0.0));
        assert!(!cuts(&world, e, 13.5, 0.0));
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);

        // A drag: a slot along it, as wide as the brush, with the next free id.
        hole_drag(&mut world, e, Vec2::new(-20.0, 5.0), Vec2::new(-5.0, 5.0), false);
        assert!(cuts(&world, e, -12.0, 7.5));
        assert!(!cuts(&world, e, -12.0, 9.0));
        let mask = current_holes(&world, e);
        assert_eq!(mask.named.len(), 2);
        assert!(mask.named_hole("editor_hole_1").is_some());
        assert!(mask.named_hole("editor_hole_2").is_some());

        // Undo and redo the slot, one entry each.
        world.resource_scope::<UndoStack, _>(|world, mut stack| assert!(stack.undo(world)));
        assert!(!cuts(&world, e, -12.0, 7.5));
        assert!(cuts(&world, e, 12.5, 0.0));
        world.resource_scope::<UndoStack, _>(|world, mut stack| assert!(stack.redo(world)));
        assert!(cuts(&world, e, -12.0, 7.5));

        // Shift removes the hole under the release point; undo brings it back.
        hole_drag(&mut world, e, Vec2::new(10.0, 0.0), Vec2::new(10.0, 0.0), true);
        assert!(!cuts(&world, e, 10.0, 0.0));
        world.resource_scope::<UndoStack, _>(|world, mut stack| assert!(stack.undo(world)));
        assert!(cuts(&world, e, 10.0, 0.0));

        // Shift over solid ground removes nothing and adds no undo entry.
        let depth = world.resource::<UndoStack>().undo_depth();
        hole_drag(&mut world, e, Vec2::new(40.0, 40.0), Vec2::new(40.0, 40.0), true);
        assert_eq!(world.resource::<UndoStack>().undo_depth(), depth);

        // Choosing another mode leaves Hole mode.
        world.resource_mut::<TerrainTool>().select_mode(1);
        assert!(!world.resource::<TerrainTool>().cut_holes);
    }

    #[test]
    fn hole_edits_go_through_a_linked_draw() {
        let (mut world, e) = world_with_terrain();
        hole_tool(&mut world);
        let handle = TerrainHoles::new_linked();
        world.insert_resource(handle.clone());
        hole_drag(&mut world, e, Vec2::ZERO, Vec2::ZERO, false);
        assert!(handle.state().requested.is_some_and(|m| m.contains(0.0, 0.0)));
        assert!(world.get::<Terrain>(e).expect("terrain").holes.is_none());
    }

    #[test]
    fn a_save_writes_edited_holes_and_keeps_the_ones_on_disk() {
        let root = regia_test_utils::unique_temp_dir("editor_terrain_holes");
        let level = root.join("arena.level.ron");
        let file = root.join("arena.holes.ron");
        let (mut world, e) = world_with_terrain();
        hole_tool(&mut world);
        world.insert_resource(crate::level_io::OpenLevel {
            id: Default::default(),
            path: Some(level.clone()),
        });

        // No edit: nothing is written, whatever the session's mask says.
        assert_eq!(save_terrain_holes(&mut world, &level), Ok(None));
        assert!(!file.exists());

        // A hole already on disk, never loaded by this session.
        let mut saved = HolesMask::default();
        saved
            .set_named(NamedHole {
                id: "mine_mouth".into(),
                open: true,
                shape: HoleShape::segment([30.0, 30.0], [40.0, 30.0], 4.0),
            })
            .expect("hole");
        std::fs::write(&file, saved.to_holes_ron().expect("ron")).expect("write");
        assert_eq!(save_terrain_holes(&mut world, &level), Ok(None));

        // The first edit brings it in; the save holds both, and a second save writes nothing.
        hole_drag(&mut world, e, Vec2::ZERO, Vec2::ZERO, false);
        assert!(cuts(&world, e, 35.0, 30.0), "the saved hole is shown");
        assert_eq!(save_terrain_holes(&mut world, &level), Ok(Some(file.clone())));
        let back = load_holes_ron(&file).expect("read").expect("file");
        assert!(back.named_hole("mine_mouth").is_some());
        assert!(back.named_hole("editor_hole_1").is_some());
        assert_eq!(save_terrain_holes(&mut world, &level), Ok(None));

        // Undo after a save is an edit too: the file follows.
        world.resource_scope::<UndoStack, _>(|world, mut stack| assert!(stack.undo(world)));
        assert_eq!(save_terrain_holes(&mut world, &level), Ok(Some(file.clone())));
        let back = load_holes_ron(&file).expect("read").expect("file");
        assert!(back.named_hole("editor_hole_1").is_none());
        assert!(back.named_hole("mine_mouth").is_some());

        // An agent cut a hole before the first editor edit: the file's holes still come in.
        let (mut mixed, me) = world_with_terrain();
        hole_tool(&mut mixed);
        mixed.insert_resource(crate::level_io::OpenLevel {
            id: Default::default(),
            path: Some(level.clone()),
        });
        edit_holes(&mut mixed, me, |mask| {
            mask.set_named(NamedHole {
                id: "from_agent".into(),
                open: true,
                shape: HoleShape::segment([-30.0, -30.0], [-25.0, -30.0], 2.0),
            })
            .map(|_| ())
        })
        .expect("agent hole");
        hole_drag(&mut mixed, me, Vec2::ZERO, Vec2::ZERO, false);
        let mask = current_holes(&mixed, me);
        assert!(mask.named_hole("from_agent").is_some());
        assert!(mask.named_hole("mine_mouth").is_some());
        assert_eq!(next_hole_id(&mask), "editor_hole_2");

        // A broken file refuses the edit, so a save cannot replace it.
        let (mut other, oe) = world_with_terrain();
        hole_tool(&mut other);
        other.insert_resource(crate::level_io::OpenLevel {
            id: Default::default(),
            path: Some(level.clone()),
        });
        std::fs::write(&file, "not ron").expect("write");
        hole_drag(&mut other, oe, Vec2::ZERO, Vec2::ZERO, false);
        assert!(!cuts(&other, oe, 0.0, 0.0));
        assert_eq!(save_terrain_holes(&mut other, &level), Ok(None));
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "not ron");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_save_writes_painted_weights_only_after_a_paint_edit() {
        let root = regia_test_utils::unique_temp_dir("editor_terrain_paint");
        let level = root.join("arena.level.ron");
        let file = root.join("arena.weights.bin");
        let (mut world, e) = world_with_terrain();
        world.init_resource::<TerrainTool>();
        world.entity_mut(e).insert(TerrainEdits::default());
        assert_eq!(save_terrain_paint(&mut world, &level), Ok(None));

        // A sculpt alone writes no weights.
        assert!(begin_stroke(&mut world, e, Vec2::ZERO, false));
        end_stroke(&mut world);
        pump(&mut world, true);
        assert_eq!(save_terrain_paint(&mut world, &level), Ok(None));
        assert!(!file.exists());

        // A paint stroke on the threaded path does, once, and the blob reads back.
        world.resource_mut::<TerrainTool>().select_mode(5);
        assert!(begin_stroke(&mut world, e, Vec2::ZERO, false));
        end_stroke(&mut world);
        pump(&mut world, true);
        assert_eq!(save_terrain_paint(&mut world, &level), Ok(Some(file.clone())));
        let bytes = std::fs::read(&file).expect("read");
        let back = LayerWeightMap::from_bytes(&bytes).expect("decode");
        assert_eq!(world.get::<TerrainPaint>(e).map(|p| &p.0), Some(&back));
        assert_eq!(save_terrain_paint(&mut world, &level), Ok(None));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn scatter_set() -> regia_terrain::ScatterSet {
        let class = |name: &str| regia_terrain::ScatterClass {
            name: name.into(),
            density: 0.0,
            slope_max: 1.5,
            meshes: vec![regia_terrain::ScatterMeshRef {
                mesh: "rock".into(),
                weight: 1.0,
                min_scale: 1.0,
                max_scale: 1.0,
                scale_bias: 1.0,
            }],
            ..regia_terrain::ScatterClass::default()
        };
        regia_terrain::ScatterSet {
            classes: vec![class("trees"), class("bushes")],
            ..regia_terrain::ScatterSet::default()
        }
    }

    fn scatter_tool(world: &mut World) {
        world.init_resource::<TerrainTool>();
        world.insert_resource(regia_terrain::ScatterPaintSet(Arc::new(scatter_set())));
        let mut tool = world.resource_mut::<TerrainTool>();
        tool.brush.shape = BrushShape::Circle { radius: 8.0 };
        tool.brush.hardness = 1.0;
        tool.scatter_density = 0.5;
        tool.select_mode(9);
    }

    fn painted(world: &World, e: Entity) -> regia_terrain::PaintedScatter {
        world
            .get::<TerrainScatterPaint>(e)
            .map(|p| p.0.clone())
            .unwrap_or_default()
    }

    fn undo(world: &mut World) {
        world.resource_scope::<UndoStack, _>(|world, mut stack| assert!(stack.undo(world)));
    }

    fn redo(world: &mut World) {
        world.resource_scope::<UndoStack, _>(|world, mut stack| assert!(stack.redo(world)));
    }

    /// One drag is one undo step; undo and redo restore the store; Shift erases (ADR-1936).
    #[test]
    fn scatter_mode_paints_erases_undoes_and_redoes() {
        let (mut world, e) = world_with_terrain();
        scatter_tool(&mut world);
        assert!(world.resource::<TerrainTool>().status().contains("scatter"));

        assert!(begin_stroke(&mut world, e, Vec2::new(-15.0, 0.0), false));
        assert!(world.resource::<TerrainTool>().stroking());
        let first = painted(&world, e).len();
        assert!(first > 0, "the press paints at once");
        continue_stroke(&mut world, Vec2::new(0.0, 0.0));
        continue_stroke(&mut world, Vec2::new(15.0, 0.0));
        end_stroke(&mut world);
        assert!(!world.resource::<TerrainTool>().stroking());
        let drag = painted(&world, e);
        assert!(drag.len() > first, "the drag paints as it goes");
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            1,
            "one drag, one step"
        );
        assert!(drag.instances("trees").all(|i| i.position[2].abs() < 8.0));

        undo(&mut world);
        assert!(painted(&world, e).is_empty());
        redo(&mut world);
        assert_eq!(painted(&world, e), drag);

        // Shift erases what is under the brush, one step.
        assert!(begin_stroke(&mut world, e, Vec2::new(0.0, 0.0), true));
        end_stroke(&mut world);
        let erased = painted(&world, e);
        assert!(erased.len() < drag.len());
        assert!(
            erased
                .instances("trees")
                .all(|i| i.position[0].hypot(i.position[2]) >= 8.0)
        );
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 2);
        undo(&mut world);
        assert_eq!(painted(&world, e), drag);

        // The next class; another mode leaves Scatter; a stroke that changes nothing leaves no step.
        world.resource_mut::<TerrainTool>().step_layer(1);
        let depth = world.resource::<UndoStack>().undo_depth();
        assert!(begin_stroke(&mut world, e, Vec2::new(1000.0, 1000.0), false));
        end_stroke(&mut world);
        assert_eq!(world.resource::<UndoStack>().undo_depth(), depth);
        assert!(begin_stroke(&mut world, e, Vec2::new(0.0, 30.0), false));
        end_stroke(&mut world);
        assert!(painted(&world, e).instances("bushes").count() > 0);
        world.resource_mut::<TerrainTool>().select_mode(1);
        assert!(!world.resource::<TerrainTool>().paint_scatter);
    }

    #[test]
    fn scatter_mode_without_a_set_refuses() {
        let (mut world, e) = world_with_terrain();
        world.init_resource::<TerrainTool>();
        world.resource_mut::<TerrainTool>().select_mode(9);
        assert!(!begin_stroke(&mut world, e, Vec2::ZERO, false));
        assert!(!world.resource::<TerrainTool>().stroking());
    }

    /// An agent stroke goes through the same journal and undo stack.
    #[test]
    fn logged_scatter_strokes_become_undo_entries() {
        let (mut world, e) = world_with_terrain();
        scatter_tool(&mut world);
        let set = scatter_set();
        let brush = ScatterBrush {
            hardness: 1.0,
            density: 0.5,
            ..ScatterBrush::for_class(&set.classes[0])
        };
        world
            .entity_mut(e)
            .insert((TerrainScatterPaint::default(), TerrainEdits::default()));
        let mut query = world.query::<(&Terrain, &mut TerrainScatterPaint, &mut TerrainEdits)>();
        let (terrain, mut paint, mut edits) = query.get_mut(&mut world, e).expect("terrain");
        let stroke = apply_scatter_stroke(terrain, &mut paint, &mut edits, &set, &brush, &[Vec2::ZERO])
            .expect("stroke");
        world.init_resource::<TerrainStrokeLog>();
        world.resource_mut::<TerrainStrokeLog>().strokes.push((e, stroke));
        drain_stroke_log(&mut world);
        assert!(!painted(&world, e).is_empty());
        undo(&mut world);
        assert!(painted(&world, e).is_empty());
    }

    #[test]
    fn a_save_writes_painted_instances_and_a_level_load_brings_them_back() {
        let root = regia_test_utils::unique_temp_dir("editor_scatter_paint");
        let level = root.join("arena.level.ron");
        let file = root.join("arena.scatterpaint.bin");
        let (mut world, e) = world_with_terrain();
        scatter_tool(&mut world);
        world.entity_mut(e).insert(TerrainEdits::default());
        assert_eq!(save_terrain_scatter(&mut world, &level), Ok(None));

        assert!(begin_stroke(&mut world, e, Vec2::ZERO, false));
        end_stroke(&mut world);
        assert_eq!(save_terrain_scatter(&mut world, &level), Ok(Some(file.clone())));
        assert_eq!(
            save_terrain_scatter(&mut world, &level),
            Ok(None),
            "nothing changed"
        );
        let saved = painted(&world, e);

        // Save As: the new name has no file yet, so it gets one with no edit.
        let other = root.join("copy.level.ron");
        assert_eq!(
            save_terrain_scatter(&mut world, &other),
            Ok(Some(root.join("copy.scatterpaint.bin")))
        );

        // A new session: the file arrives with the level, once, and edits add to it.
        let (mut fresh, f) = world_with_terrain();
        scatter_tool(&mut fresh);
        fresh.insert_resource(crate::level_io::OpenLevel {
            id: Default::default(),
            path: Some(level.clone()),
        });
        frame_effects(&mut fresh);
        assert_eq!(painted(&fresh, f), saved);
        fresh.resource_mut::<TerrainTool>().scatter_density = 2.0;
        assert!(begin_stroke(&mut fresh, f, Vec2::new(20.0, 0.0), false));
        end_stroke(&mut fresh);
        let more = painted(&fresh, f);
        assert!(more.len() > saved.len());
        frame_effects(&mut fresh);
        assert_eq!(painted(&fresh, f), more, "one load per level");
        assert_eq!(save_terrain_scatter(&mut fresh, &level), Ok(Some(file.clone())));
        let back = regia_terrain::load_scatter_paint(&file)
            .expect("read")
            .expect("file");
        assert_eq!(back, more);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A sculpt under painted instances, and its undo, through the tool's own frame.
    #[test]
    fn a_sculpt_reseats_painted_instances_in_the_frame() {
        let (mut world, e) = world_with_terrain();
        scatter_tool(&mut world);
        assert!(begin_stroke(&mut world, e, Vec2::ZERO, false));
        end_stroke(&mut world);
        frame_effects(&mut world);
        let before = painted(&world, e);
        assert!(!before.is_empty());

        world.resource_mut::<TerrainTool>().select_mode(1);
        world.resource_mut::<TerrainTool>().brush.shape = BrushShape::Circle { radius: 20.0 };
        assert!(begin_stroke(&mut world, e, Vec2::ZERO, false));
        end_stroke(&mut world);
        pump(&mut world, true);
        frame_effects(&mut world);
        let raised = painted(&world, e);
        assert_ne!(raised, before);
        let terrain = world.get::<Terrain>(e).expect("terrain");
        for i in raised.instances("trees") {
            assert_eq!(
                i.position[1],
                terrain.map.sample_bilinear(i.position[0], i.position[2])
            );
        }

        undo(&mut world);
        frame_effects(&mut world);
        assert_eq!(painted(&world, e), before, "bit for bit");
    }

    #[test]
    fn a_second_press_does_not_lose_a_scatter_drag() {
        let (mut world, e) = world_with_terrain();
        scatter_tool(&mut world);
        assert!(begin_stroke(&mut world, e, Vec2::ZERO, false));
        assert!(!begin_stroke(&mut world, e, Vec2::new(30.0, 0.0), false));
        end_stroke(&mut world);
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
    }

    #[test]
    fn a_level_without_a_file_does_not_inherit_the_last_levels_instances() {
        let root = regia_test_utils::unique_temp_dir("editor_scatter_switch");
        let (mut world, e) = world_with_terrain();
        scatter_tool(&mut world);
        world.entity_mut(e).insert(TerrainEdits::default());
        let open = |path: PathBuf| crate::level_io::OpenLevel {
            id: Default::default(),
            path: Some(path),
        };
        let first = root.join("first.level.ron");
        world.insert_resource(open(first.clone()));
        frame_effects(&mut world);
        assert!(begin_stroke(&mut world, e, Vec2::ZERO, false));
        end_stroke(&mut world);
        assert!(save_terrain_scatter(&mut world, &first).expect("save").is_some());
        let saved = painted(&world, e);

        world.insert_resource(open(root.join("second.level.ron")));
        frame_effects(&mut world);
        assert!(painted(&world, e).is_empty());
        assert_eq!(
            save_terrain_scatter(&mut world, &root.join("second.level.ron")),
            Ok(None)
        );

        // And back: the first level's file returns.
        world.insert_resource(open(first));
        frame_effects(&mut world);
        assert_eq!(painted(&world, e), saved);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_broken_scatter_file_refuses_edits_and_is_never_replaced() {
        let root = regia_test_utils::unique_temp_dir("editor_scatter_broken");
        let level = root.join("arena.level.ron");
        let file = root.join("arena.scatterpaint.bin");
        std::fs::write(&file, b"junk").expect("write");
        let (mut world, e) = world_with_terrain();
        scatter_tool(&mut world);
        world.insert_resource(crate::level_io::OpenLevel {
            id: Default::default(),
            path: Some(level.clone()),
        });
        frame_effects(&mut world);
        assert!(!begin_stroke(&mut world, e, Vec2::ZERO, false));
        assert_eq!(save_terrain_scatter(&mut world, &level), Ok(None));
        assert_eq!(std::fs::read(&file).expect("read"), b"junk");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The editor's threaded stroke and `terrain.stroke`'s synchronous one, same brush and path.
    fn assert_threaded_matches_synchronous(brush: Brush) {
        let (mut a, ea) = world_with_terrain();
        let (mut b, eb) = world_with_terrain();
        a.init_resource::<TerrainTool>();
        a.resource_mut::<TerrainTool>().brush = brush;
        assert!(begin_stroke(&mut a, ea, Vec2::new(0.0, 0.0), false));
        continue_stroke(&mut a, Vec2::new(15.0, 4.0));
        end_stroke(&mut a);
        pump(&mut a, true);
        // The tool advances a noise seed as the stroke begins: the other path uses the same one.
        let brush = a.resource::<TerrainTool>().brush;
        let mut query = b.query::<(&mut Terrain, Option<&mut TerrainPaint>)>();
        let (mut terrain, _) = query.get_mut(&mut b, eb).expect("terrain");
        let mut edits = TerrainEdits::default();
        regia_terrain::apply_stroke(
            &mut terrain,
            None,
            &mut edits,
            &brush,
            &[Vec2::ZERO, Vec2::new(15.0, 4.0)],
        )
        .expect("stroke");
        assert_eq!(heights(&a, ea), heights(&b, eb));
    }

    #[test]
    fn threaded_stroke_matches_the_agent_path() {
        assert_threaded_matches_synchronous(Brush {
            shape: BrushShape::Circle { radius: 9.0 },
            mode: BrushMode::Smooth { kernel_radius: 2 },
            ..Brush::default()
        });
    }

    #[test]
    fn threaded_noise_stroke_matches_the_agent_path() {
        assert_threaded_matches_synchronous(Brush {
            shape: BrushShape::Circle { radius: 9.0 },
            mode: BrushMode::Noise {
                amount: 1.5,
                wavelength: 4.0,
                seed: 41,
            },
            ..Brush::default()
        });
    }

    #[test]
    fn painting_a_layer_and_agent_strokes_reach_the_undo_stack() {
        let (mut world, e) = world_with_terrain();
        world.init_resource::<TerrainTool>();
        world.resource_mut::<TerrainTool>().select_mode(5);
        assert!(begin_stroke(&mut world, e, Vec2::ZERO, false));
        end_stroke(&mut world);
        pump(&mut world, true);
        assert!(
            world
                .get::<TerrainPaint>(e)
                .is_some_and(|p| p.0.get(0, 64, 64) == 255)
        );
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        // A stroke logged by terrain.stroke becomes the next undo entry.
        let mut log = TerrainStrokeLog::default();
        let mut query = world.query::<(&mut Terrain, &mut TerrainEdits)>();
        let (mut terrain, mut edits) = query.get_mut(&mut world, e).expect("terrain");
        let undo = regia_terrain::apply_stroke(
            &mut terrain,
            None,
            &mut edits,
            &Brush {
                shape: BrushShape::Circle { radius: 4.0 },
                ..Brush::default()
            },
            &[Vec2::new(30.0, 30.0)],
        )
        .expect("stroke");
        log.strokes.push((e, undo));
        world.insert_resource(log);
        frame_effects(&mut world);
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 2);
        assert!(world.resource::<TerrainStrokeLog>().strokes.is_empty());
        // Painting a layer the map does not have is refused.
        world.resource_mut::<TerrainTool>().step_layer(3);
        assert!(!begin_stroke(&mut world, e, Vec2::ZERO, false));
    }

    #[test]
    fn hotkey_helpers_and_status() {
        let mut tool = TerrainTool::default();
        tool.scale_size(2.0);
        assert_eq!(tool.brush.shape, BrushShape::Circle { radius: 16.0 });
        tool.scale_strength(2.0);
        assert_eq!(tool.brush.mode, BrushMode::Raise { amount: 2.0 });
        tool.select_mode(3);
        tool.scale_strength(0.5);
        assert!((tool.brush.alpha - 0.5).abs() < 1e-6);
        tool.select_mode(5);
        tool.step_layer(2);
        assert_eq!(tool.brush.target, BrushTarget::LayerWeight { layer: 2 });
        assert!(tool.status().contains("layer 2"));
        tool.select_mode(1);
        assert_eq!(tool.brush.target, BrushTarget::Height);
    }

    /// Audit wave 5 (terrain-g2b Low): two terrains with unsaved edits whose cook targets name
    /// one cooked file are refused; nothing is written and both keep their unsaved tiles.
    #[test]
    fn two_terrains_cooking_to_one_file_refuse_to_save() {
        let (mut world, a) = world_with_terrain();
        let b = spawn_terrain(&mut world);
        let dir = regia_test_utils::unique_temp_dir("terrain_two_cook_targets");
        let map_id = AssetId::new_random();
        let brush = Brush {
            shape: BrushShape::Circle { radius: 4.0 },
            mode: BrushMode::Raise { amount: 5.0 },
            ..Brush::default()
        };
        for e in [a, b] {
            world.entity_mut(e).insert(TerrainCookTarget {
                map_id,
                cache_dir: dir.clone(),
            });
            let mut query = world.query::<&mut Terrain>();
            let mut terrain = query.get_mut(&mut world, e).expect("terrain");
            let terrain = &mut *terrain;
            let record = regia_terrain::brush::stroke_heights(
                &mut terrain.map,
                &brush,
                &[Vec2::new(-16.0, 16.0)],
                EDIT_CELL_SIZE,
            );
            let mut edits = TerrainEdits::default();
            edits.note(&terrain.map, &mut terrain.minmax, record.dirty, &record.tiles);
            world.entity_mut(e).insert(edits);
        }
        let err = save_terrain_edits(&mut world).expect_err("one file, two terrains");
        assert!(err.contains("two terrains"), "{err}");
        let id = regia_asset_pipeline::terrain_data::heightfield_asset_id(map_id);
        assert!(!dir.join(regia_asset::cooked_file_name(id)).exists());
        for e in [a, b] {
            assert!(
                !world
                    .get::<TerrainEdits>(e)
                    .expect("edits")
                    .unsaved_tiles()
                    .is_empty()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_tile_edit_saves_only_that_tile() {
        let (mut world, e) = world_with_terrain();
        let dir = std::env::temp_dir().join(format!("regia-terrain-tool-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let map_id = AssetId::new_random();
        world.entity_mut(e).insert(TerrainCookTarget {
            map_id,
            cache_dir: dir.clone(),
        });
        // A first save cooks everything (no cooked file yet).
        let map = world.get::<Terrain>(e).map(|t| t.map.clone()).expect("map");
        let full = regia_asset_pipeline::terrain_data::cook_tiled_heightfield(&map.to_heightfield(), 64)
            .expect("cook");
        let cooked = regia_terrain::CookedTiledHeightfield {
            version: full.version,
            width: full.width,
            height: full.height,
            tile_size: full.tile_size,
            tiles: full
                .tiles
                .iter()
                .map(|t| regia_terrain::CookedHeightfieldTile {
                    x: t.x,
                    y: t.y,
                    min: t.min,
                    max: t.max,
                    samples: t.samples.clone(),
                })
                .collect(),
        };
        write_cooked(map_id, cooked, &dir).expect("write");
        // A small stroke at sample (48, 80): edit cell (1, 2), inside 64-sample cook tile (0, 1).
        let brush = Brush {
            shape: BrushShape::Circle { radius: 4.0 },
            mode: BrushMode::Raise { amount: 5.0 },
            ..Brush::default()
        };
        {
            let mut query = world.query::<&mut Terrain>();
            let mut terrain = query.get_mut(&mut world, e).expect("terrain");
            let terrain = &mut *terrain;
            let record = regia_terrain::brush::stroke_heights(
                &mut terrain.map,
                &brush,
                &[Vec2::new(-16.0, 16.0)],
                EDIT_CELL_SIZE,
            );
            let mut edits = TerrainEdits::default();
            edits.note(&terrain.map, &mut terrain.minmax, record.dirty, &record.tiles);
            world.entity_mut(e).insert(edits);
        }
        let saves = save_terrain_edits(&mut world).expect("save");
        assert_eq!(saves.len(), 1);
        assert_eq!(saves[0].tiles_recooked, 1, "only the touched tile is re-cooked");
        assert_eq!(saves[0].tiles_total, 9);
        // The written file equals a full cook of the edited map, byte for byte.
        let edited = world.get::<Terrain>(e).map(|t| t.map.clone()).expect("map");
        let expect = regia_asset_pipeline::terrain_data::cook_tiled_heightfield(&edited.to_heightfield(), 64)
            .expect("cook");
        let bytes = std::fs::read(&saves[0].path).expect("read");
        let got = regia_terrain::decode_cooked_tiled_heightfield(&bytes).expect("decode");
        assert_eq!(got.tiles.len(), expect.tiles.len());
        for (g, x) in got.tiles.iter().zip(&expect.tiles) {
            assert_eq!((g.x, g.y, g.min, g.max), (x.x, x.y, x.min, x.max));
            assert_eq!(g.samples, x.samples);
        }
        // Nothing left to save.
        assert!(save_terrain_edits(&mut world).expect("save").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_mesh_is_bounded_and_coloured() {
        let (world, e) = world_with_terrain();
        let terrain = world.get::<Terrain>(e).expect("terrain");
        let (v, i) = preview_mesh(terrain, world.get::<TerrainPaint>(e), 32);
        // 129 samples at step 5 plus the closing edge: 27 columns and rows.
        assert_eq!(v.len(), 27 * 27);
        assert_eq!(i.len(), 26 * 26 * 6);
        assert!(i.iter().all(|&k| (k as usize) < v.len()));
        assert!(v.iter().all(|x| x.color[3] == 1.0 && x.color[0] > 0.0));
    }
}
