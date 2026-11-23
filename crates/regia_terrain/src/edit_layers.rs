// SPDX-License-Identifier: MPL-2.0
//! Non-destructive terrain edit layers (ADR-7825, `docs/specs/82-terrain-editing.md` § 12,
//! `docs/formats/terrain-editlayers.md`).
//!
//! An edit layer is a named heightfield over the terrain's **base** heights. The terrain the
//! game sees is the base folded through every enabled layer in stack order (bottom first):
//!
//! - an **Add** layer adds `opacity × delta` to the height below it;
//! - a **Set** layer moves the height below it toward its own target height by
//!   `coverage × opacity` (`coverage` is 0 where the layer was never painted, 1 where it was).
//!
//! Consecutive enabled Add layers are summed in `f64` in layer-id order before the sum is added,
//! so the order of Add layers never changes a bit of the result; moving an Add layer across a Set
//! layer does change it (the Set layer replaces what is below it).
//!
//! The folded heights live in [`crate::Terrain::map`] as before, so drawing, collision and the
//! cook read the flattened result and the cooked format does not change. A stroke still edits the
//! map (the live preview); when it lands, [`TerrainEditLayers::absorb_stroke`] moves its change
//! into the target layer and rewrites the touched samples with the canonical fold, and the
//! returned [`LayerStrokeUndo`] plus the composite [`crate::StrokeUndo`] restore both bit for bit.
//! A toggle, opacity change, reorder or delete recomputes only the samples the layer ever covered
//! ([`TerrainEditLayers::recomputed_samples`] counts them).
//!
//! The stack is saved beside the level as `<map>.terrain.editlayers` (postcard, version first,
//! sparse 64 × 64 chunks): the base is stored only where some layer covers it, because elsewhere
//! the cooked map already holds it.
//!
//! Version 2 (ADR-8381) adds a sparse material-weight delta on each layer and on the base, and an
//! optional procgen target (graph asset id plus output name) on each layer. A v1 file migrates to
//! a v2 file with empty weights and no target; every height value keeps its `f32` bits. After any
//! weight edit the folded weights of a sample sum to 1. A layer that names a procgen target is
//! owned by that graph: a brush into it is refused until the target is detached.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

use crate::Terrain;
use crate::brush::{StrokeRecord, TileCoord};
use crate::deform::DirtyRect;
use crate::edit::{EDIT_CELL_SIZE, StrokeUndo, TerrainEdits};
use crate::layers::MAX_LAYERS;
use crate::sampler::TerrainMap;

/// Version of the `<map>.terrain.editlayers` sidecar this build writes and reads.
pub const EDIT_LAYERS_FORMAT_VERSION: u32 = 2;

/// The sidecar version ADR-7825 froze. [`migrate_edit_layers_bytes`] reads it.
const EDIT_LAYERS_V1: u32 = 1;

/// Longest procgen graph id stored on a layer, in characters.
const MAX_PROCGEN_GRAPH_LEN: usize = 256;

/// Edge, in samples, of the chunks the sidecar stores.
pub const EDIT_LAYERS_CHUNK: u32 = 64;

/// Most layers one terrain may stack.
pub const MAX_EDIT_LAYERS: usize = 32;

/// Longest layer name, in characters.
pub const MAX_EDIT_LAYER_NAME: usize = 64;

/// Largest map (samples) a sidecar may describe: 8193 × 8193.
const MAX_EDIT_LAYER_SAMPLES: u64 = 8193 * 8193;

/// How an edit layer combines with the height below it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditLayerBlend {
    /// Adds `opacity × delta` (meters). Order among Add layers does not matter.
    #[default]
    Add,
    /// Moves the height toward the layer's target by `coverage × opacity`. Order matters.
    Set,
}

impl EditLayerBlend {
    /// `"add"` or `"set"`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Set => "set",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "add" => Some(Self::Add),
            "set" => Some(Self::Set),
            _ => None,
        }
    }
}

/// Why an edit-layer request was refused. [`Self::code`] is the agent error code.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum EditLayerError {
    /// No layer has this id.
    #[error("no edit layer with id {0}")]
    NotFound(u32),
    /// The layer is locked.
    #[error("edit layer `{0}` is locked")]
    Locked(String),
    /// The layer is off or fully transparent: a stroke into it would not show.
    #[error("edit layer `{0}` is hidden (off or opacity 0); a stroke into it would not show")]
    Hidden(String),
    /// The layer is owned by a procgen graph. Detach the target before brushing it.
    #[error(
        "edit layer `{name}` is owned by procgen graph {graph} output {output}; detach the target before brushing"
    )]
    Owned {
        /// The layer's name.
        name: String,
        /// The procgen graph asset id or path.
        graph: String,
        /// The output name inside that graph.
        output: String,
    },
    /// A weight channel is not finite, or more than [`MAX_LAYERS`] channels were given.
    #[error("edit layer weights are at most {MAX_LAYERS} finite numbers")]
    BadWeight,
    /// A procgen target's graph id or output name is empty, too long, or has a control character.
    #[error(
        "a procgen target needs a graph id of 1..={MAX_PROCGEN_GRAPH_LEN} characters and an output name of 1..={MAX_EDIT_LAYER_NAME} characters"
    )]
    BadTarget,
    /// [`MAX_EDIT_LAYERS`] layers already exist.
    #[error("a terrain holds at most {MAX_EDIT_LAYERS} edit layers")]
    Limit,
    /// The name is empty, too long or has control characters.
    #[error("an edit layer name is 1..={MAX_EDIT_LAYER_NAME} printable characters")]
    BadName,
    /// The opacity is not in `0..=1`.
    #[error("edit layer opacity is a number in 0..=1")]
    BadOpacity,
    /// The stack and the map have different grids.
    #[error("the edit layers are {layers:?} samples but the map is {map:?}")]
    SizeMismatch {
        /// `[columns, rows]` of the stack.
        layers: [u32; 2],
        /// `[columns, rows]` of the map.
        map: [u32; 2],
    },
    /// The sidecar cannot be decoded or holds out-of-range values.
    #[error("edit layers file: {0}")]
    File(String),
    /// The sidecar is a newer version.
    #[error("edit layers file version {0} (this build reads {EDIT_LAYERS_FORMAT_VERSION})")]
    Version(u32),
    /// The entity does not exist or has no terrain.
    #[error("entity {0} has no Terrain")]
    NoTerrain(u64),
    /// The cooked map no longer matches the sidecar's fold: the map was changed outside the layers.
    #[error("the terrain changed outside its edit layers ({0} samples differ); the layers file is stale")]
    Stale(usize),
}

impl EditLayerError {
    /// The lowercase agent error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "edit_layer_not_found",
            Self::Locked(_) => "edit_layer_locked",
            Self::Hidden(_) => "edit_layer_hidden",
            Self::Owned { .. } => "edit_layer_owned",
            Self::BadWeight => "edit_layer_bad_weight",
            Self::BadTarget => "edit_layer_bad_target",
            Self::Limit => "edit_layer_limit",
            Self::BadName => "edit_layer_bad_name",
            Self::BadOpacity => "edit_layer_bad_opacity",
            Self::SizeMismatch { .. } => "edit_layer_size_mismatch",
            Self::File(_) => "edit_layer_file_invalid",
            Self::Version(_) => "edit_layer_file_version",
            Self::Stale(_) => "edit_layer_file_stale",
            Self::NoTerrain(_) => "edit_layer_no_terrain",
        }
    }
}

/// One edit layer.
#[derive(Clone, Debug, PartialEq)]
pub struct EditLayer {
    /// Stable id (never reused within a stack).
    pub id: u32,
    /// Display name.
    pub name: String,
    /// The layer takes part in the fold.
    pub enabled: bool,
    /// Strokes, deletion and flattening are refused.
    pub locked: bool,
    /// `0..=1`.
    pub opacity: f32,
    /// How the layer combines with the height below.
    pub blend: EditLayerBlend,
    /// Add: delta, meters. Set: target height, meters. Row-major over the map.
    values: Arc<Vec<f32>>,
    /// Set only: per-sample coverage `0..=1` (empty for Add).
    coverage: Arc<Vec<f32>>,
    /// Signed material-weight deltas, sample index to [`MAX_LAYERS`] channels. A missing sample
    /// is an all-zero delta. Not a full grid: only painted samples are stored.
    weights: Arc<BTreeMap<usize, [f32; MAX_LAYERS]>>,
    /// When set, this layer is owned by that procgen output. Brush edits are refused.
    procgen: Option<ProcgenLayerTarget>,
    /// Bounds of every sample the layer ever touched (conservative; never shrinks).
    bounds: Option<DirtyRect>,
}

/// The procgen output that owns an edit layer (ADR-8381). Publish writes that layer only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcgenLayerTarget {
    /// Procgen graph asset id, or the project-relative graph path when the id is not a guid.
    pub graph: String,
    /// Output name inside `graph`.
    pub output: String,
}

/// One sample [`TerrainEditLayers::compose_into_layer`] wrote.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerSampleWrite {
    /// Row-major sample index.
    pub index: u32,
    /// The layer value before the write (Add: delta; Set: target height).
    pub before_value: f32,
    /// The layer coverage before the write (Add: 0).
    pub before_coverage: f32,
    /// The layer value after the write.
    pub after_value: f32,
    /// The layer coverage after the write.
    pub after_coverage: f32,
}

impl EditLayer {
    /// The layer's value at sample `index` (Add: delta; Set: target height).
    #[must_use]
    pub fn value(&self, index: usize) -> f32 {
        self.values.get(index).copied().unwrap_or(0.0)
    }

    /// The layer's coverage at sample `index` (Add layers: 1 where the delta is not zero).
    #[must_use]
    pub fn coverage(&self, index: usize) -> f32 {
        match self.blend {
            EditLayerBlend::Add => f32::from(u8::from(self.value(index) != 0.0)),
            EditLayerBlend::Set => self.coverage.get(index).copied().unwrap_or(0.0),
        }
    }

    /// Bounds of the samples the layer touched, `None` when it is empty.
    #[must_use]
    pub fn bounds(&self) -> Option<DirtyRect> {
        self.bounds
    }

    /// The procgen output that owns this layer, if one was attached.
    #[must_use]
    pub fn procgen(&self) -> Option<&ProcgenLayerTarget> {
        self.procgen.as_ref()
    }

    /// The signed weight delta at `index` (all zero when that sample was never painted).
    #[must_use]
    pub fn weight_delta(&self, index: usize) -> [f32; MAX_LAYERS] {
        self.weights.get(&index).copied().unwrap_or([0.0; MAX_LAYERS])
    }

