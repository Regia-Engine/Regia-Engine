// SPDX-License-Identifier: MIT
// Portions ported from Fyrox (https://github.com/FyroxEngine/Fyrox, commit 0876f67),
// fyrox-impl/src/scene/terrain/brushstroke/mod.rs.
// Copyright (c) 2019-present Dmitry Stepanov and Fyrox Engine contributors, MIT licence
// (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

//! Terrain sculpt and paint brushes: Raise / Lower / Flatten / Assign / Smooth with circle or
//! rectangle shapes and hardness falloff, over the heightfield ([`crate::TerrainMap`]) or one
//! painted layer-weight map ([`crate::LayerWeightMap`]).
//!
//! A stroke runs through three buffers, as in the original:
//!
//! 1. A channel of [`BrushThreadMessage`]s feeds a worker thread ([`BrushThread`]). Pixel
//!    messages go to the next buffer; start/end messages act at once.
//! 2. A [`PixelMessageBuffer`] holds a few pixel messages and coalesces repeats of the same
//!    sample (a brush dragged over the terrain hits the same sample many times in a row).
//! 3. [`StrokeData`] keeps, per touched sample, the brush strength, the value before the stroke
//!    and the latest value. Computed samples accumulate in [`StrokeChunks`] (grouped by cook
//!    tile) and are written back in batches.
//!
//! Regia Engine's write-back differs from Fyrox's: the worker never touches the live terrain. It
//! reads a [`GridSnapshot`] taken at stroke start and sends [`StrokeBatch`]es back; the owner of
//! the ECS [`crate::Terrain`] applies them, which yields the [`DirtyRect`] for the live GPU
//! update ([`crate::gpu::TerrainGpu::update_heights`], the same path craters use) and the
//! touched tiles for the incremental re-cook. At stroke end the worker sends a
//! [`StrokeRecord`]: the before/after value of every changed sample, which is the stroke's one
//! undo entry and restores the heights bit for bit.
//!
//! Each sample is written at most once per stroke strength: the result is computed from the
//! sample's *original* value with the strongest brush coverage seen so far, so the outcome does
//! not depend on message timing or coalescing. The synchronous path ([`BrushStroke`] driven
//! directly, used by `terrain.stroke` and the tests) and the threaded path agree bit for bit.
//!
//! Changes from the original besides nalgebra → glam: an explicit `Lower` mode, a weight-map
//! target instead of Fyrox's chunk textures, neighbour reads for Smooth fixed to read the
//! neighbour (the original read the centre sample for untouched neighbours) and clamped at
//! the map border (the original used 0), and no hole-mask target (Regia Engine's holes are
//! authored ellipses, [`crate::holes`]).

pub mod chunks;
pub mod raster;

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use glam::{IVec2, Mat2, UVec2, Vec2};
use regia_core::math::det_cos;
use serde::{Deserialize, Serialize};

pub use chunks::{StrokeChunks, TileCoord, union_rect};
pub use raster::{
    BrushPixel, BrushRaster, CircleRaster, IRect, RectIter, RectRaster, SmearPixels, StampPixels,
};

use crate::deform::DirtyRect;

/// Pixel messages accepted before the worker must process them. Later messages often make
/// earlier ones redundant, so letting a few accumulate saves work.
pub const MESSAGE_BUFFER_SIZE: usize = 40;
/// Computed samples held before they are written back. Fyrox uses 40 because its write-back
/// modifies a texture resource; Regia Engine's is a slice copy plus one dirty-rect upload a frame,
/// so batches are larger.
pub const PIXEL_BUFFER_SIZE: usize = 1024;
/// Most samples one stamp or smear may touch: a guard against a typed-in brush so large the
/// editor would freeze.
pub const BRUSH_PIXEL_SANITY_LIMIT: i64 = 1_000_000;

/// A value a brush can edit: heights (`f32`, metres) or layer weights (`u8`, 0..=255 = 0..=1).
pub trait BrushValue: Copy + Default + PartialEq + std::fmt::Debug + Send + Sync + 'static {
    /// `self` raised by `amount` (metres for heights, weight fraction for layer weights).
    fn raise(self, amount: f32) -> Self;
    /// `self` blended toward `value` by `t` (`value` in the same units as `raise`).
    fn lerp_to(self, value: f32, t: f32) -> Self;
    /// The value as `f32` for the Smooth mean (raw units: metres or 0..=255).
    fn to_mean(self) -> f32;
    /// `self` blended toward the neighbourhood `mean` (raw units) by `t`.
    fn smooth_to(self, mean: f32, t: f32) -> Self;
}

impl BrushValue for f32 {
    fn raise(self, amount: f32) -> Self {
        self + amount
    }

    fn lerp_to(self, value: f32, t: f32) -> Self {
        self * (1.0 - t) + value * t
    }

    fn to_mean(self) -> f32 {
        self
    }

    fn smooth_to(self, mean: f32, t: f32) -> Self {
        self * (1.0 - t) + mean * t
    }
}

impl BrushValue for u8 {
    fn raise(self, amount: f32) -> Self {
        (f32::from(self) + amount * 255.0).clamp(0.0, 255.0) as u8
    }

    fn lerp_to(self, value: f32, t: f32) -> Self {
        (f32::from(self) * (1.0 - t) + value * 255.0 * t).clamp(0.0, 255.0) as u8
    }

    fn to_mean(self) -> f32 {
        f32::from(self)
    }

    fn smooth_to(self, mean: f32, t: f32) -> Self {
        // Integer mean, as the original's mask smoothing.
        (f32::from(self) * (1.0 - t) + mean.floor() * t).clamp(0.0, 255.0) as u8
    }
}

/// Read access to a sample grid for a stroke.
pub trait StrokeSource<V> {
    /// Grid size in samples `(columns, rows)`.
    fn size(&self) -> UVec2;
    /// The value at `position`, `None` outside the grid.
    fn read(&self, position: IVec2) -> Option<V>;
    /// The value at `position` clamped to the border (`None` only for an empty grid).
    fn read_clamped(&self, position: IVec2) -> Option<V> {
        let size = self.size().as_ivec2();
        if size.x <= 0 || size.y <= 0 {
            return None;
        }
        self.read(position.clamp(IVec2::ZERO, size - IVec2::ONE))
    }
}

/// Write access to a sample grid for a stroke.
pub trait StrokeTarget<V>: StrokeSource<V> {
    /// Sets the value at `position` (ignored outside the grid).
    fn write(&mut self, position: IVec2, value: V);
}

fn in_grid(size: UVec2, p: IVec2) -> Option<usize> {
    if p.x < 0 || p.y < 0 || p.x as u32 >= size.x || p.y as u32 >= size.y {
        return None;
    }
    Some(p.y as usize * size.x as usize + p.x as usize)
}

impl StrokeSource<f32> for crate::TerrainMap {
    fn size(&self) -> UVec2 {
        UVec2::new(self.width(), self.height())
    }

    fn read(&self, position: IVec2) -> Option<f32> {
        in_grid(StrokeSource::size(self), position).map(|i| self.height_at_index(i))
    }
}

impl StrokeTarget<f32> for crate::TerrainMap {
    fn write(&mut self, position: IVec2, value: f32) {
        if let Some(i) = in_grid(StrokeSource::size(self), position) {
            self.set_height(i, value);
        }
    }
}

/// A copy of a grid taken at stroke start: what the worker thread reads.
#[derive(Clone, Debug, PartialEq)]
pub struct GridSnapshot<V> {
    size: UVec2,
    data: Vec<V>,
}

impl<V: Copy> GridSnapshot<V> {
    /// A snapshot of `size` samples (row-major `data`). `None` when the lengths disagree.
    pub fn new(size: UVec2, data: Vec<V>) -> Option<Self> {
        (data.len() == size.x as usize * size.y as usize).then_some(Self { size, data })
    }