    fn touches(&self, index: usize) -> bool {
        match self.blend {
            EditLayerBlend::Add => self.value(index) != 0.0,
            EditLayerBlend::Set => self.coverage.get(index).is_some_and(|c| *c > 0.0),
        }
    }
}

/// A partial change of a layer's settings ([`TerrainEditLayers::set`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EditLayerPatch {
    /// New name.
    pub name: Option<String>,
    /// On or off.
    pub enabled: Option<bool>,
    /// Locked or not.
    pub locked: Option<bool>,
    /// New opacity, `0..=1`.
    pub opacity: Option<f32>,
}

/// One fold step: a run of Add layers (indices, id order) or one Set layer.
enum Step {
    Add(Vec<usize>),
    Set(usize),
}

/// The edit-layer stack of one terrain (a component on the terrain entity).
#[derive(Component, Clone, Debug, PartialEq)]
pub struct TerrainEditLayers {
    width: u32,
    height: u32,
    base: Arc<Vec<f32>>,
    /// Last composite actually written to the map. Runtime-only: detects unlayered writers
    /// even after a caller changes layer settings before asking for a recompute.
    published: Arc<Vec<f32>>,
    /// Cumulative unlayered base displacement, independent of layer-operation undo snapshots.
    /// Sparse and runtime-only; the resulting base heights already live in the sidecar.
    base_changes: Arc<BTreeMap<usize, f64>>,
    /// Base material weights, sparse. A missing sample is channel 0 at weight 1. An explicit
    /// all-zero sample is stored and falls back to channel 0 when the folded sum is 0.
    base_weights: Arc<BTreeMap<usize, [f32; MAX_LAYERS]>>,
    layers: Vec<EditLayer>,
    /// The layer strokes write into; `None` writes the base.
    active: Option<u32>,
    next_id: u32,
    /// Samples recomputed since creation (toggles, reorders, strokes).
    recomputed: u64,
    /// Changed since the last save.
    unsaved: bool,
}

/// The cook tiles (edit cells) a rectangle covers.
fn cells_of(rect: DirtyRect) -> BTreeSet<TileCoord> {
    let mut cells = BTreeSet::new();
    if rect.is_empty() {
        return cells;
    }
    for y in rect.row_start / EDIT_CELL_SIZE..=(rect.row_end - 1) / EDIT_CELL_SIZE {
        for x in rect.col_start / EDIT_CELL_SIZE..=(rect.col_end - 1) / EDIT_CELL_SIZE {
            cells.insert((x, y));
        }
    }
    cells
}

fn union(a: Option<DirtyRect>, b: Option<DirtyRect>) -> Option<DirtyRect> {
    match (a, b) {
        (Some(a), Some(b)) => Some(crate::brush::union_rect(a, b)),
        (a, None) => a,
        (None, b) => b,
    }
}

fn point_rect(col: u32, row: u32) -> DirtyRect {
    DirtyRect {
        col_start: col,
        col_end: col + 1,
        row_start: row,
        row_end: row + 1,
    }
}

fn valid_name(name: &str) -> bool {
    let n = name.chars().count();
    (1..=MAX_EDIT_LAYER_NAME).contains(&n) && !name.chars().any(char::is_control) && !name.trim().is_empty()
}

impl TerrainEditLayers {
    /// An empty stack over `map`: the base is the map's current heights, writes go to the base.
    #[must_use]
    pub fn new(map: &TerrainMap) -> Self {
        let base = Arc::new(map.heights().to_vec());
        Self {
            width: map.width(),
            height: map.height(),
            published: base.clone(),
            base,
            base_changes: Arc::new(BTreeMap::new()),
            base_weights: Arc::new(BTreeMap::new()),
            layers: Vec::new(),
            active: None,
            next_id: 1,
            recomputed: 0,
            unsaved: false,
        }
    }

    /// `[columns, rows]` of the grid.
    #[must_use]
    pub fn size(&self) -> [u32; 2] {
        [self.width, self.height]
    }

    /// The base heights, row-major.
    #[must_use]
    pub fn base(&self) -> &[f32] {
        &self.base
    }

    /// The layers, bottom first.
    #[must_use]
    pub fn layers(&self) -> &[EditLayer] {
        &self.layers
    }

    /// The layer with `id`.
    #[must_use]
    pub fn layer(&self, id: u32) -> Option<&EditLayer> {
        self.layers.iter().find(|l| l.id == id)
    }

    /// The stack position of the layer with `id`.
    #[must_use]
    pub fn index_of(&self, id: u32) -> Option<usize> {
        self.layers.iter().position(|l| l.id == id)
    }