    /// Copies every sample of `source`.
    pub fn capture(source: &impl StrokeSource<V>) -> Self {
        let size = source.size();
        let mut data = Vec::with_capacity(size.x as usize * size.y as usize);
        for row in 0..size.y as i32 {
            for col in 0..size.x as i32 {
                if let Some(v) = source.read(IVec2::new(col, row)) {
                    data.push(v);
                }
            }
        }
        Self { size, data }
    }
}

impl<V: Copy> StrokeSource<V> for GridSnapshot<V> {
    fn size(&self) -> UVec2 {
        self.size
    }

    fn read(&self, position: IVec2) -> Option<V> {
        in_grid(self.size, position).and_then(|i| self.data.get(i).copied())
    }
}

impl<V: Copy> StrokeTarget<V> for GridSnapshot<V> {
    fn write(&mut self, position: IVec2, value: V) {
        if let Some(slot) = in_grid(self.size, position).and_then(|i| self.data.get_mut(i)) {
            *slot = value;
        }
    }
}

/// Shape of a brush, in metres.
#[derive(Copy, Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum BrushShape {
    /// A disc of the given radius.
    Circle {
        /// Radius, metres.
        radius: f32,
    },
    /// A rectangle (`width` along X, `length` along Z).
    Rectangle {
        /// Extent along X, metres.
        width: f32,
        /// Extent along Z, metres.
        length: f32,
    },
}

impl Default for BrushShape {
    fn default() -> Self {
        BrushShape::Circle { radius: 8.0 }
    }
}

impl BrushShape {
    /// `true` when `point` is inside the shape centred at `center` (metres).
    pub fn contains(&self, center: Vec2, point: Vec2) -> bool {
        match *self {
            BrushShape::Circle { radius } => center.distance(point) < radius,
            BrushShape::Rectangle { width, length } => {
                let d = (point - center).abs();
                d.x <= width * 0.5 && d.y <= length * 0.5
            }
        }
    }

    /// Bounding radius, metres.
    pub fn reach(&self) -> f32 {
        match *self {
            BrushShape::Circle { radius } => radius,
            BrushShape::Rectangle { width, length } => 0.5 * width.hypot(length),
        }
    }

    /// Scales the shape's size by `factor` (clamped to at least 1 cm).
    pub fn scale(&mut self, factor: f32) {
        match self {
            BrushShape::Circle { radius } => *radius = (*radius * factor).max(0.01),
            BrushShape::Rectangle { width, length } => {
                *width = (*width * factor).max(0.01);
                *length = (*length * factor).max(0.01);
            }
        }
    }
}

/// What a brush does to the samples under it.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub enum BrushMode {
    /// Add `amount` (metres for heights, weight fraction for layers) at full strength.
    Raise {
        /// Offset at full strength.
        amount: f32,
    },
    /// Subtract `amount` at full strength.
    Lower {
        /// Offset at full strength.
        amount: f32,
    },
    /// Blend toward the value picked at stroke start (the height under the cursor).
    Flatten,
    /// Blend toward a fixed value (metres, or a weight in 0..=1).
    Assign {
        /// Target value.
        value: f32,
    },
    /// Blend toward the mean of the `(2r+1)²` neighbourhood (centre excluded).
    Smooth {
        /// Neighbourhood radius in samples (1 = 3×3).
        kernel_radius: u32,
    },
    /// Add seeded value noise in `-amount..=amount` at full strength (`docs/specs/82-terrain-editing.md` § 2).
    Noise {
        /// Peak offset at full strength (metres for heights, weight fraction for layers).
        amount: f32,
        /// Noise feature size in samples (at least 0.5).
        wavelength: f32,
        /// Noise seed: the same seed gives the same field.
        seed: u32,
    },
    /// Blend toward the line from `start` to `end`: the target at a sample is `start_value` ..
    /// `end_value` by its projection onto the segment, clamped to it ([`ramp_mode`](crate::ramp_mode)
    /// builds one from world points).
    Ramp {
        /// Start of the ramp, sample coordinates (column, row).
        start: [f32; 2],
        /// End of the ramp, sample coordinates (column, row).
        end: [f32; 2],
        /// Target value at `start` (metres, or a weight in 0..=1).
        start_value: f32,
        /// Target value at `end`.
        end_value: f32,
    },
}

impl Default for BrushMode {
    fn default() -> Self {
        BrushMode::Raise { amount: 1.0 }
    }
}

impl BrushMode {
    /// The mode with Raise/Lower swapped, Assign inverted, the noise amount negated and the
    /// ramp's ends swapped (the editor's Shift modifier).
    pub fn inverted(self) -> Self {
        match self {
            BrushMode::Raise { amount } => BrushMode::Lower { amount },
            BrushMode::Lower { amount } => BrushMode::Raise { amount },
            BrushMode::Assign { value } => BrushMode::Assign { value: 1.0 - value },
            BrushMode::Noise {
                amount,
                wavelength,
                seed,
            } => BrushMode::Noise {
                amount: -amount,
                wavelength,
                seed,
            },
            BrushMode::Ramp {
                start,
                end,
                start_value,
                end_value,
            } => BrushMode::Ramp {
                start: end,
                end: start,
                start_value: end_value,
                end_value: start_value,
            },
            other => other,
        }
    }

    /// Short lower-case name (`"raise"`, `"lower"`, `"flatten"`, `"assign"`, `"smooth"`,
    /// `"noise"`, `"ramp"`).
    pub fn name(&self) -> &'static str {
        match self {
            BrushMode::Raise { .. } => "raise",
            BrushMode::Lower { .. } => "lower",
            BrushMode::Flatten => "flatten",
            BrushMode::Assign { .. } => "assign",
            BrushMode::Smooth { .. } => "smooth",
            BrushMode::Noise { .. } => "noise",
            BrushMode::Ramp { .. } => "ramp",
        }
    }

    /// The Noise offset factor at `position` (sample coordinates): `2 · value_noise2 − 1`, in
    /// `-1..=1`. `wavelength` is clamped to at least 0.5 samples.
    pub fn noise_at(position: IVec2, wavelength: f32, seed: u32) -> f32 {
        let w = if wavelength.is_finite() {
            wavelength.max(0.5)
        } else {
            0.5
        };
        let n = crate::noise::value_noise2(position.x as f32 / w, position.y as f32 / w, seed);
        2.0 * n - 1.0
    }

    /// The Ramp target at `position` (sample coordinates): `start_value` .. `end_value` by the
    /// projection of `position` onto `start` → `end`, clamped to the segment (`start_value` for
    /// a degenerate segment).
    pub fn ramp_at(position: IVec2, start: [f32; 2], end: [f32; 2], start_value: f32, end_value: f32) -> f32 {
        let a = Vec2::from(start);
        let d = Vec2::from(end) - a;
        let len2 = d.length_squared();
        let t = if len2 < 1e-6 {
            0.0
        } else {
            ((position.as_vec2() - a).dot(d) / len2).clamp(0.0, 1.0)
        };
        start_value + (end_value - start_value) * t
    }
}

/// The data a brush edits.
#[derive(Copy, Default, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BrushTarget {
    /// The heightfield.
    #[default]
    Height,
    /// The painted weight of one material layer ([`crate::LayerWeightMap`]).
    LayerWeight {
        /// Layer index into [`crate::TerrainLayers::layers`].
        layer: usize,
    },
}

/// Shape of the fade between the full-strength interior and the brush edge.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrushFalloff {
    /// The existing raised-cosine fade, evaluated with the deterministic cosine kernel.
    #[default]
    Smooth,
    /// A straight fade.
    Linear,
    /// A circular shoulder: `sqrt(t * (2 - t))` for inward coverage `t`.
    Sphere,
    /// A pointed shoulder: `1 - sqrt(1 - t * t)` for inward coverage `t`.
    Tip,
}

impl BrushFalloff {
    /// Maps inward coverage (zero at the edge, one in the interior) to strength.
    #[must_use]
    pub fn evaluate(self, coverage: f32) -> f32 {
        let t = coverage.clamp(0.0, 1.0);
        match self {
            Self::Smooth => 0.5 * (1.0 - det_cos(t * std::f32::consts::PI)),
            Self::Linear => t,
            Self::Sphere => (t * (2.0 - t)).sqrt(),
            Self::Tip => 1.0 - (1.0 - t * t).max(0.0).sqrt(),
        }
    }
}

/// A terrain brush: shape, mode, target, falloff and opacity.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct Brush {
    /// Shape and size.
    pub shape: BrushShape,
    /// Operation.
    pub mode: BrushMode,
    /// Edited data.
    pub target: BrushTarget,
    /// Extra 2×2 transform of the shape (rotation/shear), column-major.
    pub transform: [f32; 4],
    /// Edge softness: 0 fades gradually from the centre, 1 keeps full strength to the edge.
    pub hardness: f32,
    /// Curve used outside the full-strength interior; omitted data keeps the smooth curve.
    #[serde(default)]
    pub falloff: BrushFalloff,
    /// Opacity: 0 draws nothing, 1 applies the full operation.
    pub alpha: f32,
}

impl Default for Brush {
    fn default() -> Self {
        Self {
            shape: BrushShape::default(),
            mode: BrushMode::default(),
            target: BrushTarget::default(),
            transform: Mat2::IDENTITY.to_cols_array(),
            hardness: 0.0,
            falloff: BrushFalloff::default(),
            alpha: 1.0,
        }
    }
}

/// `true` when the stamp's bounds are within [`BRUSH_PIXEL_SANITY_LIMIT`] (warns otherwise).
fn within_size_limit(bounds: &IRect) -> bool {
    let area = bounds.sample_count();
    let accepted = area <= BRUSH_PIXEL_SANITY_LIMIT;
    if !accepted {
        tracing::warn!(target: "regia_terrain", "terrain brush dropped by the sanity limit: {area} samples");
    }
    accepted
}

impl Brush {
    /// The shape transform as a matrix, with the X row scaled for non-square samples.
    fn sample_transform(&self, scale: Vec2) -> Mat2 {
        let mut transform = Mat2::from_cols_array(&self.transform);
        let x_factor = if scale.x.abs() > f32::EPSILON {
            scale.y / scale.x
        } else {
            1.0
        };
        transform.x_axis.x *= x_factor;
        transform.y_axis.x *= x_factor;
        transform
    }

    /// Calls `draw_pixel(sample, strength)` for every sample of a stamp at `position`.
    ///
    /// - `position`: the stamp centre in sample coordinates (column, row).
    /// - `scale`: metres per sample along columns and rows (converts the shape to samples).
    pub fn stamp(&self, position: Vec2, scale: Vec2, mut draw_pixel: impl FnMut(IVec2, f32)) {
        let transform = self.sample_transform(scale);
        let sy = scale.y.max(f32::EPSILON);
        match self.shape {
            BrushShape::Circle { radius } => {
                let iter = StampPixels::new(CircleRaster(radius / sy), position, self.hardness, transform);
                if within_size_limit(&iter.bounds()) {
                    iter.for_each(|p| draw_pixel(p.position, p.strength));
                }
            }
            BrushShape::Rectangle { width, length } => {
                let iter = StampPixels::new(
                    RectRaster(width * 0.5 / sy, length * 0.5 / sy),
                    position,
                    self.hardness,
                    transform,
                );
                if within_size_limit(&iter.bounds()) {
                    iter.for_each(|p| draw_pixel(p.position, p.strength));
                }
            }
        }
    }

    /// Calls `draw_pixel(sample, strength)` for every sample of a smear from `start` to `end`
    /// (sample coordinates; `scale` as in [`Brush::stamp`]).
    pub fn smear(&self, start: Vec2, end: Vec2, scale: Vec2, mut draw_pixel: impl FnMut(IVec2, f32)) {
        let transform = self.sample_transform(scale);
        let sy = scale.y.max(f32::EPSILON);
        match self.shape {
            BrushShape::Circle { radius } => {
                let iter = SmearPixels::new(CircleRaster(radius / sy), start, end, self.hardness, transform);
                if within_size_limit(&iter.bounds()) {
                    iter.for_each(|p| draw_pixel(p.position, p.strength));
                }
            }
            BrushShape::Rectangle { width, length } => {
                let iter = SmearPixels::new(
                    RectRaster(width * 0.5 / sy, length * 0.5 / sy),
                    start,
                    end,
                    self.hardness,
                    transform,
                );
                if within_size_limit(&iter.bounds()) {
                    iter.for_each(|p| draw_pixel(p.position, p.strength));
                }
            }
        }
    }
}

/// Paint one sample with the given brush coverage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrushPixelMessage {
    /// Sample `(column, row)`.
    pub position: IVec2,
    /// Brush coverage, 0 (none) to 1 (full).
    pub alpha: f32,
    /// Mode parameter: the Flatten target value.
    pub value: f32,
}

/// A short queue of pixel messages that merges repeats of the same sample (keeping the
/// strongest coverage).
#[derive(Debug)]
pub struct PixelMessageBuffer {
    data: VecDeque<BrushPixelMessage>,
    max_size: usize,
}

impl PixelMessageBuffer {
    /// A buffer holding at most `max_size` messages (at least 1).
    pub fn new(max_size: usize) -> Self {
        let max_size = max_size.max(1);
        Self {
            data: VecDeque::with_capacity(max_size),
            max_size,
        }
    }

    /// `true` when no more messages fit.
    pub fn is_full(&self) -> bool {
        self.data.len() >= self.max_size
    }

    /// `true` when nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Queued messages.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Removes the oldest message.
    pub fn pop(&mut self) -> Option<BrushPixelMessage> {
        self.data.pop_front()
    }

    /// Queues `message`, merging it into a queued message for the same sample. Returns `false`
    /// (dropping nothing) when the buffer is full and the sample is new: the caller must pop
    /// first.
    pub fn push(&mut self, message: BrushPixelMessage) -> bool {
        if let Some(m) = self.data.iter_mut().find(|m| m.position == message.position) {
            if message.alpha > m.alpha {
                m.alpha = message.alpha;
                m.value = message.value;
            }
            true
        } else if self.is_full() {
            false
        } else {
            self.data.push_back(message);
            true
        }
    }
}

/// One sample touched by a stroke.
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct StrokeElement<V> {
    /// Strongest brush coverage seen at this sample (after the falloff curve).
    pub strength: f32,
    /// Value before the stroke.
    pub original_value: V,
    /// Current value.
    pub latest_value: V,
}

/// Every sample a stroke touched, with its original and latest value.
#[derive(Debug, Clone)]
pub struct StrokeData<V>(HashMap<IVec2, StrokeElement<V>>);

impl<V> Default for StrokeData<V> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}

impl<V: Copy> StrokeData<V> {
    /// Forgets the stroke.
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Number of touched samples.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// `true` when no sample was touched.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Value of a touched sample before the stroke.
    pub fn original_pixel_value(&self, position: IVec2) -> Option<&V> {
        self.0.get(&position).map(|x| &x.original_value)
    }

    /// Current value of a touched sample.
    pub fn latest_pixel_value(&self, position: IVec2) -> Option<&V> {
        self.0.get(&position).map(|x| &x.latest_value)
    }

    /// Sets the latest value of a sample recorded by [`StrokeData::update_strength`] (ignored
    /// for an unrecorded sample).
    pub fn set_latest(&mut self, position: IVec2, value: V) {
        if let Some(el) = self.0.get_mut(&position) {
            el.latest_value = value;
        }
    }