    /// The first layer named `name`.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&EditLayer> {
        self.layers.iter().find(|l| l.name == name)
    }

    /// The layer strokes write into (`None`: the base).
    #[must_use]
    pub fn active(&self) -> Option<u32> {
        self.active
    }

    /// Samples recomputed since the stack was made: a toggle of a small layer costs its bounds,
    /// not the map.
    #[must_use]
    pub fn recomputed_samples(&self) -> u64 {
        self.recomputed
    }

    /// Changed since the last [`Self::mark_saved`].
    #[must_use]
    pub fn unsaved(&self) -> bool {
        self.unsaved
    }

    /// Clears [`Self::unsaved`].
    pub fn mark_saved(&mut self) {
        self.unsaved = false;
    }

    /// Marks the stack changed (an undo step restored it).
    pub fn mark_unsaved(&mut self) {
        self.unsaved = true;
    }

    fn check_map(&self, map: &TerrainMap) -> Result<(), EditLayerError> {
        if map.width() != self.width || map.height() != self.height {
            return Err(EditLayerError::SizeMismatch {
                layers: [self.width, self.height],
                map: [map.width(), map.height()],
            });
        }
        Ok(())
    }

    fn samples(&self) -> usize {
        self.width as usize * self.height as usize
    }

    fn whole_map(&self) -> DirtyRect {
        DirtyRect {
            col_start: 0,
            col_end: self.width,
            row_start: 0,
            row_end: self.height,
        }
    }

    /// Absorbs unlayered map changes over `rect` into the base; returns their bounds.
    ///
    /// The displacement is measured against the last published composite, not the current
    /// layer settings. This is also safe after changing a layer's visibility or opacity.
    /// Call [`Self::recompute`] afterward to publish the new fold (a Set layer may cover the
    /// base). `recompute` already performs this absorption for its own rectangle. Reconcile
    /// the whole map before calling [`Self::flatten_into_base`] or [`Self::to_bytes`] directly.
    /// World-level operations and editor saves do that automatically (ADR-8202).
    ///
    /// # Errors
    ///
    /// [`EditLayerError::SizeMismatch`] when the map has a different grid.
    pub fn absorb_base_changes(
        &mut self,
        map: &TerrainMap,
        rect: DirtyRect,
    ) -> Result<Option<DirtyRect>, EditLayerError> {
        let _span = tracing::info_span!(target: "regia_terrain", "terrain.edit_layers.absorb_base").entered();
        self.check_map(map)?;
        let mut changed = None;
        for r in rect.row_start.min(self.height)..rect.row_end.min(self.height) {
            for c in rect.col_start.min(self.width)..rect.col_end.min(self.width) {
                let i = r as usize * self.width as usize + c as usize;
                if self.absorb_base_sample(i, map.heights()[i]) {
                    changed = Some(DirtyRect::include(changed, c, r));
                }
            }
        }
        Ok(changed)
    }

    fn absorb_base_sample(&mut self, i: usize, observed: f32) -> bool {
        if observed.to_bits() == self.published[i].to_bits() {
            return false;
        }
        let before = self.base[i];
        let after = (f64::from(before) + f64::from(observed) - f64::from(self.published[i])) as f32;
        Arc::make_mut(&mut self.base)[i] = after;
        *Arc::make_mut(&mut self.base_changes).entry(i).or_default() += f64::from(after) - f64::from(before);
        Arc::make_mut(&mut self.published)[i] = observed;
        self.unsaved = true;
        true
    }

    /// Carries later unlayered changes into an older undo snapshot without restoring the old
    /// composite snapshot over the live map. Named-layer/base strokes retain their own undo.
    fn inherit_base_changes(&mut self, live: &Self) {
        let indices: BTreeSet<usize> = self
            .base_changes
            .keys()
            .chain(live.base_changes.keys())
            .copied()
            .collect();
        for i in indices {
            let old = self.base_changes.get(&i).copied().unwrap_or_default();
            let new = live.base_changes.get(&i).copied().unwrap_or_default();
            if old != new {
                Arc::make_mut(&mut self.base)[i] = (f64::from(self.base[i]) + new - old) as f32;
            }
        }
        self.base_changes = live.base_changes.clone();
        self.published = live.published.clone();
    }

    /// Selects the layer strokes write into (`None`: the base).
    ///
    /// # Errors
    ///
    /// [`EditLayerError::NotFound`] for an unknown id.
    pub fn set_active(&mut self, id: Option<u32>) -> Result<(), EditLayerError> {
        if let Some(id) = id
            && self.layer(id).is_none()
        {
            return Err(EditLayerError::NotFound(id));
        }
        self.active = id;
        Ok(())
    }

    /// Adds an empty layer on top and makes it active; returns its id.
    ///
    /// # Errors
    ///
    /// [`EditLayerError::BadName`], [`EditLayerError::Limit`].
    pub fn create(&mut self, name: &str, blend: EditLayerBlend) -> Result<u32, EditLayerError> {
        if !valid_name(name) {
            return Err(EditLayerError::BadName);
        }
        if self.layers.len() >= MAX_EDIT_LAYERS {
            return Err(EditLayerError::Limit);
        }
        let id = self.next_id;
        self.next_id += 1;
        let n = self.samples();
        self.layers.push(EditLayer {
            id,
            name: name.to_owned(),
            enabled: true,
            locked: false,
            opacity: 1.0,
            blend,
            values: Arc::new(vec![0.0; n]),
            coverage: Arc::new(match blend {
                EditLayerBlend::Add => Vec::new(),
                EditLayerBlend::Set => vec![0.0; n],
            }),
            weights: Arc::new(BTreeMap::new()),
            procgen: None,
            bounds: None,
        });
        self.active = Some(id);
        self.unsaved = true;
        Ok(id)
    }

    /// Changes a layer's settings; returns the region to recompute (`None`: the fold is unchanged).
    ///
    /// # Errors
    ///
    /// [`EditLayerError::NotFound`], [`EditLayerError::BadName`], [`EditLayerError::BadOpacity`].
    pub fn set(&mut self, id: u32, patch: &EditLayerPatch) -> Result<Option<DirtyRect>, EditLayerError> {
        if let Some(name) = &patch.name
            && !valid_name(name)
        {
            return Err(EditLayerError::BadName);
        }
        if let Some(o) = patch.opacity
            && !(o.is_finite() && (0.0..=1.0).contains(&o))
        {
            return Err(EditLayerError::BadOpacity);
        }
        let i = self.index_of(id).ok_or(EditLayerError::NotFound(id))?;
        let layer = &mut self.layers[i];
        let mut visual = false;
        if let Some(name) = &patch.name {
            layer.name.clone_from(name);
        }
        if let Some(locked) = patch.locked {
            layer.locked = locked;
        }
        if let Some(enabled) = patch.enabled {
            visual |= layer.enabled != enabled;
            layer.enabled = enabled;
        }
        if let Some(opacity) = patch.opacity {
            visual |= layer.enabled && layer.opacity.to_bits() != opacity.to_bits();
            layer.opacity = opacity;
        }
        self.unsaved = true;
        Ok(if visual { layer.bounds } else { None })
    }

    /// Removes a layer; returns the region to recompute.
    ///
    /// # Errors
    ///
    /// [`EditLayerError::NotFound`], [`EditLayerError::Locked`].
    pub fn delete(&mut self, id: u32) -> Result<Option<DirtyRect>, EditLayerError> {
        let i = self.index_of(id).ok_or(EditLayerError::NotFound(id))?;
        if self.layers[i].locked {
            return Err(EditLayerError::Locked(self.layers[i].name.clone()));
        }
        let layer = self.layers.remove(i);
        if self.active == Some(id) {
            self.active = self.layers.last().map(|l| l.id);
        }
        self.unsaved = true;
        Ok(if layer.enabled { layer.bounds } else { None })
    }

    /// Moves a layer to stack position `index` (clamped; 0 is the bottom); returns the region to
    /// recompute. Only the moved layer's samples can change.
    ///
    /// # Errors
    ///
    /// [`EditLayerError::NotFound`].
    pub fn reorder(&mut self, id: u32, index: usize) -> Result<Option<DirtyRect>, EditLayerError> {
        let i = self.index_of(id).ok_or(EditLayerError::NotFound(id))?;
        let to = index.min(self.layers.len() - 1);
        if i == to {
            return Ok(None);
        }
        let layer = self.layers.remove(i);
        let region = if layer.enabled { layer.bounds } else { None };
        self.layers.insert(to, layer);
        self.unsaved = true;
        Ok(region)
    }

    /// Bakes every enabled layer into the base and removes them (off layers stay). The fold, and so
    /// the map, does not change by a bit.
    ///
    /// # Errors
    ///
    /// [`EditLayerError::Locked`] when an enabled layer is locked.
    pub fn flatten_into_base(&mut self) -> Result<usize, EditLayerError> {
        if let Some(l) = self.layers.iter().find(|l| l.enabled && l.locked) {
            return Err(EditLayerError::Locked(l.name.clone()));
        }
        let region = self
            .layers
            .iter()
            .filter(|l| l.enabled)
            .fold(None, |acc, l| union(acc, l.bounds));
        let merged = self.layers.iter().filter(|l| l.enabled).count();
        if let Some(rect) = region {
            let plan = self.plan();
            let mut base = (*self.base).clone();
            for r in rect.row_start..rect.row_end {
                for c in rect.col_start..rect.col_end {
                    let i = r as usize * self.width as usize + c as usize;
                    base[i] = self.fold(i, &plan);
                }
            }
            self.base = Arc::new(base);
        }
        self.bake_enabled_weights();
        self.layers.retain(|l| !l.enabled);
        if self.active.is_some_and(|id| self.layer(id).is_none()) {
            self.active = None;
        }
        if merged > 0 {
            self.unsaved = true;
        }
        Ok(merged)
    }

    /// Bakes enabled layers' weight deltas into the base so removing those layers does not change
    /// the folded weights. Disabled layers are left alone (they do not contribute).
    fn bake_enabled_weights(&mut self) {
        let any = self
            .layers
            .iter()
            .any(|layer| layer.enabled && !layer.weights.is_empty());
        if !any {
            return;
        }
        let n = self.samples();
        let mut indexes: BTreeSet<usize> = self.base_weights.keys().copied().collect();
        for layer in &self.layers {
            if layer.enabled {
                indexes.extend(layer.weights.keys().copied());
            }
        }
        let mut next = BTreeMap::new();
        for index in indexes {
            if index >= n {
                continue;
            }
            let folded = self.folded_weights(index);
            if !is_default_base(&folded) {
                next.insert(index, folded);
            }
        }
        self.base_weights = Arc::new(next);
    }

    fn plan(&self) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut run: Vec<usize> = Vec::new();
        for (i, l) in self.layers.iter().enumerate() {
            if !l.enabled {
                continue;
            }
            match l.blend {
                EditLayerBlend::Add => run.push(i),
                EditLayerBlend::Set => {
                    if !run.is_empty() {
                        run.sort_by_key(|&j| self.layers[j].id);
                        steps.push(Step::Add(std::mem::take(&mut run)));
                    }
                    steps.push(Step::Set(i));
                }
            }
        }
        if !run.is_empty() {
            run.sort_by_key(|&j| self.layers[j].id);
            steps.push(Step::Add(run));
        }
        steps
    }

    fn fold_from(&self, mut h: f32, index: usize, steps: &[Step]) -> f32 {
        for step in steps {
            match step {
                Step::Add(run) => {
                    let mut sum = 0.0f64;
                    let mut any = false;
                    for &j in run {
                        let l = &self.layers[j];
                        let d = l.values[index];
                        if d != 0.0 && l.opacity > 0.0 {
                            sum += f64::from(d) * f64::from(l.opacity);
                            any = true;
                        }
                    }
                    if any {
                        h = (f64::from(h) + sum) as f32;
                    }
                }
                Step::Set(j) => {
                    let l = &self.layers[*j];
                    let t = l.coverage[index] * l.opacity;
                    if t >= 1.0 {
                        h = l.values[index];
                    } else if t > 0.0 {
                        h += (l.values[index] - h) * t;
                    }
                }
            }
        }
        h
    }

    fn fold(&self, index: usize, steps: &[Step]) -> f32 {
        self.fold_from(self.base[index], index, steps)
    }

    /// The folded height at sample `index` (base plus every enabled layer).
    #[must_use]
    pub fn flattened_at(&self, index: usize) -> f32 {
        self.fold(index, &self.plan())
    }

    /// The whole folded heightfield: what the cook bakes.
    #[must_use]
    pub fn flatten(&self) -> Vec<f32> {
        let _span = tracing::info_span!(target: "regia_terrain", "terrain.edit_layers.flatten").entered();
        let plan = self.plan();
        (0..self.samples()).map(|i| self.fold(i, &plan)).collect()
    }

    /// Writes the fold over `rect` into `map`; returns the samples written.
    ///
    /// # Errors
    ///
    /// [`EditLayerError::SizeMismatch`].
    pub fn recompute(&mut self, map: &mut TerrainMap, rect: DirtyRect) -> Result<usize, EditLayerError> {
        let _span = tracing::info_span!(target: "regia_terrain", "terrain.edit_layers.recompute").entered();
        self.check_map(map)?;
        let rect = DirtyRect {
            col_start: rect.col_start.min(self.width),
            col_end: rect.col_end.min(self.width),
            row_start: rect.row_start.min(self.height),
            row_end: rect.row_end.min(self.height),
        };
        if rect.is_empty() {
            return Ok(0);
        }
        self.absorb_base_changes(map, rect)?;
        let plan = self.plan();
        let w = self.width as usize;
        for r in rect.row_start..rect.row_end {
            for c in rect.col_start..rect.col_end {
                let i = r as usize * w + c as usize;
                let h = self.fold(i, &plan);
                if map.heights()[i].to_bits() != h.to_bits() {
                    map.set_height(i, h);
                }
                Arc::make_mut(&mut self.published)[i] = h;
            }
        }
        let n = (rect.col_end - rect.col_start) as usize * (rect.row_end - rect.row_start) as usize;
        self.recomputed += n as u64;
        Ok(n)
    }

    /// [`Self::recompute`] on a terrain, recording the edit (GPU upload, re-cook, min/max).
    ///
    /// # Errors
    ///
    /// [`EditLayerError::SizeMismatch`].
    pub fn recompute_terrain(
        &mut self,
        terrain: &mut Terrain,
        edits: &mut TerrainEdits,
        rect: Option<DirtyRect>,
    ) -> Result<usize, EditLayerError> {
        let Some(rect) = rect else { return Ok(0) };
        let n = self.recompute(&mut terrain.map, rect)?;
        let terrain = &mut *terrain;
        edits.note(&terrain.map, &mut terrain.minmax, rect, &cells_of(rect));
        Ok(n)
    }

    /// Moves a height stroke that already landed on `map` into the active layer (or the base),
    /// rewrites its samples with the canonical fold, and fixes `record.after` to match, so the
    /// composite record and the returned layer record restore both bit for bit.
    ///
    /// # Errors
    ///
    /// [`EditLayerError::Locked`] / [`EditLayerError::Hidden`] for the active layer,
    /// [`EditLayerError::SizeMismatch`]. Nothing changes then (the caller reverts the stroke).
    pub fn absorb_stroke(
        &mut self,
        record: &mut StrokeRecord<f32>,
        map: &mut TerrainMap,
    ) -> Result<LayerStrokeUndo, EditLayerError> {
        self.absorb_stroke_into(self.active, record, map)
    }

    /// Whether a stroke may write into `layer` (`None`: the base); the stack position when it may.
    ///
    /// # Errors
    ///
    /// [`EditLayerError::NotFound`], [`EditLayerError::Owned`], [`EditLayerError::Locked`],
    /// [`EditLayerError::Hidden`].
    pub fn check_target(&self, layer: Option<u32>) -> Result<Option<usize>, EditLayerError> {
        let Some(id) = layer else { return Ok(None) };
        let i = self.index_of(id).ok_or(EditLayerError::NotFound(id))?;
        let l = &self.layers[i];
        if let Some(target) = &l.procgen {
            return Err(EditLayerError::Owned {
                name: l.name.clone(),
                graph: target.graph.clone(),
                output: target.output.clone(),
            });
        }
        if l.locked {
            return Err(EditLayerError::Locked(l.name.clone()));
        }
        if !l.enabled || l.opacity <= 0.0 {
            return Err(EditLayerError::Hidden(l.name.clone()));
        }
        Ok(Some(i))
    }

    /// The base weight at `index` before any layer. A missing sample is channel 0 at weight 1.
    #[must_use]
    pub fn base_weight(&self, index: usize) -> [f32; MAX_LAYERS] {
        self.base_weights
            .get(&index)
            .copied()
            .unwrap_or_else(default_base_weight)
    }

    /// Folded material weights at `index`: base plus each enabled layer's opacity-scaled signed
    /// delta, negative channels clamped to 0, then divided by the channel sum. A zero sum is
    /// channel 0 at weight 1 (the base material). The result sums to 1.
    #[must_use]
    pub fn folded_weights(&self, index: usize) -> [f32; MAX_LAYERS] {
        if index >= self.samples() {
            return default_base_weight();
        }
        let mut w = self.base_weight(index);
        for layer in &self.layers {
            if !layer.enabled || layer.opacity <= 0.0 {
                continue;
            }
            let Some(delta) = layer.weights.get(&index) else {
                continue;
            };
            let opacity = layer.opacity;
            for (channel, delta) in w.iter_mut().zip(delta) {
                *channel += opacity * *delta;
            }
        }
        for channel in &mut w {
            if *channel < 0.0 {
                *channel = 0.0;
            }
        }
        let sum: f32 = w.iter().sum();
        if sum == 0.0 {
            return default_base_weight();
        }
        if sum.to_bits() != 1.0f32.to_bits() {
            for channel in &mut w {
                *channel /= sum;
            }
        }
        w
    }

    /// Sets the base weights (`layer` is `None`) or one layer's signed weight delta at `index`.
    /// A short `channels` slice is padded with 0. The key is dropped when it returns to the
    /// default (base: channel 0 = 1; layer: all zero). Returns the folded weights.
    ///
    /// # Errors
    ///
    /// [`EditLayerError::Owned`], [`EditLayerError::Locked`], [`EditLayerError::Hidden`],
    /// [`EditLayerError::NotFound`], [`EditLayerError::BadWeight`], [`EditLayerError::File`]
    /// when `index` is outside the grid.
    pub fn paint_weights(
        &mut self,
        layer: Option<u32>,
        index: usize,
        channels: &[f32],
    ) -> Result<[f32; MAX_LAYERS], EditLayerError> {
        if index >= self.samples() {
            return Err(EditLayerError::File(format!(
                "sample {index} is outside the grid"
            )));
        }
        if channels.len() > MAX_LAYERS || channels.iter().any(|c| !c.is_finite()) {
            return Err(EditLayerError::BadWeight);
        }
        let target = self.check_target(layer)?;
        let mut w = [0.0; MAX_LAYERS];
        for (slot, value) in w.iter_mut().zip(channels) {
            *slot = *value;
        }
        match target {
            None => {
                let map = Arc::make_mut(&mut self.base_weights);
                if is_default_base(&w) {
                    map.remove(&index);
                } else {
                    map.insert(index, w);
                }
            }
            Some(t) => {
                let map = Arc::make_mut(&mut self.layers[t].weights);
                if is_zero_weight(&w) {
                    map.remove(&index);
                } else {
                    map.insert(index, w);
                }
            }
        }
        self.unsaved = true;
        Ok(self.folded_weights(index))
    }

    /// Attaches or detaches the procgen target of layer `id`. `None` detaches. Detach is an
    /// ordinary stack change, so undo puts the target back.
    ///
    /// # Errors
    ///
    /// [`EditLayerError::NotFound`], [`EditLayerError::BadTarget`].
    pub fn set_procgen_target(
        &mut self,
        id: u32,
        target: Option<ProcgenLayerTarget>,
    ) -> Result<(), EditLayerError> {
        if let Some(target) = &target {
            validate_procgen_target(target)?;
        }
        let i = self.index_of(id).ok_or(EditLayerError::NotFound(id))?;
        self.layers[i].procgen = target;
        self.unsaved = true;
        Ok(())
    }

    /// The topmost layer whose procgen target is `graph` + `output`.
    #[must_use]
    pub fn layer_for_procgen(&self, graph: &str, output: &str) -> Option<u32> {
        self.layers.iter().rev().find_map(|layer| {
            layer
                .procgen
                .as_ref()
                .and_then(|target| (target.graph == graph && target.output == output).then_some(layer.id))
        })
    }

    /// The layer's value and coverage at `index`.
    #[must_use]
    pub fn layer_sample(&self, id: u32, index: usize) -> Option<(f32, f32)> {
        let t = self.index_of(id)?;
        if index >= self.samples() {
            return None;
        }
        Some(self.read(Some(t), index))
    }

    /// Writes one layer sample without the brush ownership check. Procgen publish and restore use
    /// this; a brush uses [`Self::absorb_stroke`].
    ///
    /// # Errors
    ///
    /// [`EditLayerError::NotFound`], [`EditLayerError::File`] for an index outside the grid or a
    /// non-finite value.
    pub fn set_layer_sample(
        &mut self,
        id: u32,
        index: usize,
        value: f32,
        coverage: f32,
    ) -> Result<(), EditLayerError> {
        let t = self.index_of(id).ok_or(EditLayerError::NotFound(id))?;
        if index >= self.samples() || !value.is_finite() || !coverage.is_finite() {
            return Err(EditLayerError::File(format!(
                "sample {index} is outside the grid or not finite"
            )));
        }
        self.write(Some(t), index, value, coverage);
        let col = (index as u32) % self.width;
        let row = (index as u32) / self.width;
        let layer = &mut self.layers[t];
        layer.bounds = union(layer.bounds, Some(point_rect(col, row)));
        self.unsaved = true;
        Ok(())
    }

    /// Makes the composed height at `index` equal `wanted_top` by changing only layer `id`.
    /// The map is not written; call [`Self::recompute`] on the touched rectangle afterward.
    /// An owned layer is writable here: this is the owning graph's publish path.
    ///
    /// # Errors
    ///
    /// [`EditLayerError::NotFound`], [`EditLayerError::Hidden`] when the layer cannot show a
    /// change, [`EditLayerError::File`] for a bad index or a non-finite result.
    pub fn compose_into_layer(
        &mut self,
        id: u32,
        index: usize,
        wanted_top: f32,
    ) -> Result<LayerSampleWrite, EditLayerError> {
        if index >= self.samples() || !wanted_top.is_finite() {
            return Err(EditLayerError::File(format!(
                "sample {index} is outside the grid or not finite"
            )));
        }
        let t = self.index_of(id).ok_or(EditLayerError::NotFound(id))?;
        if !self.layers[t].enabled || self.layers[t].opacity <= 0.0 {
            return Err(EditLayerError::Hidden(self.layers[t].name.clone()));
        }
        let below = self.fold_sequential(index, 0..t);
        let at_level = self.apply_layer(t, below, index);
        let top = self.flattened_at(index);
        let wanted = wanted_top - (top - at_level);
        let (before_v, before_c) = self.read(Some(t), index);
        let (after_v, after_c): (f32, f32) = {
            let layer = &self.layers[t];
            match layer.blend {
                EditLayerBlend::Add => (before_v + (wanted - at_level) / layer.opacity, 0.0),
                EditLayerBlend::Set if layer.opacity >= 1.0 => (wanted, 1.0),
                EditLayerBlend::Set => (below + (wanted - below) / layer.opacity, 1.0),
            }
        };
        if !after_v.is_finite() || !after_c.is_finite() {
            return Err(EditLayerError::File(format!(
                "sample {index} did not invert to a finite layer value"
            )));
        }
        self.write(Some(t), index, after_v, after_c);
        let col = (index as u32) % self.width;
        let row = (index as u32) / self.width;
        let layer = &mut self.layers[t];
        layer.bounds = union(layer.bounds, Some(point_rect(col, row)));
        self.unsaved = true;
        Ok(LayerSampleWrite {
            index: index as u32,
            before_value: before_v,
            before_coverage: before_c,
            after_value: after_v,
            after_coverage: after_c,
        })
    }

    /// [`Self::absorb_stroke`] into `layer` (`None`: the base) instead of the active layer.
    ///
    /// # Errors
    ///
    /// As [`Self::absorb_stroke`].
    pub fn absorb_stroke_into(
        &mut self,
        layer: Option<u32>,
        record: &mut StrokeRecord<f32>,
        map: &mut TerrainMap,
    ) -> Result<LayerStrokeUndo, EditLayerError> {
        let _span = tracing::info_span!(target: "regia_terrain", "terrain.edit_layers.absorb").entered();
        self.check_map(map)?;
        let target = self.check_target(layer)?;
        // The live preview has already changed the map. Only the stroke's *before* value
        // can reveal an earlier unlayered edit; its after value belongs to this stroke.
        for s in &record.samples {
            let (c, r) = (s.position.x, s.position.y);
            if c >= 0 && r >= 0 && (c as u32) < self.width && (r as u32) < self.height {
                self.absorb_base_sample(r as usize * self.width as usize + c as usize, s.before);
            }
        }
        let plan = self.plan();
        // Steps below and above the target, for the inversion.
        let w = self.width as usize;
        let mut changes = Vec::with_capacity(record.samples.len());
        for s in &record.samples {
            let (c, r) = (s.position.x, s.position.y);
            if c < 0 || r < 0 || c as u32 >= self.width || r as u32 >= self.height {
                continue;
            }
            let i = r as usize * w + c as usize;
            // The height at the target's level now, and what the layers above add to it.
            let (below, at_level) = match target {
                None => (self.base[i], self.base[i]),
                Some(t) => {
                    let below = self.fold_sequential(i, 0..t);
                    (below, self.apply_layer(t, below, i))
                }
            };
            let top = self.fold(i, &plan);
            let offset = top - at_level;
            let wanted = s.after - offset;
            let (before_v, before_c) = self.read(target, i);
            let (after_v, after_c) = match target {
                None => (wanted, 1.0),
                Some(t) => {
                    let l = &self.layers[t];
                    match l.blend {
                        EditLayerBlend::Add => (before_v + (wanted - at_level) / l.opacity, 0.0),
                        EditLayerBlend::Set if l.opacity >= 1.0 => (wanted, 1.0),
                        EditLayerBlend::Set => (below + (wanted - below) / l.opacity, 1.0),
                    }
                }
            };
            self.write(target, i, after_v, after_c);
            changes.push(LayerSampleChange {
                index: i as u32,
                before: [before_v, before_c],
                after: [after_v, after_c],
                composite: [s.before, s.after],
                absorbed_base: self.base_changes.get(&i).copied().unwrap_or_default(),
            });
            if let Some(t) = target {
                let l = &mut self.layers[t];
                l.bounds = union(l.bounds, Some(point_rect(c as u32, r as u32)));
            }
        }
        // The canonical fold of every touched sample, into the map and the composite record.
        let plan = self.plan();
        for s in &mut record.samples {
            let (c, r) = (s.position.x, s.position.y);
            if c < 0 || r < 0 || c as u32 >= self.width || r as u32 >= self.height {
                continue;
            }
            let i = r as usize * w + c as usize;
            let h = self.fold(i, &plan);
            map.set_height(i, h);
            Arc::make_mut(&mut self.published)[i] = h;
            s.after = h;
        }
        for change in &mut changes {
            change.composite[1] = self.published[change.index as usize];
        }
        self.recomputed += record.samples.len() as u64;
        self.unsaved = true;
        Ok(LayerStrokeUndo {
            layer: target.map(|t| self.layers[t].id),
            changes,
        })
    }

    fn fold_sequential(&self, i: usize, range: std::ops::Range<usize>) -> f32 {
        let mut h = self.base[i];
        for t in range {
            if self.layers[t].enabled {
                h = self.apply_layer(t, h, i);
            }
        }
        h
    }

    fn apply_layer(&self, t: usize, h: f32, i: usize) -> f32 {
        let l = &self.layers[t];
        match l.blend {
            EditLayerBlend::Add => h + l.values[i] * l.opacity,
            EditLayerBlend::Set => {
                let k = l.coverage[i] * l.opacity;
                if k >= 1.0 {
                    l.values[i]
                } else {
                    h + (l.values[i] - h) * k
                }
            }
        }
    }

    fn read(&self, target: Option<usize>, i: usize) -> (f32, f32) {
        match target {
            None => (self.base[i], 1.0),
            Some(t) => {
                let l = &self.layers[t];
                (l.values[i], l.coverage.get(i).copied().unwrap_or(0.0))
            }
        }
    }

    fn write(&mut self, target: Option<usize>, i: usize, value: f32, coverage: f32) {
        match target {
            None => Arc::make_mut(&mut self.base)[i] = value,
            Some(t) => {
                let l = &mut self.layers[t];
                Arc::make_mut(&mut l.values)[i] = value;
                if l.blend == EditLayerBlend::Set {
                    Arc::make_mut(&mut l.coverage)[i] = coverage;
                }
            }
        }
    }

    /// Encodes the stack as a `<map>.terrain.editlayers` file.
    ///
    /// # Errors
    ///
    /// The serializer failed.
    pub fn to_bytes(&self) -> Result<Vec<u8>, EditLayerError> {
        let chunked = |values: &[f32], keep: &dyn Fn(usize) -> bool| -> Vec<EditLayerChunk> {
            let mut out = Vec::new();
            for (cx, cy, rect) in self.chunks() {
                let mut any = false;
                let mut vals = Vec::with_capacity((rect.col_end - rect.col_start) as usize * 64);
                for r in rect.row_start..rect.row_end {
                    for c in rect.col_start..rect.col_end {
                        let i = r as usize * self.width as usize + c as usize;
                        any |= keep(i);
                        vals.push(values[i]);
                    }
                }
                if any {
                    out.push(EditLayerChunk {
                        x: cx,
                        y: cy,
                        values: vals,
                    });
                }
            }
            out
        };
        let covered = |i: usize| self.layers.iter().any(|l| l.touches(i));
        let (width, height) = (self.width, self.height);
        let file = EditLayersFile {
            version: EDIT_LAYERS_FORMAT_VERSION,
            width,
            height,
            chunk: EDIT_LAYERS_CHUNK,
            active: self.active,
            next_id: self.next_id,
            base: chunked(&self.base, &covered),
            base_weights: chunk_weights(width, height, &self.base_weights, default_base_weight()),
            layers: self
                .layers
                .iter()
                .map(|l| EditLayerFile {
                    id: l.id,
                    name: l.name.clone(),
                    enabled: l.enabled,
                    locked: l.locked,
                    opacity: l.opacity,
                    blend: l.blend,
                    values: chunked(&l.values, &|i| l.touches(i)),
                    coverage: if l.blend == EditLayerBlend::Set {
                        chunked(&l.coverage, &|i| l.touches(i))
                    } else {
                        Vec::new()
                    },
                    weights: chunk_weights(width, height, &l.weights, [0.0; MAX_LAYERS]),
                    procgen: l.procgen.clone(),
                })
                .collect(),
        };
        postcard::to_allocvec(&file).map_err(|e| EditLayerError::File(e.to_string()))
    }

    fn chunks(&self) -> impl Iterator<Item = (u32, u32, DirtyRect)> + '_ {
        let (cw, ch) = (
            self.width.div_ceil(EDIT_LAYERS_CHUNK),
            self.height.div_ceil(EDIT_LAYERS_CHUNK),
        );
        (0..ch).flat_map(move |cy| {
            (0..cw).map(move |cx| {
                (
                    cx,
                    cy,
                    DirtyRect {
                        col_start: cx * EDIT_LAYERS_CHUNK,
                        col_end: ((cx + 1) * EDIT_LAYERS_CHUNK).min(self.width),
                        row_start: cy * EDIT_LAYERS_CHUNK,
                        row_end: ((cy + 1) * EDIT_LAYERS_CHUNK).min(self.height),
                    },
                )
            })
        })
    }

    /// Decodes a `<map>.terrain.editlayers` file over `map` (the cooked, flattened heights) and
    /// checks that its fold reproduces `map` where any layer covers it.
    ///
    /// # Errors
    ///
    /// [`EditLayerError::Version`] for a newer file, [`EditLayerError::File`] for a malformed one,
    /// [`EditLayerError::SizeMismatch`] for another grid, [`EditLayerError::Stale`] when the map was
    /// changed outside the layers since the file was written.
    pub fn from_bytes(bytes: &[u8], map: &TerrainMap) -> Result<Self, EditLayerError> {
        let bad = EditLayerError::File;
        let (version, _) = postcard::take_from_bytes::<u32>(bytes).map_err(|e| bad(e.to_string()))?;
        if version != EDIT_LAYERS_V1 && version != EDIT_LAYERS_FORMAT_VERSION {
            return Err(EditLayerError::Version(version));
        }
        // The grid comes before any array: refuse a hostile size before allocating it.
        let (_, rest) = postcard::take_from_bytes::<u32>(bytes).map_err(|e| bad(e.to_string()))?;
        let (width, rest) = postcard::take_from_bytes::<u32>(rest).map_err(|e| bad(e.to_string()))?;
        let (height, _) = postcard::take_from_bytes::<u32>(rest).map_err(|e| bad(e.to_string()))?;
        if width < 2 || height < 2 || u64::from(width) * u64::from(height) > MAX_EDIT_LAYER_SAMPLES {
            return Err(bad(format!("grid {width} x {height} is out of range")));
        }
        if [width, height] != [map.width(), map.height()] {
            return Err(EditLayerError::SizeMismatch {
                layers: [width, height],
                map: [map.width(), map.height()],
            });
        }
        let file = decode_edit_layers_file(bytes)?;
        if file.chunk != EDIT_LAYERS_CHUNK {
            return Err(bad(format!(
                "chunk {} (expected {EDIT_LAYERS_CHUNK})",
                file.chunk
            )));
        }
        if file.layers.len() > MAX_EDIT_LAYERS {
            return Err(EditLayerError::Limit);
        }
        let mut stack = Self::new(map);
        stack.next_id = file.next_id.max(1);
        let n = stack.samples();
        let mut seen = BTreeSet::new();
        for lf in &file.layers {
            if !valid_name(&lf.name) {
                return Err(EditLayerError::BadName);
            }
            if !(lf.opacity.is_finite() && (0.0..=1.0).contains(&lf.opacity)) {
                return Err(EditLayerError::BadOpacity);
            }
            if lf.id == 0 || lf.id >= stack.next_id || !seen.insert(lf.id) {
                return Err(bad(format!("layer id {} is duplicated or past next_id", lf.id)));
            }
            let mut values = vec![0.0; n];
            let b1 = stack.unchunk(&lf.values, &mut values, |_| true)?;
            let (coverage, b2) = match lf.blend {
                EditLayerBlend::Add => {
                    if !lf.coverage.is_empty() {
                        return Err(bad("an add layer has coverage".into()));
                    }
                    (Vec::new(), None)
                }
                EditLayerBlend::Set => {
                    let mut cov = vec![0.0; n];
                    let b = stack.unchunk(&lf.coverage, &mut cov, |v| (0.0..=1.0).contains(&v))?;
                    (cov, b)
                }
            };
            if let Some(target) = &lf.procgen {
                validate_procgen_target(target)?;
            }
            let mut weights = BTreeMap::new();
            stack.unchunk_weights(&lf.weights, &mut weights, |w| !is_zero_weight(w))?;
            stack.layers.push(EditLayer {
                id: lf.id,
                name: lf.name.clone(),
                enabled: lf.enabled,
                locked: lf.locked,
                opacity: lf.opacity,
                blend: lf.blend,
                values: Arc::new(values),
                coverage: Arc::new(coverage),
                weights: Arc::new(weights),
                procgen: lf.procgen.clone(),
                bounds: union(b1, b2),
            });
        }
        if let Some(id) = file.active
            && stack.layer(id).is_none()
        {
            return Err(bad(format!("active layer {id} does not exist")));
        }
        stack.active = file.active;
        let mut base = (*stack.base).clone();
        let covered = stack.unchunk(&file.base, &mut base, |_| true)?;
        stack.base = Arc::new(base);
        let mut base_weights = BTreeMap::new();
        stack.unchunk_weights(&file.base_weights, &mut base_weights, |w| !is_default_base(w))?;
        stack.base_weights = Arc::new(base_weights);
        // Where a layer covers the map, the fold must give the cooked heights back.
        let region = stack.layers.iter().fold(covered, |acc, l| union(acc, l.bounds));
        if let Some(rect) = region {
            let plan = stack.plan();
            let mapping = crate::runtime_patch::PatchTarget::of(map);
            let mut differ = 0;
            for r in rect.row_start..rect.row_end {
                for c in rect.col_start..rect.col_end {
                    let i = r as usize * width as usize + c as usize;
                    let authored = stack.fold(i, &plan);
                    let observed = map.heights()[i].to_bits();
                    // The live editor keeps f32 heights; a cook/reload uses the exact u16
                    // mapping. Neither representation is an intervening author edit.
                    if !authored.is_finite()
                        || (authored.to_bits() != observed
                            && mapping.dequantize(mapping.quantize(authored)).to_bits() != observed)
                    {
                        differ += 1;
                    }
                }
            }
            if differ > 0 {
                return Err(EditLayerError::Stale(differ));
            }
        }
        Ok(stack)
    }

    /// Copies chunks into `out`; returns the bounds of the samples that are not the default.
    fn unchunk(
        &self,
        chunks: &[EditLayerChunk],
        out: &mut [f32],
        valid: impl Fn(f32) -> bool,
    ) -> Result<Option<DirtyRect>, EditLayerError> {
        let (cw, ch) = (
            self.width.div_ceil(EDIT_LAYERS_CHUNK),
            self.height.div_ceil(EDIT_LAYERS_CHUNK),
        );
        let mut bounds = None;
        let mut seen = BTreeSet::new();
        for chunk in chunks {
            if chunk.x >= cw || chunk.y >= ch || !seen.insert((chunk.x, chunk.y)) {
                return Err(EditLayerError::File(format!(
                    "chunk ({}, {}) is outside the grid or repeated",
                    chunk.x, chunk.y
                )));
            }
            let rect = DirtyRect {
                col_start: chunk.x * EDIT_LAYERS_CHUNK,
                col_end: ((chunk.x + 1) * EDIT_LAYERS_CHUNK).min(self.width),
                row_start: chunk.y * EDIT_LAYERS_CHUNK,
                row_end: ((chunk.y + 1) * EDIT_LAYERS_CHUNK).min(self.height),
            };
            let cols = (rect.col_end - rect.col_start) as usize;
            if chunk.values.len() != cols * (rect.row_end - rect.row_start) as usize {
                return Err(EditLayerError::File(format!(
                    "chunk ({}, {}) has {} values",
                    chunk.x,
                    chunk.y,
                    chunk.values.len()
                )));
            }
            if chunk.values.iter().any(|v| !v.is_finite() || !valid(*v)) {
                return Err(EditLayerError::File(format!(
                    "chunk ({}, {}) has a value out of range",
                    chunk.x, chunk.y
                )));
            }
            for (k, v) in chunk.values.iter().enumerate() {
                let (c, r) = (
                    rect.col_start as usize + k % cols,
                    rect.row_start as usize + k / cols,
                );
                out[r * self.width as usize + c] = *v;
            }
            bounds = union(bounds, Some(rect));
        }
        Ok(bounds)
    }

    fn unchunk_weights(
        &self,
        chunks: &[EditLayerWeightChunk],
        into: &mut BTreeMap<usize, [f32; MAX_LAYERS]>,
        keep: impl Fn(&[f32; MAX_LAYERS]) -> bool,
    ) -> Result<(), EditLayerError> {
        let (cw, ch) = (
            self.width.div_ceil(EDIT_LAYERS_CHUNK),
            self.height.div_ceil(EDIT_LAYERS_CHUNK),
        );
        let mut seen = BTreeSet::new();
        for chunk in chunks {
            if chunk.x >= cw || chunk.y >= ch || !seen.insert((chunk.x, chunk.y)) {
                return Err(EditLayerError::File(format!(
                    "weight chunk ({}, {}) is outside the grid or repeated",
                    chunk.x, chunk.y
                )));
            }
            let rect = DirtyRect {
                col_start: chunk.x * EDIT_LAYERS_CHUNK,
                col_end: ((chunk.x + 1) * EDIT_LAYERS_CHUNK).min(self.width),
                row_start: chunk.y * EDIT_LAYERS_CHUNK,
                row_end: ((chunk.y + 1) * EDIT_LAYERS_CHUNK).min(self.height),
            };
            let cols = (rect.col_end - rect.col_start) as usize;
            let samples = cols * (rect.row_end - rect.row_start) as usize;
            if chunk.channels.len() != samples * MAX_LAYERS {
                return Err(EditLayerError::File(format!(
                    "weight chunk ({}, {}) has {} values",
                    chunk.x,
                    chunk.y,
                    chunk.channels.len()
                )));
            }
            if chunk.channels.iter().any(|v| !v.is_finite()) {
                return Err(EditLayerError::File(format!(
                    "weight chunk ({}, {}) has a value that is not finite",
                    chunk.x, chunk.y
                )));
            }
            for s in 0..samples {
                let start = s * MAX_LAYERS;
                let mut w = [0.0; MAX_LAYERS];
                w.copy_from_slice(&chunk.channels[start..start + MAX_LAYERS]);
                if keep(&w) {
                    let c = rect.col_start as usize + s % cols;
                    let r = rect.row_start as usize + s / cols;
                    into.insert(r * self.width as usize + c, w);
                }
            }
        }
        Ok(())
    }
}