    /// Records coverage `strength` at `position`. Returns the sample's original value when the
    /// sample must be recomputed (first touch, or a stronger coverage than before), `None`
    /// otherwise. `pixel_value` supplies the current value on first touch (`None` outside the
    /// grid, which skips the sample).
    pub fn update_strength(
        &mut self,
        position: IVec2,
        strength: f32,
        pixel_value: impl FnOnce() -> Option<V>,
    ) -> Option<V> {
        if strength == 0.0 {
            None
        } else if let Some(element) = self.0.get_mut(&position) {
            if element.strength < strength {
                element.strength = strength;
                Some(element.original_value)
            } else {
                None
            }
        } else {
            let value = pixel_value()?;
            self.0.insert(
                position,
                StrokeElement {
                    strength,
                    original_value: value,
                    latest_value: value,
                },
            );
            Some(value)
        }
    }

    /// Every touched sample, sorted row-major.
    pub fn sorted(&self) -> Vec<(IVec2, StrokeElement<V>)> {
        let mut out: Vec<(IVec2, StrokeElement<V>)> = self.0.iter().map(|(p, e)| (*p, *e)).collect();
        out.sort_unstable_by_key(|(p, _)| (p.y, p.x));
        out
    }
}

/// What one write-back changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrokeFlush {
    /// Samples written (GPU dirty-rect upload).
    pub dirty: DirtyRect,
    /// Cook tiles written (incremental re-cook).
    pub tiles: BTreeSet<TileCoord>,
}

/// Computed samples sent from the worker thread to the terrain's owner.
#[derive(Debug, Clone, PartialEq)]
pub struct StrokeBatch<V> {
    /// `(sample, new value)`, row-major.
    pub values: Vec<(IVec2, V)>,
    /// Samples written.
    pub dirty: DirtyRect,
    /// Cook tiles written.
    pub tiles: BTreeSet<TileCoord>,
}

impl<V: Copy> StrokeBatch<V> {
    /// Writes the batch into `target` and returns what changed.
    pub fn apply(&self, target: &mut impl StrokeTarget<V>) -> StrokeFlush {
        for (p, v) in &self.values {
            target.write(*p, *v);
        }
        StrokeFlush {
            dirty: self.dirty,
            tiles: self.tiles.clone(),
        }
    }
}

/// One sample of a finished stroke.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrokeSample<V> {
    /// Sample `(column, row)`.
    pub position: IVec2,
    /// Value before the stroke.
    pub before: V,
    /// Value after the stroke.
    pub after: V,
}

/// A finished stroke: every changed sample's before/after value. Undo writes `before`, redo
/// writes `after`; both are exact copies, so undo restores the grid bit for bit.
#[derive(Debug, Clone, PartialEq)]
pub struct StrokeRecord<V> {
    /// The brush that made the stroke.
    pub brush: Brush,
    /// Changed samples, row-major.
    pub samples: Vec<StrokeSample<V>>,
    /// Bounds of the changed samples.
    pub dirty: DirtyRect,
    /// Cook tiles holding changed samples.
    pub tiles: BTreeSet<TileCoord>,
}

impl<V: Copy> StrokeRecord<V> {
    /// `true` when the stroke changed nothing.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Restores the values from before the stroke.
    pub fn undo(&self, target: &mut impl StrokeTarget<V>) -> DirtyRect {
        for s in &self.samples {
            target.write(s.position, s.before);
        }
        self.dirty
    }

    /// Re-applies the stroke.
    pub fn redo(&self, target: &mut impl StrokeTarget<V>) -> DirtyRect {
        for s in &self.samples {
            target.write(s.position, s.after);
        }
        self.dirty
    }
}

impl<V: Copy + PartialEq> StrokeRecord<V> {
    /// The record of a stroke that never finished: every sample inside `dirty` that differs
    /// between `before` and `now`. The owner of a [`BrushThread`] builds this when the worker
    /// dies after some batches were written, so what landed can still be undone.
    pub fn from_states(
        brush: Brush,
        before: &impl StrokeSource<V>,
        now: &impl StrokeSource<V>,
        dirty: DirtyRect,
        tiles: BTreeSet<TileCoord>,
    ) -> Self {
        let mut samples = Vec::new();
        for row in dirty.row_start..dirty.row_end {
            for col in dirty.col_start..dirty.col_end {
                let position = UVec2::new(col, row).as_ivec2();
                if let (Some(before), Some(after)) = (before.read(position), now.read(position))
                    && before != after
                {
                    samples.push(StrokeSample {
                        position,
                        before,
                        after,
                    });
                }
            }
        }
        Self {
            brush,
            samples,
            dirty,
            tiles,
        }
    }
}

/// An in-progress stroke: the brush, the touched samples and the pending write-back.
#[derive(Debug, Clone)]
pub struct BrushStroke<V> {
    brush: Brush,
    pixels: StrokeData<V>,
    chunks: StrokeChunks,
    /// Every tile written so far (survives [`StrokeChunks::clear`]).
    tiles: BTreeSet<TileCoord>,
}

impl<V: BrushValue> BrushStroke<V> {
    /// A new stroke with `brush`, batching write-back per cook tile of `tile_size` samples.
    pub fn new(brush: Brush, tile_size: u32) -> Self {
        Self {
            brush,
            pixels: StrokeData::default(),
            chunks: StrokeChunks::new(tile_size),
            tiles: BTreeSet::new(),
        }
    }

    /// The stroke's brush.
    pub fn brush(&self) -> &Brush {
        &self.brush
    }

    /// Samples computed but not yet written back.
    pub fn pending(&self) -> usize {
        self.chunks.count()
    }

    /// Samples touched so far.
    pub fn touched(&self) -> usize {
        self.pixels.len()
    }

    /// Stamps the brush at `position` (sample coordinates); `scale` is metres per sample and
    /// `value` the Flatten target.
    pub fn stamp(&mut self, source: &impl StrokeSource<V>, position: Vec2, scale: Vec2, value: f32) {
        let brush = self.brush;
        brush.stamp(position, scale, |position, alpha| {
            self.draw_pixel(
                source,
                BrushPixelMessage {
                    position,
                    alpha,
                    value,
                },
            );
        });
    }

    /// Smears the brush from `start` to `end` (sample coordinates).
    pub fn smear(&mut self, source: &impl StrokeSource<V>, start: Vec2, end: Vec2, scale: Vec2, value: f32) {
        let brush = self.brush;
        brush.smear(start, end, scale, |position, alpha| {
            self.draw_pixel(
                source,
                BrushPixelMessage {
                    position,
                    alpha,
                    value,
                },
            );
        });
    }

    /// Computes one sample. `source` must read the grid as it was before the stroke for every
    /// sample the stroke has not touched (the live grid or a [`GridSnapshot`]).
    pub fn draw_pixel(&mut self, source: &impl StrokeSource<V>, pixel: BrushPixelMessage) {
        if pixel.alpha <= 0.0 {
            return;
        }
        // Apply the selected falloff curve to the coverage, through the
        // deterministic `det_cos` kernel: the platform `f32::cos` is not bit-stable across
        // libms and debug/release folding, and a sample within an ulp of a heightfield
        // rounding boundary would cook to a different `u16`.
        let alpha = self.brush.falloff.evaluate(pixel.alpha);
        let position = pixel.position;
        let Some(original) = self
            .pixels
            .update_strength(position, alpha, || source.read(position))
        else {
            return;
        };
        let alpha = self.brush.alpha * alpha;
        let result = match self.brush.mode {
            BrushMode::Raise { amount } => original.raise(amount * alpha),
            BrushMode::Lower { amount } => original.raise(-amount * alpha),
            BrushMode::Flatten => original.lerp_to(pixel.value, alpha),
            BrushMode::Assign { value } => original.lerp_to(value, alpha),
            BrushMode::Smooth { kernel_radius } => original.smooth_to(
                self.neighbour_mean(source, position, kernel_radius, original),
                alpha,
            ),
            BrushMode::Noise {
                amount,
                wavelength,
                seed,
            } => original.raise(amount * alpha * BrushMode::noise_at(position, wavelength, seed)),
            BrushMode::Ramp {
                start,
                end,
                start_value,
                end_value,
            } => original.lerp_to(
                BrushMode::ramp_at(position, start, end, start_value, end_value),
                alpha,
            ),
        };
        self.pixels.set_latest(position, result);
        self.chunks.write(position);
    }