/// A change to a terrain's edit-layer stack ([`apply_op`]).
#[derive(Clone, Debug, PartialEq)]
pub enum EditLayerOp {
    /// Adds a layer on top and makes it active (creates the stack on first use).
    Create {
        /// Name.
        name: String,
        /// Blend.
        blend: EditLayerBlend,
    },
    /// Renames, toggles, locks or changes the opacity of a layer.
    Set {
        /// The layer.
        id: u32,
        /// What changes.
        patch: EditLayerPatch,
    },
    /// Removes a layer.
    Delete {
        /// The layer.
        id: u32,
    },
    /// Moves a layer to a stack position (0 is the bottom).
    Reorder {
        /// The layer.
        id: u32,
        /// The new position (clamped).
        index: usize,
    },
    /// Bakes every enabled layer into the base.
    Flatten,
    /// Sets or clears the procgen target of a layer. `None` detaches it.
    SetProcgen {
        /// The layer.
        id: u32,
        /// The target, or `None` to detach.
        target: Option<ProcgenLayerTarget>,
    },
    /// Paints material weights on the base (`layer` is `None`) or one layer.
    PaintWeights {
        /// The layer, or `None` for the base.
        layer: Option<u32>,
        /// Row-major sample index.
        index: u32,
        /// Channel values, padded with 0 up to [`MAX_LAYERS`].
        channels: [f32; MAX_LAYERS],
    },
}