    /// Mean of the `(2r+1)²` neighbourhood around `position`, from the values before the
    /// stroke, clamped at the border. The centre is excluded after clamping, so a border
    /// offset that lands on the centre is not one of its neighbours, and the divisor is the
    /// number of samples actually added.
    fn neighbour_mean(
        &self,
        source: &impl StrokeSource<V>,
        position: IVec2,
        kernel_radius: u32,
        centre: V,
    ) -> f32 {
        let radius = kernel_radius.min(64) as i32;
        if radius == 0 {
            return centre.to_mean();
        }
        let max = source.size().as_ivec2() - IVec2::ONE;
        let mut total = 0.0f64;
        let mut count = 0u32;
        for y in -radius..=radius {
            for x in -radius..=radius {
                // Clamp first so a border sample this stroke already changed reads its original.
                let pos = (position + IVec2::new(x, y)).clamp(IVec2::ZERO, max.max(IVec2::ZERO));
                if pos == position {
                    // The centre itself, or a border offset clamped onto it: not a neighbour.
                    continue;
                }
                let value = self
                    .pixels
                    .original_pixel_value(pos)
                    .copied()
                    .or_else(|| source.read(pos))
                    .unwrap_or(centre);
                total += f64::from(value.to_mean());
                count += 1;
            }
        }
        if count == 0 {
            return centre.to_mean();
        }
        (total / f64::from(count)) as f32
    }

    /// Writes the pending samples into `target` (the terrain's live grid).
    pub fn flush(&mut self, target: &mut impl StrokeTarget<V>) -> StrokeFlush {
        let size = target.size();
        let dirty = self.chunks.dirty_rect(size.x, size.y);
        let tiles = self.chunks.tiles();
        self.chunks.apply(&self.pixels, |p, v| target.write(p, v));
        self.tiles.extend(tiles.iter().copied());
        self.chunks.clear();
        StrokeFlush { dirty, tiles }
    }

    /// Takes the pending samples as a batch for another thread to apply.
    pub fn take_batch(&mut self, size: UVec2) -> StrokeBatch<V> {
        let dirty = self.chunks.dirty_rect(size.x, size.y);
        let tiles = self.chunks.tiles();
        let mut values = Vec::with_capacity(self.chunks.count());
        self.chunks.apply(&self.pixels, |p, v| values.push((p, v)));
        self.tiles.extend(tiles.iter().copied());
        self.chunks.clear();
        StrokeBatch { values, dirty, tiles }
    }

    /// Ends the stroke: the record of every changed sample (the stroke's undo entry).
    /// Pending samples must be flushed first; [`BrushStroke::finish_into`] does both.
    pub fn finish(self, size: UVec2) -> StrokeRecord<V> {
        let mut samples = Vec::new();
        let mut rect: Option<DirtyRect> = None;
        for (position, e) in self.pixels.sorted() {
            if e.original_value == e.latest_value {
                continue;
            }
            samples.push(StrokeSample {
                position,
                before: e.original_value,
                after: e.latest_value,
            });
            let (x, y) = (position.x as u32, position.y as u32);
            if x < size.x && y < size.y {
                let one = DirtyRect {
                    col_start: x,
                    col_end: x + 1,
                    row_start: y,
                    row_end: y + 1,
                };
                rect = Some(rect.map_or(one, |r| union_rect(r, one)));
            }
        }
        let tile_size = self.chunks.tile_size();
        let tiles = samples
            .iter()
            .map(|s| (s.position.x as u32 / tile_size, s.position.y as u32 / tile_size))
            .collect();
        StrokeRecord {
            brush: self.brush,
            samples,
            dirty: rect.unwrap_or(DirtyRect {
                col_start: 0,
                col_end: 0,
                row_start: 0,
                row_end: 0,
            }),
            tiles,
        }
    }

    /// Flushes into `target` and ends the stroke.
    pub fn finish_into(mut self, target: &mut impl StrokeTarget<V>) -> StrokeRecord<V> {
        self.flush(target);
        self.finish(target.size())
    }
}

/// A message to the brush worker thread.
#[derive(Debug, Clone)]
pub enum BrushThreadMessage<V> {
    /// Begin a stroke with `brush` over `source` (the grid as it is now).
    StartStroke {
        /// The brush.
        brush: Brush,
        /// The grid before the stroke.
        source: Arc<GridSnapshot<V>>,
        /// Cook tile size for write-back batching.
        tile_size: u32,
    },
    /// No more pixels for the current stroke.
    EndStroke,
    /// Paint one sample.
    Pixel(BrushPixelMessage),
}

/// What the worker sends back.
#[derive(Debug, Clone)]
pub enum BrushThreadEvent<V> {
    /// Computed samples to write into the live grid.
    Batch(StrokeBatch<V>),
    /// The stroke ended: its undo record (every batch of the stroke was sent before this).
    Finished(StrokeRecord<V>),
}

/// The sending half of a brush worker.
#[derive(Debug, Clone)]
pub struct BrushSender<V>(Sender<BrushThreadMessage<V>>);

impl<V> BrushSender<V> {
    /// Begins a stroke. Returns `false` when the worker is gone.
    pub fn start_stroke(&self, brush: Brush, source: Arc<GridSnapshot<V>>, tile_size: u32) -> bool {
        self.0
            .send(BrushThreadMessage::StartStroke {
                brush,
                source,
                tile_size,
            })
            .is_ok()
    }

    /// Ends the current stroke. Returns `false` when the worker is gone.
    pub fn end_stroke(&self) -> bool {
        self.0.send(BrushThreadMessage::EndStroke).is_ok()
    }

    /// Paints one sample of the current stroke (zero coverage is skipped). Returns `false` when
    /// the worker is gone.
    pub fn draw_pixel(&self, position: IVec2, alpha: f32, value: f32) -> bool {
        if alpha == 0.0 {
            return true;
        }
        self.0
            .send(BrushThreadMessage::Pixel(BrushPixelMessage {
                position,
                alpha,
                value,
            }))
            .is_ok()
    }
}

/// A brush worker thread: strokes are computed off the frame thread and come back as
/// [`BrushThreadEvent`]s. Dropping the handle closes the channel; the worker finishes the
/// current stroke and exits.
#[derive(Debug)]
pub struct BrushThread<V> {
    sender: BrushSender<V>,
    events: Receiver<BrushThreadEvent<V>>,
}

impl<V: BrushValue> BrushThread<V> {
    /// Spawns the worker.
    ///
    /// # Errors
    ///
    /// Returns the OS error when the thread cannot be spawned.
    pub fn spawn() -> std::io::Result<Self> {
        let (sender, receiver) = std::sync::mpsc::channel::<BrushThreadMessage<V>>();
        let (event_tx, events) = std::sync::mpsc::channel::<BrushThreadEvent<V>>();
        std::thread::Builder::new()
            .name("terrain brush".into())
            .spawn(move || accept_messages(&receiver, &event_tx))?;
        Ok(Self {
            sender: BrushSender(sender),
            events,
        })
    }

    /// A handle whose worker has already exited: every send fails and [`Self::wait`] answers
    /// `None`. What a panicked worker leaves behind, for tests of the owner's recovery.
    #[must_use]
    pub fn without_worker() -> Self {
        let (sender, _) = std::sync::mpsc::channel::<BrushThreadMessage<V>>();
        let (_, events) = std::sync::mpsc::channel::<BrushThreadEvent<V>>();
        Self {
            sender: BrushSender(sender),
            events,
        }
    }

    /// The message sender.
    pub fn sender(&self) -> &BrushSender<V> {
        &self.sender
    }