impl EditLayerOp {
    /// The undo menu label.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Create { name, .. } => format!("Add terrain layer {name}"),
            Self::Set { patch, .. } => {
                if patch.enabled == Some(false) {
                    "Hide terrain layer".to_owned()
                } else if patch.enabled == Some(true) {
                    "Show terrain layer".to_owned()
                } else if patch.locked.is_some() {
                    "Lock terrain layer".to_owned()
                } else if patch.opacity.is_some() {
                    "Terrain layer opacity".to_owned()
                } else {
                    "Rename terrain layer".to_owned()
                }
            }
            Self::Delete { .. } => "Delete terrain layer".to_owned(),
            Self::Reorder { .. } => "Reorder terrain layers".to_owned(),
            Self::Flatten => "Flatten terrain layers into base".to_owned(),
            Self::SetProcgen { target: None, .. } => "Detach terrain layer procgen".to_owned(),
            Self::SetProcgen { .. } => "Attach terrain layer procgen".to_owned(),
            Self::PaintWeights { .. } => "Paint terrain layer weights".to_owned(),
        }
    }
}

/// What [`apply_op`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EditLayerOpOutcome {
    /// The layer created or changed.
    pub layer: Option<u32>,
    /// Map samples recomputed.
    pub recomputed: usize,
    /// Layers merged by a flatten.
    pub merged: usize,
}