    /// Every event available now (non-blocking).
    pub fn poll(&self) -> Vec<BrushThreadEvent<V>> {
        let mut out = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            out.push(event);
        }
        out
    }

    /// [`Self::poll`], and whether the worker is gone: nothing more will ever arrive.
    pub fn poll_closed(&self) -> (Vec<BrushThreadEvent<V>>, bool) {
        let mut out = Vec::new();
        loop {
            match self.events.try_recv() {
                Ok(event) => out.push(event),
                Err(std::sync::mpsc::TryRecvError::Empty) => return (out, false),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return (out, true),
            }
        }
    }

    /// Blocks until the next event (`None` when the worker has exited).
    pub fn wait(&self) -> Option<BrushThreadEvent<V>> {
        self.events.recv().ok()
    }
}

/// The worker loop: blocks on `receiver` until every sender is gone, painting as messages
/// arrive and sending batches and stroke records to `events`.
pub fn accept_messages<V: BrushValue>(
    receiver: &Receiver<BrushThreadMessage<V>>,
    events: &Sender<BrushThreadEvent<V>>,
) {
    let mut worker: Option<(BrushStroke<V>, Arc<GridSnapshot<V>>)> = None;
    let mut buffer = PixelMessageBuffer::new(MESSAGE_BUFFER_SIZE);
    loop {
        // Collect waiting messages until the buffer is full.
        while !buffer.is_full() {
            match receiver.try_recv() {
                Ok(message) => handle_message(message, &mut worker, &mut buffer, events),
                Err(_) => break,
            }
        }
        if let Some(pixel) = buffer.pop() {
            if let Some((stroke, source)) = worker.as_mut() {
                stroke.draw_pixel(source.as_ref(), pixel);
                if stroke.pending() >= PIXEL_BUFFER_SIZE {
                    let batch = stroke.take_batch(source.size());
                    let _ = events.send(BrushThreadEvent::Batch(batch));
                }
            }
        } else if let Some((stroke, source)) = worker.as_mut().filter(|(s, _)| s.pending() > 0) {
            // Out of pixels: hand the computed samples over before blocking.
            let batch = stroke.take_batch(source.size());
            let _ = events.send(BrushThreadEvent::Batch(batch));
        } else {
            match receiver.recv() {
                Ok(message) => handle_message(message, &mut worker, &mut buffer, events),
                Err(_) => {
                    end_stroke(&mut worker, &mut buffer, events);
                    return;
                }
            }
        }
    }
}

fn handle_message<V: BrushValue>(
    message: BrushThreadMessage<V>,
    worker: &mut Option<(BrushStroke<V>, Arc<GridSnapshot<V>>)>,
    buffer: &mut PixelMessageBuffer,
    events: &Sender<BrushThreadEvent<V>>,
) {
    match message {
        BrushThreadMessage::StartStroke {
            brush,
            source,
            tile_size,
        } => {
            // A start without an end closes the previous stroke first.
            end_stroke(worker, buffer, events);
            *worker = Some((BrushStroke::new(brush, tile_size), source));
        }
        BrushThreadMessage::EndStroke => end_stroke(worker, buffer, events),
        BrushThreadMessage::Pixel(pixel) => {
            if !buffer.push(pixel) {
                // Full with a new sample: make room by processing the oldest message.
                if let (Some(oldest), Some((stroke, source))) = (buffer.pop(), worker.as_mut()) {
                    stroke.draw_pixel(source.as_ref(), oldest);
                }
                let _ = buffer.push(pixel);
            }
        }
    }
}

fn end_stroke<V: BrushValue>(
    worker: &mut Option<(BrushStroke<V>, Arc<GridSnapshot<V>>)>,
    buffer: &mut PixelMessageBuffer,
    events: &Sender<BrushThreadEvent<V>>,
) {
    let Some((mut stroke, source)) = worker.take() else {
        // No stroke: drop stray pixels.
        while buffer.pop().is_some() {}
        return;
    };
    while let Some(pixel) = buffer.pop() {
        stroke.draw_pixel(source.as_ref(), pixel);
    }
    let size = source.size();
    if stroke.pending() > 0 {
        let batch = stroke.take_batch(size);
        let _ = events.send(BrushThreadEvent::Batch(batch));
    }
    let _ = events.send(BrushThreadEvent::Finished(stroke.finish(size)));
}

/// Metres per sample of `map` along columns and rows.
pub fn map_scale(map: &crate::TerrainMap) -> Vec2 {
    Vec2::new(
        map.world_width() / (map.width().max(2) - 1) as f32,
        map.world_width() / (map.height().max(2) - 1) as f32,
    )
}

/// Continuous sample coordinates of world point `(x, z)` on `map`.
pub fn world_to_sample(map: &crate::TerrainMap, x: f32, z: f32) -> Vec2 {
    Vec2::from(map.world_to_grid(x, z))
}

/// One synchronous stroke over the heightfield: a stamp at the first point of `path` (world
/// XZ, metres), smeared through the rest. Flatten targets the height under the first point.
/// Returns the undo record; the map is edited in place.
pub fn stroke_heights(
    map: &mut crate::TerrainMap,
    brush: &Brush,
    path: &[Vec2],
    tile_size: u32,
) -> StrokeRecord<f32> {
    let _span = tracing::info_span!(target: "regia_terrain", "brush.stroke_heights").entered();
    let mut stroke = BrushStroke::new(*brush, tile_size);
    let scale = map_scale(map);
    let value = path.first().map_or(0.0, |p| map.sample_bilinear(p.x, p.y));
    let mut prev: Option<Vec2> = None;
    for p in path {
        let s = world_to_sample(map, p.x, p.y);
        match prev {
            None => stroke.stamp(&*map, s, scale, value),
            Some(start) => stroke.smear(&*map, start, s, scale, value),
        }
        prev = Some(s);
        if stroke.pending() >= PIXEL_BUFFER_SIZE {
            stroke.flush(map);
        }
    }
    stroke.finish_into(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_built_from_two_states_undoes_what_differs() {
        let size = UVec2::new(4, 2);
        let before = GridSnapshot::new(size, vec![0u8; 8]).expect("grid");
        let mut now = before.clone();
        now.write(IVec2::new(1, 0), 5);
        now.write(IVec2::new(2, 1), 9);
        // Outside the rectangle: not part of this stroke.
        now.write(IVec2::new(0, 1), 7);
        let dirty = DirtyRect {
            col_start: 1,
            col_end: 4,
            row_start: 0,
            row_end: 2,
        };
        let record = StrokeRecord::from_states(Brush::default(), &before, &now, dirty, BTreeSet::new());
        assert_eq!(record.samples.len(), 2);
        record.undo(&mut now);
        assert_eq!(now.read(IVec2::new(1, 0)), Some(0));
        assert_eq!(now.read(IVec2::new(2, 1)), Some(0));
        assert_eq!(now.read(IVec2::new(0, 1)), Some(7));
    }

    #[test]
    fn a_handle_without_a_worker_fails_sends_and_ends_waits() {
        let thread = BrushThread::<f32>::without_worker();
        assert!(!thread.sender().end_stroke());
        assert!(thread.wait().is_none());
        assert!(thread.poll().is_empty());
        assert!(thread.poll_closed().1);
    }
    use crate::TerrainMap;

    fn ramp_map() -> TerrainMap {
        let n = 65u32;
        let heights = (0..n * n)
            .map(|i| {
                let (c, r) = (i % n, i / n);
                10.0 + 0.25 * c as f32 + 0.1 * r as f32
            })
            .collect();
        TerrainMap::from_heights(n, n, heights, 64.0, 1.0, 0.0).expect("valid map")
    }

    fn circle(radius: f32, mode: BrushMode) -> Brush {
        Brush {
            shape: BrushShape::Circle { radius },
            mode,
            ..Brush::default()
        }
    }

    #[test]
    fn message_buffer_coalesces_repeats() {
        let mut buffer = PixelMessageBuffer::new(2);
        let at = |x: i32, alpha: f32, value: f32| BrushPixelMessage {
            position: IVec2::new(x, 0),
            alpha,
            value,
        };
        assert!(buffer.push(at(0, 0.3, 1.0)));
        assert!(buffer.push(at(0, 0.7, 2.0)));
        assert!(buffer.push(at(0, 0.5, 3.0)));
        assert!(buffer.push(at(1, 0.1, 0.0)));
        assert!(buffer.is_full());
        assert!(!buffer.push(at(2, 0.1, 0.0)), "a new sample does not fit");
        let first = buffer.pop().expect("queued");
        assert_eq!((first.alpha, first.value), (0.7, 2.0));
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn stroke_data_keeps_the_strongest_coverage() {
        let mut data = StrokeData::<f32>::default();
        let p = IVec2::new(3, 4);
        assert_eq!(data.update_strength(p, 0.5, || Some(7.0)), Some(7.0));
        assert_eq!(data.update_strength(p, 0.4, || Some(99.0)), None);
        assert_eq!(data.update_strength(p, 0.9, || Some(99.0)), Some(7.0));
        assert_eq!(data.update_strength(p, 0.0, || Some(99.0)), None);
        assert_eq!(data.update_strength(IVec2::new(-1, 0), 1.0, || None), None);
        data.set_latest(p, 8.0);
        assert_eq!(data.latest_pixel_value(p), Some(&8.0));
        assert_eq!(data.original_pixel_value(p), Some(&7.0));
    }

    #[test]
    fn raise_then_lower_and_dragging_does_not_accumulate() {
        let mut map = ramp_map();
        let before = map.clone();
        let brush = circle(6.0, BrushMode::Raise { amount: 2.0 });
        // A long drag back and forth over the same spot raises it once (strongest coverage).
        let path = [
            Vec2::ZERO,
            Vec2::new(3.0, 0.0),
            Vec2::ZERO,
            Vec2::new(-3.0, 0.0),
            Vec2::ZERO,
        ];
        stroke_heights(&mut map, &brush, &path, 16);
        let centre = map.sample_bilinear(0.0, 0.0) - before.sample_bilinear(0.0, 0.0);
        assert!(
            (centre - 2.0).abs() < 1e-4,
            "raised by the full amount once: {centre}"
        );
        let lower = circle(6.0, BrushMode::Lower { amount: 2.0 });
        stroke_heights(&mut map, &lower, &[Vec2::ZERO], 16);
        let back = map.sample_bilinear(0.0, 0.0) - before.sample_bilinear(0.0, 0.0);
        assert!(back.abs() < 1e-4, "lowered back: {back}");
        // Outside the brush nothing moved.
        assert_eq!(
            map.sample_bilinear(20.0, 20.0),
            before.sample_bilinear(20.0, 20.0)
        );
    }

    #[test]
    fn hardness_shapes_the_falloff() {
        let soft = {
            let mut map = ramp_map();
            let before = map.clone();
            stroke_heights(
                &mut map,
                &circle(8.0, BrushMode::Raise { amount: 1.0 }),
                &[Vec2::ZERO],
                16,
            );
            map.sample_bilinear(5.0, 0.0) - before.sample_bilinear(5.0, 0.0)
        };
        let hard = {
            let mut map = ramp_map();
            let before = map.clone();
            let brush = Brush {
                hardness: 1.0,
                ..circle(8.0, BrushMode::Raise { amount: 1.0 })
            };
            stroke_heights(&mut map, &brush, &[Vec2::ZERO], 16);
            map.sample_bilinear(5.0, 0.0) - before.sample_bilinear(5.0, 0.0)
        };
        assert!(soft > 0.0 && soft < 0.5, "soft brush is weak off-center: {soft}");
        assert!(
            (hard - 1.0).abs() < 1e-4,
            "hard brush is full strength to the edge: {hard}"
        );
    }

    #[test]
    fn soft_raise_falloff_matches_det_cos_bits() {
        // Flat 0 m map, height_scale 512 (one 16-bit heightfield step ≈ 7.8 mm): a falloff
        // weight one ulp off can straddle a `to_heightfield` rounding boundary, so the stamp
        // must come from the deterministic `det_cos` kernel, not the platform `f32::cos`.
        // The PINS below are literal expected bits — chosen within a few ulps of a cook
        // rounding boundary, so a libm `cos` one ulp off cooks them differently — and the
        // quantised half is read back through the production `TerrainMap::to_heightfield`,
        // not a re-derived formula (Muse review of fix-terrain1-determinism finding 2).
        let n = 33u32;
        let mut map =
            TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], 32.0, 512.0, 0.0).expect("valid map");
        let center = world_to_sample(&map, 0.0, 0.0);
        stroke_heights(
            &mut map,
            &circle(8.0, BrushMode::Raise { amount: 10.0 }),
            &[Vec2::ZERO],
            16,
        );
        let heights = map.heights();
        let field = map.to_heightfield();
        assert_eq!(field.samples.len(), heights.len());
        // Literal pins: (col, row, expected stamped height bits, expected cooked u16).
        // (16,14) and (12,10) sit ~0.03 of a cook step from a `to_heightfield` rounding
        // boundary — a platform libm whose `cos` lands one ulp off cooks them to the
        // neighbouring u16 (glibc's f32 cos agrees with the kernel bit for bit today, so the
        // pins also lock the kernel against any future drift).
        for &(col, row, bits, q) in &[
            (16u32, 16u32, 0x4120_0000u32, 1280u16),
            (16, 14, 0x4108_918c, 1093),
            (12, 10, 0x3e73_bd1c, 30),
            (13, 11, 0x3fda_780a, 218),
            (19, 21, 0x3fda_780a, 218),
        ] {
            let i = (row * n + col) as usize;
            assert_eq!(heights[i].to_bits(), bits, "stamped height at ({col}, {row})");
            assert_eq!(field.samples[i], q, "heightfield sample ({col}, {row})");
        }
        let inv = 65535.0 / map.height_scale();
        let offset = map.height_offset();
        let radius = 8.0; // 8 m at 1 m per sample
        let mut stamped = 0usize;
        for row in 0..n {
            for col in 0..n {
                let i = (row * n + col) as usize;
                let h = heights[i];
                let delta = Vec2::new(col as f32, row as f32) - center;
                let dist_sqr = delta.length_squared();
                let expected: f32 = if dist_sqr >= radius * radius {
                    0.0
                } else {
                    stamped += 1;
                    let strength = (1.0 - dist_sqr.sqrt() / radius).max(0.0);
                    let alpha =
                        0.5 * (1.0 - regia_core::math::det_cos(strength.min(1.0) * std::f32::consts::PI));
                    10.0 * alpha
                };
                assert_eq!(
                    h.to_bits(),
                    expected.to_bits(),
                    "stamped height at sample ({col}, {row})"
                );
                // The production cook quantises exactly what the falloff prescribed.
                let q = ((expected - offset) * inv).round().clamp(0.0, 65535.0) as u16;
                assert_eq!(field.samples[i], q, "heightfield sample ({col}, {row})");
            }
        }
        assert!(stamped > 0, "the stamp wrote nothing");
    }

    #[test]
    fn rectangle_brush_covers_a_rectangle() {
        let mut map = ramp_map();
        let before = map.clone();
        let brush = Brush {
            shape: BrushShape::Rectangle {
                width: 20.0,
                length: 4.0,
            },
            hardness: 1.0,
            ..Brush::default()
        };
        stroke_heights(&mut map, &brush, &[Vec2::ZERO], 16);
        let d = |x: f32, z: f32| map.sample_bilinear(x, z) - before.sample_bilinear(x, z);
        assert!(d(8.0, 0.0) > 0.99, "inside along X");
        assert!(d(0.0, 8.0).abs() < 1e-6, "outside along Z");
    }

    #[test]
    fn flatten_and_assign_reach_their_targets() {
        let mut map = ramp_map();
        let target = map.sample_bilinear(0.0, 0.0);
        let flatten = Brush {
            hardness: 1.0,
            ..circle(6.0, BrushMode::Flatten)
        };
        stroke_heights(&mut map, &flatten, &[Vec2::ZERO], 16);
        for x in [-4.0f32, -2.0, 2.0, 4.0] {
            let h = map.sample_bilinear(x, 0.0);
            assert!((h - target).abs() < 1e-3, "flattened to {target} at {x}: {h}");
        }
        let assign = Brush {
            hardness: 1.0,
            ..circle(6.0, BrushMode::Assign { value: 3.5 })
        };
        stroke_heights(&mut map, &assign, &[Vec2::new(10.0, 10.0)], 16);
        assert!((map.sample_bilinear(10.0, 10.0) - 3.5).abs() < 1e-4);
    }

    #[test]
    fn smooth_kernel_removes_a_spike_and_keeps_a_plane() {
        // A plane is a fixed point of the neighbourhood mean.
        let mut map = ramp_map();
        let before = map.clone();
        let smooth = Brush {
            hardness: 1.0,
            ..circle(6.0, BrushMode::Smooth { kernel_radius: 1 })
        };
        stroke_heights(&mut map, &smooth, &[Vec2::ZERO], 16);
        for (a, b) in map.heights().iter().zip(before.heights()) {
            assert!((a - b).abs() < 1e-4, "plane unchanged: {a} vs {b}");
        }
        // A one-sample spike on flat ground becomes the mean of its 3×3 ring (the ground).
        let n = 33u32;
        let mut heights = vec![5.0f32; (n * n) as usize];
        let c = (n / 2) as usize;
        heights[c * n as usize + c] = 13.0;
        let mut spike = TerrainMap::from_heights(n, n, heights, 32.0, 1.0, 0.0).expect("map");
        stroke_heights(&mut spike, &smooth, &[Vec2::ZERO], 16);
        let h = spike.heights()[c * n as usize + c];
        assert!((h - 5.0).abs() < 1e-4, "spike smoothed to the ground: {h}");
        // Neighbours read the original spike: each moves toward (7·5 + 13)/8 = 6.
        let side = spike.heights()[c * n as usize + c + 1];
        assert!(
            (side - 6.0).abs() < 1e-4,
            "neighbor averaged with the spike: {side}"
        );
    }

    /// Terrain1 correctness audit finding 3: border offsets clamp onto the grid, and a
    /// clamped offset that lands on the stroked sample itself is not one of its neighbours.
    #[test]
    fn smooth_at_the_corner_excludes_the_centre() {
        let n = 17u32;
        let mut heights = vec![0.0f32; (n * n) as usize];
        heights[0] = 100.0;
        let mut map = TerrainMap::from_heights(n, n, heights, 64.0, 1.0, 0.0).expect("map");
        let smooth = Brush {
            hardness: 1.0,
            ..circle(6.0, BrushMode::Smooth { kernel_radius: 1 })
        };
        let half = map.world_width() * 0.5;
        stroke_heights(&mut map, &smooth, &[Vec2::new(-half, -half)], 16);
        let corner = map.heights()[0];
        assert!(
            corner.abs() < 1e-4,
            "the corner smooths toward its neighbours only: {corner}"
        );
    }

    #[test]
    fn layer_weight_values_clamp() {
        assert_eq!(250u8.raise(0.1), 255);
        assert_eq!(5u8.raise(-0.1), 0);
        assert_eq!(0u8.lerp_to(1.0, 0.5), 127);
        assert_eq!(200u8.smooth_to(100.9, 1.0), 100);
    }

    #[test]
    fn stroke_is_deterministic_and_undo_is_bit_exact() {
        let brush = Brush {
            hardness: 0.3,
            alpha: 0.8,
            ..circle(7.5, BrushMode::Raise { amount: 1.7 })
        };
        let path = [
            Vec2::new(-12.0, 3.0),
            Vec2::new(-4.0, 5.5),
            Vec2::new(6.0, -2.0),
            Vec2::new(14.0, 1.0),
        ];
        let mut a = ramp_map();
        let mut b = ramp_map();
        let original = a.clone();
        let ra = stroke_heights(&mut a, &brush, &path, 16);
        let rb = stroke_heights(&mut b, &brush, &path, 16);
        assert_eq!(ra, rb);
        assert!(
            a.heights()
                .iter()
                .zip(b.heights())
                .all(|(x, y)| x.to_bits() == y.to_bits())
        );
        assert!(!ra.is_empty());
        ra.undo(&mut a);
        assert!(
            a.heights()
                .iter()
                .zip(original.heights())
                .all(|(x, y)| x.to_bits() == y.to_bits()),
            "undo restores every height bit for bit"
        );
        ra.redo(&mut a);
        assert!(
            a.heights()
                .iter()
                .zip(b.heights())
                .all(|(x, y)| x.to_bits() == y.to_bits())
        );
    }

    #[test]
    fn threaded_stroke_matches_the_synchronous_one() {
        for falloff in [
            BrushFalloff::Smooth,
            BrushFalloff::Linear,
            BrushFalloff::Sphere,
            BrushFalloff::Tip,
        ] {
            check_threaded_curve(falloff);
        }
    }

    fn check_threaded_curve(falloff: BrushFalloff) {
        let brush = Brush {
            hardness: 0.2,
            falloff,
            ..circle(9.0, BrushMode::Smooth { kernel_radius: 2 })
        };
        let n = 65u32;
        let heights = (0..n * n)
            .map(|i| ((i % 7) as f32 * 1.3 + (i / 11 % 5) as f32 * 0.7).sin() * 3.0)
            .collect();
        let map = TerrainMap::from_heights(n, n, heights, 64.0, 1.0, 0.0).expect("map");
        let path = [Vec2::new(-10.0, -10.0), Vec2::new(0.0, 4.0), Vec2::new(12.0, 2.0)];

        let mut sync_map = map.clone();
        let sync_record = stroke_heights(&mut sync_map, &brush, &path, 16);

        let thread = BrushThread::<f32>::spawn().expect("spawn");
        let mut live = map.clone();
        let snapshot = Arc::new(GridSnapshot::capture(&live));
        assert!(thread.sender().start_stroke(brush, snapshot, 16));
        let scale = map_scale(&live);
        let mut prev: Option<Vec2> = None;
        for p in &path {
            let s = world_to_sample(&live, p.x, p.y);
            let sender = thread.sender();
            match prev {
                None => brush.stamp(s, scale, |q, a| {
                    sender.draw_pixel(q, a, 0.0);
                }),
                Some(start) => brush.smear(start, s, scale, |q, a| {
                    sender.draw_pixel(q, a, 0.0);
                }),
            }
            prev = Some(s);
        }
        assert!(thread.sender().end_stroke());
        let record = loop {
            match thread.wait().expect("worker alive") {
                BrushThreadEvent::Batch(batch) => {
                    batch.apply(&mut live);
                }
                BrushThreadEvent::Finished(record) => break record,
            }
        };
        assert_eq!(record, sync_record);
        assert!(
            live.heights()
                .iter()
                .zip(sync_map.heights())
                .all(|(x, y)| x.to_bits() == y.to_bits())
        );
    }

    #[test]
    fn huge_brush_is_dropped() {
        let brush = circle(1.0e6, BrushMode::Raise { amount: 1.0 });
        let mut count = 0usize;
        brush.stamp(Vec2::ZERO, Vec2::ONE, |_, _| count += 1);
        assert_eq!(count, 0);
    }

    #[test]
    fn mode_inversion_and_shape_scale() {
        assert_eq!(
            BrushMode::Raise { amount: 1.0 }.inverted(),
            BrushMode::Lower { amount: 1.0 }
        );
        assert_eq!(
            BrushMode::Assign { value: 0.25 }.inverted(),
            BrushMode::Assign { value: 0.75 }
        );
        let mut shape = BrushShape::Circle { radius: 4.0 };
        shape.scale(1.5);
        assert_eq!(shape, BrushShape::Circle { radius: 6.0 });
        assert!(shape.contains(Vec2::ZERO, Vec2::new(5.9, 0.0)));
        assert!(!shape.contains(Vec2::ZERO, Vec2::new(6.1, 0.0)));
    }
}