/// The region whose fold can differ between two stacks: the bounds of every layer whose presence,
/// position among the others, visibility or opacity differs.
#[must_use]
pub fn changed_region(a: Option<&TerrainEditLayers>, b: Option<&TerrainEditLayers>) -> Option<DirtyRect> {
    let empty: &[EditLayer] = &[];
    let la = a.map_or(empty, |s| &s.layers);
    let lb = b.map_or(empty, |s| &s.layers);
    let visible = |l: &EditLayer| l.enabled && l.opacity > 0.0;
    let mut region = None;
    for (layers, others) in [(la, lb), (lb, la)] {
        for (i, l) in layers.iter().enumerate() {
            let same = others.iter().position(|o| o.id == l.id).is_some_and(|j| {
                let o = &others[j];
                // The relative order matters: compare the ids below.
                let below: Vec<u32> = layers[..i].iter().map(|x| x.id).collect();
                let below_o: Vec<u32> = others[..j].iter().map(|x| x.id).collect();
                visible(l) == visible(o)
                    && (!visible(l) || l.opacity.to_bits() == o.opacity.to_bits())
                    && Arc::ptr_eq(&l.values, &o.values)
                    && Arc::ptr_eq(&l.coverage, &o.coverage)
                    && below == below_o
            });
            if !same && (visible(l) || others.iter().any(|o| o.id == l.id && visible(o))) {
                region = union(region, l.bounds);
            }
        }
    }
    region
}

fn no_terrain(entity: Entity) -> EditLayerError {
    EditLayerError::NoTerrain(entity.to_bits())
}

/// Absorbs direct map writes into an entity's base and publishes their canonical composite.
/// Returns the number of samples recomputed; a terrain without a stack needs no work.
///
/// Use before saving a stack, or after a low-level write whose dirty rectangle is unavailable.
/// Regional writers should use [`TerrainEditLayers::recompute_terrain`] instead.
///
/// # Errors
///
/// [`EditLayerError::NoTerrain`] or [`EditLayerError::SizeMismatch`].
pub fn reconcile_world_base(world: &mut World, entity: Entity) -> Result<usize, EditLayerError> {
    let _span = tracing::info_span!(target: "regia_terrain", "terrain.edit_layers.reconcile").entered();
    let Ok(mut e) = world.get_entity_mut(entity) else {
        return Err(no_terrain(entity));
    };
    if !e.contains::<Terrain>() {
        return Err(no_terrain(entity));
    }
    if !e.contains::<TerrainEditLayers>() {
        return Ok(0);
    }
    if !e.contains::<TerrainEdits>() {
        e.insert(TerrainEdits::default());
    }
    let mut query = world.query::<(&mut Terrain, &mut TerrainEditLayers, &mut TerrainEdits)>();
    let Ok((mut terrain, mut stack, mut edits)) = query.get_mut(world, entity) else {
        return Err(no_terrain(entity));
    };
    let whole = stack.whole_map();
    let rect = stack.absorb_base_changes(&terrain.map, whole)?;
    stack.recompute_terrain(&mut terrain, &mut edits, rect)
}

/// Applies `op` to the stack of `entity`, recomputes the changed samples of its map and logs the
/// change for the editor's undo stack ([`TerrainEditLayerLog`]).
///
/// # Errors
///
/// [`EditLayerError::NoTerrain`], and the op's own refusals. Nothing changes then.
pub fn apply_op(
    world: &mut World,
    entity: Entity,
    op: &EditLayerOp,
) -> Result<EditLayerOpOutcome, EditLayerError> {
    let _span = tracing::info_span!(target: "regia_terrain", "terrain.edit_layers.op").entered();
    reconcile_world_base(world, entity)?;
    let Some(terrain) = world.get::<Terrain>(entity) else {
        return Err(no_terrain(entity));
    };
    let before = world.get::<TerrainEditLayers>(entity).cloned();
    let mut stack = match (&before, op) {
        (Some(s), _) => s.clone(),
        (None, EditLayerOp::Create { .. } | EditLayerOp::PaintWeights { layer: None, .. }) => {
            TerrainEditLayers::new(&terrain.map)
        }
        (None, EditLayerOp::Flatten) => return Ok(EditLayerOpOutcome::default()),
        (
            None,
            EditLayerOp::Set { id, .. }
            | EditLayerOp::Delete { id }
            | EditLayerOp::Reorder { id, .. }
            | EditLayerOp::SetProcgen { id, .. }
            | EditLayerOp::PaintWeights { layer: Some(id), .. },
        ) => {
            return Err(EditLayerError::NotFound(*id));
        }
    };
    let mut outcome = EditLayerOpOutcome::default();
    match op {
        EditLayerOp::Create { name, blend } => outcome.layer = Some(stack.create(name, *blend)?),
        EditLayerOp::Set { id, patch } => {
            stack.set(*id, patch)?;
            outcome.layer = Some(*id);
        }
        EditLayerOp::Delete { id } => {
            stack.delete(*id)?;
            outcome.layer = Some(*id);
        }
        EditLayerOp::Reorder { id, index } => {
            stack.reorder(*id, *index)?;
            outcome.layer = Some(*id);
        }
        EditLayerOp::Flatten => outcome.merged = stack.flatten_into_base()?,
        EditLayerOp::SetProcgen { id, target } => {
            stack.set_procgen_target(*id, target.clone())?;
            outcome.layer = Some(*id);
        }
        EditLayerOp::PaintWeights {
            layer,
            index,
            channels,
        } => {
            stack.paint_weights(*layer, *index as usize, channels)?;
            outcome.layer = *layer;
        }
    }
    outcome.recomputed = install_stack(world, entity, before.as_ref(), Some(stack.clone()))?;
    world
        .get_resource_or_insert_with::<TerrainEditLayerLog>(TerrainEditLayerLog::default)
        .push(EditLayerLogEntry::Stack {
            entity,
            before: before.map(Box::new),
            after: Box::new(stack),
            label: op.label(),
        });
    Ok(outcome)
}

/// Puts `next` on `entity` (removes the stack for `None`) and recomputes what differs from
/// `prev`; returns the samples recomputed. Undo and redo of a stack change use this.
///
/// # Errors
///
/// [`EditLayerError::NoTerrain`], [`EditLayerError::SizeMismatch`].
pub fn install_stack(
    world: &mut World,
    entity: Entity,
    prev: Option<&TerrainEditLayers>,
    next: Option<TerrainEditLayers>,
) -> Result<usize, EditLayerError> {
    reconcile_world_base(world, entity)?;
    let live = world.get::<TerrainEditLayers>(entity).cloned();
    let mut region = changed_region(prev, next.as_ref());
    let Ok(mut e) = world.get_entity_mut(entity) else {
        return Err(no_terrain(entity));
    };
    if !e.contains::<Terrain>() {
        return Err(no_terrain(entity));
    }
    if !e.contains::<TerrainEdits>() {
        e.insert(TerrainEdits::default());
    }
    let Some(mut next) = next else {
        e.remove::<TerrainEditLayers>();
        return Ok(0);
    };
    next.unsaved = true;
    let mut query = world.query::<(&mut Terrain, &mut TerrainEdits)>();
    let Ok((mut terrain, mut edits)) = query.get_mut(world, entity) else {
        return Err(no_terrain(entity));
    };
    next.check_map(&terrain.map)?;
    if let Some(live) = live {
        next.inherit_base_changes(&live);
    } else {
        // Redoing the first Create can restore a snapshot from before an unlayered edit.
        // A freshly created/loaded stack already has the actual map as its published state.
        region = union(region, next.absorb_base_changes(&terrain.map, next.whole_map())?);
    }
    let n = next.recompute_terrain(&mut terrain, &mut edits, region)?;
    world.entity_mut(entity).insert(next);
    Ok(n)
}

/// Selects the layer strokes on `entity` write into (`None`: the base). Not an undo step.
///
/// # Errors
///
/// [`EditLayerError::NoTerrain`] / [`EditLayerError::NotFound`].
pub fn set_active_layer(world: &mut World, entity: Entity, layer: Option<u32>) -> Result<(), EditLayerError> {
    let Some(mut stack) = world.get_mut::<TerrainEditLayers>(entity) else {
        return match layer {
            None if world.get::<Terrain>(entity).is_some() => Ok(()),
            None => Err(no_terrain(entity)),
            Some(id) => Err(EditLayerError::NotFound(id)),
        };
    };
    stack.set_active(layer)
}

/// Moves a stroke that landed on `entity` into its edit layers when it has a stack: into `layer`
/// (`Some(None)`: the base) or, for `None`, the active layer. Returns the composite record (with
/// its canonical after values) and the layer record. Without a stack, or for a paint or scatter
/// stroke, the stroke is returned as it is. A refused stroke is reverted.
///
/// # Errors
///
/// The target layer's refusal ([`TerrainEditLayers::check_target`]); the map is as before then.
pub fn absorb_world_stroke(
    world: &mut World,
    entity: Entity,
    layer: Option<Option<u32>>,
    undo: StrokeUndo,
) -> Result<(StrokeUndo, Option<LayerStrokeUndo>), EditLayerError> {
    if !world
        .get_entity(entity)
        .is_ok_and(|e| e.contains::<TerrainEditLayers>())
    {
        return Ok((undo, None));
    }
    let StrokeUndo::Height(mut record) = undo else {
        return Ok((undo, None));
    };
    let mut query = world.query::<(&mut Terrain, &mut TerrainEditLayers, &mut TerrainEdits)>();
    let Ok((mut terrain, mut stack, mut edits)) = query.get_mut(world, entity) else {
        return Ok((StrokeUndo::Height(record), None));
    };
    let target = layer.unwrap_or(stack.active);
    match stack.absorb_stroke_into(target, &mut record, &mut terrain.map) {
        Ok(layer_undo) => {
            let terrain = &mut *terrain;
            edits.note(&terrain.map, &mut terrain.minmax, record.dirty, &record.tiles);
            Ok((StrokeUndo::Height(record), Some(layer_undo)))
        }
        Err(err) => {
            let undo = StrokeUndo::Height(record);
            undo.restore(&mut terrain, None, None, &mut edits, true);
            Err(err)
        }
    }
}

/// One sample a stroke changed in a layer (or the base): `[value, coverage]` before and after.
#[derive(Clone, Copy, Debug, PartialEq)]
struct LayerSampleChange {
    index: u32,
    before: [f32; 2],
    after: [f32; 2],
    /// The composite before/after values restored by the matching height undo record.
    composite: [f32; 2],
    /// The unlayered displacement already included when the stroke was recorded.
    absorbed_base: f64,
}

/// The layer half of a layered stroke's undo entry; the composite half is the stroke's
/// [`StrokeUndo`].
#[derive(Clone, Debug, PartialEq)]
pub struct LayerStrokeUndo {
    /// The layer written (`None`: the base).
    pub layer: Option<u32>,
    changes: Vec<LayerSampleChange>,
}

impl LayerStrokeUndo {
    /// Samples changed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// `true` when nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Writes the layer values from before (`undo`) or after the stroke. The caller restores the
    /// composite with the stroke's [`StrokeUndo`], then recomputes its dirty rectangle so later
    /// unlayered base changes remain visible. Reconcile raw writes before restoring either
    /// record. Returns `false` when the layer is gone.
    pub fn restore(&self, stack: &mut TerrainEditLayers, undo: bool) -> bool {
        let target = match self.layer {
            None => None,
            Some(id) => match stack.index_of(id) {
                Some(i) => Some(i),
                None => return false,
            },
        };
        let n = stack.samples();
        for ch in &self.changes {
            let i = ch.index as usize;
            if i >= n {
                continue;
            }
            let [mut v, c] = if undo { ch.before } else { ch.after };
            if target.is_none() {
                let later = stack.base_changes.get(&i).copied().unwrap_or_default() - ch.absorbed_base;
                v = (f64::from(v) + later) as f32;
            }
            stack.write(target, i, v, c);
            Arc::make_mut(&mut stack.published)[i] = ch.composite[usize::from(!undo)];
        }
        stack.unsaved = true;
        true
    }
}

/// An edit-layer change made outside the editor's own panel (the `terrain.edit_layer_*` and
/// `terrain.stroke` agent methods); the editor drains these onto its undo stack.
#[derive(Clone, Debug, PartialEq)]
pub enum EditLayerLogEntry {
    /// A layered stroke: the composite record and the layer record.
    Stroke {
        /// The terrain.
        entity: Entity,
        /// The composite half.
        stroke: StrokeUndo,
        /// The layer half.
        layer: LayerStrokeUndo,
    },
    /// A stack change (create, set, delete, reorder, flatten): the stack before and after.
    Stack {
        /// The terrain.
        entity: Entity,
        /// The stack before (`None`: the terrain had no stack).
        before: Option<Box<TerrainEditLayers>>,
        /// The stack after.
        after: Box<TerrainEditLayers>,
        /// What the change did, for the undo menu.
        label: String,
    },
}

/// Edit-layer changes waiting for the editor's undo stack.
#[derive(Resource, Debug, Default)]
pub struct TerrainEditLayerLog {
    /// Entries in the order they were made.
    pub entries: Vec<EditLayerLogEntry>,
    /// An editor drains this log (else only the newest [`crate::STROKE_LOG_CAP`] are kept).
    pub drained: bool,
}

impl TerrainEditLayerLog {
    /// Appends an entry (capped while no editor drains the log).
    pub fn push(&mut self, entry: EditLayerLogEntry) {
        self.entries.push(entry);
        if !self.drained {
            let excess = self.entries.len().saturating_sub(crate::STROKE_LOG_CAP);
            self.entries.drain(..excess);
        }
    }
}

/// `<map>.terrain.editlayers` beside a level or map path (the holes file's stem rule).
#[must_use]
pub fn edit_layers_path(map: &Path) -> PathBuf {
    let holes = crate::holes_ron_path(map);
    let name = holes.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let stem = name.strip_suffix(".holes.ron").unwrap_or(name);
    holes.with_file_name(format!("{stem}.terrain.editlayers"))
}

/// Reads `<map>.terrain.editlayers` over `map`; `Ok(None)` when there is no file.
///
/// # Errors
///
/// The file cannot be read, or [`TerrainEditLayers::from_bytes`] refuses it.
pub fn load_edit_layers(path: &Path, map: &TerrainMap) -> Result<Option<TerrainEditLayers>, EditLayerError> {
    match std::fs::read(path) {
        Ok(bytes) => TerrainEditLayers::from_bytes(&bytes, map).map(Some),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(EditLayerError::File(format!("{}: {err}", path.display()))),
    }
}

/// The version of a `.terrain.editlayers` blob (its first varint), `None` when unreadable.
#[must_use]
pub fn edit_layers_file_version(bytes: &[u8]) -> Option<u32> {
    postcard::take_from_bytes::<u32>(bytes).ok().map(|(v, _)| v)
}

/// One stored chunk: row-major values of a `EDIT_LAYERS_CHUNK`² block (clipped at the map edge).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EditLayerChunk {
    /// Chunk column.
    pub x: u32,
    /// Chunk row.
    pub y: u32,
    /// Values, row-major.
    pub values: Vec<f32>,
}

/// One layer in the file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EditLayerFile {
    /// Stable id.
    pub id: u32,
    /// Name.
    pub name: String,
    /// On.
    pub enabled: bool,
    /// Locked.
    pub locked: bool,
    /// `0..=1`.
    pub opacity: f32,
    /// Blend.
    pub blend: EditLayerBlend,
    /// Value chunks (only chunks the layer touches).
    pub values: Vec<EditLayerChunk>,
    /// Set layers: coverage chunks.
    pub coverage: Vec<EditLayerChunk>,
    /// Material-weight chunks (sample-major, [`MAX_LAYERS`] channels). Empty when unpainted.
    pub weights: Vec<EditLayerWeightChunk>,
    /// The procgen output that owns this layer, if any.
    pub procgen: Option<ProcgenLayerTarget>,
}

/// One stored weight chunk: row-major samples, each [`MAX_LAYERS`] channels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EditLayerWeightChunk {
    /// Chunk column.
    pub x: u32,
    /// Chunk row.
    pub y: u32,
    /// `samples * MAX_LAYERS` weights, sample-major.
    pub channels: Vec<f32>,
}

/// The `<map>.terrain.editlayers` file, version 2 (`docs/formats/terrain-editlayers.md`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EditLayersFile {
    /// [`EDIT_LAYERS_FORMAT_VERSION`]; first, so a reader refuses a newer file before the body.
    pub version: u32,
    /// Columns.
    pub width: u32,
    /// Rows.
    pub height: u32,
    /// [`EDIT_LAYERS_CHUNK`].
    pub chunk: u32,
    /// The layer strokes write into.
    pub active: Option<u32>,
    /// The next layer id.
    pub next_id: u32,
    /// Base chunks where any layer covers the map.
    pub base: Vec<EditLayerChunk>,
    /// Layers, bottom first.
    pub layers: Vec<EditLayerFile>,
    /// Base material weights. A missing sample is channel 0 at weight 1.
    pub base_weights: Vec<EditLayerWeightChunk>,
}

/// Version 1 of the sidecar, kept so a v1 file decodes without the v2 fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct EditLayerFileV1 {
    id: u32,
    name: String,
    enabled: bool,
    locked: bool,
    opacity: f32,
    blend: EditLayerBlend,
    values: Vec<EditLayerChunk>,
    coverage: Vec<EditLayerChunk>,
}

/// Version 1 of the sidecar file. Field order matches the frozen v1 layout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct EditLayersFileV1 {
    version: u32,
    width: u32,
    height: u32,
    chunk: u32,
    active: Option<u32>,
    next_id: u32,
    base: Vec<EditLayerChunk>,
    layers: Vec<EditLayerFileV1>,
}

fn chunk_weights(
    width: u32,
    height: u32,
    map: &BTreeMap<usize, [f32; MAX_LAYERS]>,
    fill: [f32; MAX_LAYERS],
) -> Vec<EditLayerWeightChunk> {
    let mut out = Vec::new();
    if map.is_empty() {
        return out;
    }
    let cw = width.div_ceil(EDIT_LAYERS_CHUNK);
    let ch = height.div_ceil(EDIT_LAYERS_CHUNK);
    for cy in 0..ch {
        for cx in 0..cw {
            let rect = DirtyRect {
                col_start: cx * EDIT_LAYERS_CHUNK,
                col_end: ((cx + 1) * EDIT_LAYERS_CHUNK).min(width),
                row_start: cy * EDIT_LAYERS_CHUNK,
                row_end: ((cy + 1) * EDIT_LAYERS_CHUNK).min(height),
            };
            let samples = (rect.col_end - rect.col_start) as usize * (rect.row_end - rect.row_start) as usize;
            let mut any = false;
            let mut channels = Vec::with_capacity(samples * MAX_LAYERS);
            for r in rect.row_start..rect.row_end {
                for c in rect.col_start..rect.col_end {
                    let i = r as usize * width as usize + c as usize;
                    if let Some(w) = map.get(&i) {
                        any = true;
                        channels.extend_from_slice(w);
                    } else {
                        channels.extend_from_slice(&fill);
                    }
                }
            }
            if any {
                out.push(EditLayerWeightChunk {
                    x: cx,
                    y: cy,
                    channels,
                });
            }
        }
    }
    out
}

fn default_base_weight() -> [f32; MAX_LAYERS] {
    let mut w = [0.0; MAX_LAYERS];
    w[0] = 1.0;
    w
}

fn is_default_base(w: &[f32; MAX_LAYERS]) -> bool {
    w[0].to_bits() == 1.0f32.to_bits() && w[1..].iter().all(|c| c.to_bits() == 0.0f32.to_bits())
}

fn is_zero_weight(w: &[f32; MAX_LAYERS]) -> bool {
    w.iter().all(|c| c.to_bits() == 0.0f32.to_bits())
}

fn validate_procgen_target(target: &ProcgenLayerTarget) -> Result<(), EditLayerError> {
    let bad = |text: &str, max: usize| {
        let n = text.chars().count();
        n == 0 || n > max || text.chars().any(char::is_control) || text.trim().is_empty()
    };
    if bad(&target.graph, MAX_PROCGEN_GRAPH_LEN) || bad(&target.output, MAX_EDIT_LAYER_NAME) {
        return Err(EditLayerError::BadTarget);
    }
    Ok(())
}

fn v1_file_to_v2(file: EditLayersFileV1) -> EditLayersFile {
    EditLayersFile {
        version: EDIT_LAYERS_FORMAT_VERSION,
        width: file.width,
        height: file.height,
        chunk: file.chunk,
        active: file.active,
        next_id: file.next_id,
        base: file.base,
        layers: file
            .layers
            .into_iter()
            .map(|layer| EditLayerFile {
                id: layer.id,
                name: layer.name,
                enabled: layer.enabled,
                locked: layer.locked,
                opacity: layer.opacity,
                blend: layer.blend,
                values: layer.values,
                coverage: layer.coverage,
                weights: Vec::new(),
                procgen: None,
            })
            .collect(),
        base_weights: Vec::new(),
    }
}

fn decode_edit_layers_file(bytes: &[u8]) -> Result<EditLayersFile, EditLayerError> {
    let bad = EditLayerError::File;
    let (version, _) = postcard::take_from_bytes::<u32>(bytes).map_err(|e| bad(e.to_string()))?;
    match version {
        EDIT_LAYERS_FORMAT_VERSION => postcard::from_bytes(bytes).map_err(|e| bad(e.to_string())),
        EDIT_LAYERS_V1 => {
            let file: EditLayersFileV1 = postcard::from_bytes(bytes).map_err(|e| bad(e.to_string()))?;
            Ok(v1_file_to_v2(file))
        }
        other => Err(EditLayerError::Version(other)),
    }
}

fn edit_layers_grid(bytes: &[u8]) -> Result<(u32, u32), EditLayerError> {
    let bad = EditLayerError::File;
    let (_, rest) = postcard::take_from_bytes::<u32>(bytes).map_err(|e| bad(e.to_string()))?;
    let (width, rest) = postcard::take_from_bytes::<u32>(rest).map_err(|e| bad(e.to_string()))?;
    let (height, _) = postcard::take_from_bytes::<u32>(rest).map_err(|e| bad(e.to_string()))?;
    if width < 2 || height < 2 || u64::from(width) * u64::from(height) > MAX_EDIT_LAYER_SAMPLES {
        return Err(bad(format!("grid {width} x {height} is out of range")));
    }
    Ok((width, height))
}

/// Migrates a `<map>.terrain.editlayers` blob to version 2.
///
/// A v2 blob is returned unchanged, byte for byte. A v1 layer becomes a v2 layer with no weight
/// channels and no procgen target; every stored height value keeps its `f32` bits. Any other
/// version is [`EditLayerError::Version`].
///
/// # Errors
///
/// [`EditLayerError::Version`] or [`EditLayerError::File`].
pub fn migrate_edit_layers_bytes(bytes: &[u8]) -> Result<Vec<u8>, EditLayerError> {
    let bad = EditLayerError::File;
    let (version, _) = postcard::take_from_bytes::<u32>(bytes).map_err(|e| bad(e.to_string()))?;
    match version {
        EDIT_LAYERS_FORMAT_VERSION => Ok(bytes.to_vec()),
        EDIT_LAYERS_V1 => {
            edit_layers_grid(bytes)?;
            let file: EditLayersFileV1 = postcard::from_bytes(bytes).map_err(|e| bad(e.to_string()))?;
            postcard::to_allocvec(&v1_file_to_v2(file)).map_err(|e| bad(e.to_string()))
        }
        other => Err(EditLayerError::Version(other)),
    }
}

/// The field fingerprint of the sidecar (`cargo xtask check-formats`).
#[must_use]
pub fn edit_layers_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<EditLayersFile>(),
        regia_core::schema::serde_field_names::<EditLayerFile>(),
        regia_core::schema::serde_field_names::<EditLayerChunk>(),
        regia_core::schema::serde_field_names::<EditLayerWeightChunk>(),
        regia_core::schema::serde_field_names::<ProcgenLayerTarget>(),
    ])
}

/// Versions a migrator must still read. v1 is the only older sidecar.
#[must_use]
pub fn edit_layers_migration_sources() -> Vec<u32> {
    vec![EDIT_LAYERS_V1]
}

#[cfg(test)]
mod tests;
