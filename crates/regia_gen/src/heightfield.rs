// SPDX-License-Identifier: MPL-2.0
//! Heightfield tools over an R32F grid of heights in metres: noise fill, terrace, flatten-to-pad,
//! clamp and normalize, deterministic thermal erosion, slope and normal maps, and export to the
//! engine's terrain heightmap form ([`regia_asset::Heightfield`] plus world width, height scale
//! and offset, what `regia_terrain::Terrain::new` takes) and to a [`CpuMesh`] grid.
//!
//! Layout: sample `(col, row)` sits at `x = col · spacing − (width − 1) · spacing / 2`,
//! `z = row · spacing − (height − 1) · spacing / 2`, `y = height`; row 0 is the −Z edge and
//! column 0 the −X edge, as in `regia_asset::mesh_from_heightfield`.

use regia_asset::CookedImage;
use serde::{Deserialize, Serialize};

use crate::image::{ImageStats, stats_of};
use crate::{CpuMesh, FloatImage, GenError, NoiseParams, NoiseSampler};

/// The largest heightfield side, in samples (the cooked terrain limit,
/// [`regia_asset::HEIGHTFIELD_MAX_EDGE`]).
pub const MAX_HEIGHTFIELD_EDGE: u32 = regia_asset::HEIGHTFIELD_MAX_EDGE;

/// The largest [`Erosion::rate`]: the share of a sample's largest excess one pass may move.
pub const MAX_EROSION_RATE: f32 = 0.25;

/// A grid of heights in metres, row-major, `spacing` metres between samples on both axes.
#[derive(Clone, Debug, PartialEq)]
pub struct Heightfield {
    /// Samples per row, at least 2.
    pub width: u32,
    /// Rows, at least 2.
    pub height: u32,
    /// Metres between neighbouring samples, positive.
    pub spacing: f32,
    /// `width * height` heights in metres.
    pub heights: Vec<f32>,
}

/// A flat circular pad: [`Heightfield::flatten_pad`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pad {
    /// Centre `[x, z]` in metres (the grid is centred on the origin).
    pub center: [f32; 2],
    /// Samples within this distance (metres) are set to `height` exactly.
    pub radius: f32,
    /// Width (metres) of the band outside `radius` that blends back to the terrain (smoothstep).
    #[serde(default)]
    pub falloff: f32,
    /// The pad's height in metres.
    pub height: f32,
}

/// Thermal erosion parameters: [`Heightfield::thermal_erode`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Erosion {
    /// Passes over the whole grid.
    pub iterations: u32,
    /// The talus slope (rise over run, the tangent of the angle of repose), not negative:
    /// material moves only where a neighbour is lower by more than `talus · distance`.
    pub talus: f32,
    /// Fraction of the largest excess moved per pass, in `(0, 0.25]` (default 0.25). The passes
    /// update every sample at once, so a larger share overshoots where several slopes feed one
    /// sample and leaves a two-sample ripple; 0.25 does not.
    #[serde(default = "quarter")]
    pub rate: f32,
}

fn quarter() -> f32 {
    0.25
}

/// What [`Heightfield::thermal_erode`] did.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ErosionReport {
    /// Total height moved between samples over all passes, metres.
    pub moved: f64,
    /// Samples that gave material in the last pass.
    pub active_last_pass: usize,
}

/// The engine's terrain heightmap form of a heightfield: what `regia_terrain::Terrain::new`
/// and [`regia_asset::mesh_from_heightfield`] take.
#[derive(Clone, Debug, PartialEq)]
pub struct TerrainExport {
    /// 16-bit samples: 0 at the lowest height, 65535 at the highest.
    pub heightfield: regia_asset::Heightfield,
    /// The grid's side in metres, `(width − 1) · spacing`.
    pub world_width: f32,
    /// Metres from sample 0 to 65535 (the height range).
    pub height_scale: f32,
    /// Height of sample 0 in metres (the minimum).
    pub height_offset: f32,
}

const SQRT_2: f32 = std::f32::consts::SQRT_2;

/// The 8 neighbours `(dx, dy, distance factor)`, in a fixed order.
const NEIGHBOURS: [(i64, i64, f32); 8] = [
    (-1, -1, SQRT_2),
    (0, -1, 1.0),
    (1, -1, SQRT_2),
    (-1, 0, 1.0),
    (1, 0, 1.0),
    (-1, 1, SQRT_2),
    (0, 1, 1.0),
    (1, 1, SQRT_2),
];

fn check_grid(width: u32, height: u32, spacing: f32) -> Result<usize, GenError> {
    if width < 2 || height < 2 {
        return Err(GenError::InvalidInput(format!(
            "a heightfield needs at least 2x2 samples, got {width}x{height}"
        )));
    }
    if width > MAX_HEIGHTFIELD_EDGE || height > MAX_HEIGHTFIELD_EDGE {
        return Err(GenError::TooLarge {
            what: "heightfield side",
            value: width.max(height) as usize,
            limit: MAX_HEIGHTFIELD_EDGE as usize,
        });
    }
    if !(spacing.is_finite() && spacing > 0.0) {
        return Err(GenError::InvalidInput(format!(
            "spacing must be positive, got {spacing}"
        )));
    }
    Ok(width as usize * height as usize)
}

impl Heightfield {
    /// A flat field at height 0.
    pub fn flat(width: u32, height: u32, spacing: f32) -> Result<Self, GenError> {
        let n = check_grid(width, height, spacing)?;
        Ok(Self {
            width,
            height,
            spacing,
            heights: vec![0.0; n],
        })
    }

    /// A field whose sample `(col, row)` is `f(col, row)`.
    pub fn from_fn(
        width: u32,
        height: u32,
        spacing: f32,
        mut f: impl FnMut(u32, u32) -> f32,
    ) -> Result<Self, GenError> {
        let n = check_grid(width, height, spacing)?;
        let mut heights = Vec::with_capacity(n);
        for row in 0..height {
            for col in 0..width {
                heights.push(f(col, row));
            }
        }
        crate::finite("heights", &heights)?;
        Ok(Self {
            width,
            height,
            spacing,
            heights,
        })
    }

    /// The world `[x, z]` of sample `(col, row)`.
    pub fn sample_position(&self, col: u32, row: u32) -> [f32; 2] {
        let hx = (self.width - 1) as f32 * self.spacing / 2.0;
        let hz = (self.height - 1) as f32 * self.spacing / 2.0;
        [col as f32 * self.spacing - hx, row as f32 * self.spacing - hz]
    }

    /// Minimum, maximum and mean height.
    pub fn stats(&self) -> ImageStats {
        stats_of(&self.heights)
    }

    /// Adds `base + amplitude · noise` to every sample (noise in `[0, 1]`, evaluated at the
    /// sample as a pixel of a `width × height` image).
    pub fn add_noise(&mut self, params: &NoiseParams, amplitude: f32, base: f32) -> Result<(), GenError> {
        let _span = tracing::trace_span!("regia_gen::heightfield_noise").entered();
        crate::finite("noise amplitude and base", &[amplitude, base])?;
        let s = NoiseSampler::new(params, self.width, self.height)?;
        for row in 0..self.height {
            for col in 0..self.width {
                let i = (row * self.width + col) as usize;
                self.heights[i] += base + amplitude * s.sample(i64::from(col), i64::from(row));
            }
        }
        Ok(())
    }

    /// Terraces the field into `steps` (at least 2) flat levels spread evenly from the current
    /// minimum to the maximum: a sample at fraction `t` of the range moves toward level
    /// `min(⌊t · steps⌋, steps − 1)`, by `strength` (`[0, 1]`; 1 snaps onto the level).
    pub fn terrace(&mut self, steps: u32, strength: f32) -> Result<(), GenError> {
        if steps < 2 {
            return Err(GenError::InvalidInput(format!(
                "terrace needs at least 2 steps, got {steps}"
            )));
        }
        if steps > 65_536 {
            return Err(GenError::TooLarge {
                what: "terrace steps",
                value: steps as usize,
                limit: 65_536,
            });
        }
        if !(strength.is_finite() && (0.0..=1.0).contains(&strength)) {
            return Err(GenError::InvalidInput(format!(
                "terrace strength must be in [0, 1], got {strength}"
            )));
        }
        let s = self.stats();
        let range = s.max - s.min;
        if range <= 0.0 {
            return Ok(());
        }
        let top = (steps - 1) as f32;
        for h in &mut self.heights {
            let t = (*h - s.min) / range;
            let level = (t * steps as f32).floor().clamp(0.0, top);
            let target = s.min + range * (level / top);
            *h = if strength == 1.0 {
                target
            } else {
                *h + (target - *h) * strength
            };
        }
        Ok(())
    }

    /// Flattens a circular pad: samples within `radius` of `center` become `height` exactly; in
    /// the `falloff` band beyond, the terrain blends back with a smoothstep; outside, nothing
    /// changes.
    pub fn flatten_pad(&mut self, pad: &Pad) -> Result<(), GenError> {
        crate::finite(
            "pad",
            &[pad.center[0], pad.center[1], pad.radius, pad.falloff, pad.height],
        )?;
        if pad.radius < 0.0 || pad.falloff < 0.0 {
            return Err(GenError::InvalidInput(
                "pad radius and falloff must not be negative".into(),
            ));
        }
        for row in 0..self.height {
            for col in 0..self.width {
                let [x, z] = self.sample_position(col, row);
                let (dx, dz) = (x - pad.center[0], z - pad.center[1]);
                let d = (dx * dx + dz * dz).sqrt();
                let i = (row * self.width + col) as usize;
                if d <= pad.radius {
                    self.heights[i] = pad.height;
                } else if d < pad.radius + pad.falloff {
                    let t = (d - pad.radius) / pad.falloff;
                    let s = t * t * (3.0 - 2.0 * t);
                    self.heights[i] = pad.height + (self.heights[i] - pad.height) * s;
                }
            }
        }
        Ok(())
    }

    /// Clamps every height to `[min, max]`.
    pub fn clamp(&mut self, min: f32, max: f32) -> Result<(), GenError> {
        crate::finite("clamp bounds", &[min, max])?;
        if min > max {
            return Err(GenError::InvalidInput(format!(
                "clamp min {min} is above max {max}"
            )));
        }
        for h in &mut self.heights {
            *h = h.clamp(min, max);
        }
        Ok(())
    }

    /// Maps the current minimum to `min` and maximum to `max` linearly (a flat field becomes
    /// `min`).
    pub fn normalize(&mut self, min: f32, max: f32) -> Result<(), GenError> {
        crate::finite("normalize bounds", &[min, max])?;
        if min > max {
            return Err(GenError::InvalidInput(format!(
                "normalize min {min} is above max {max}"
            )));
        }
        let s = self.stats();
        let range = s.max - s.min;
        for h in &mut self.heights {
            *h = if range > 0.0 {
                min + (max - min) * ((*h - s.min) / range)
            } else {
                min
            };
        }
        Ok(())
    }

    /// Thermal erosion (Musgrave, Kolb and Mace 1989; as given by Olsen 2004): in each pass every
    /// sample compares itself with its 8 neighbours; where a neighbour is lower by more than
    /// `talus · distance`, the sample gives `rate · (largest excess)` in total, shared among those
    /// neighbours in proportion to their excess. Transfers are computed from the pass's starting
    /// heights and applied together (in `f64`), so the result does not depend on visiting order
    /// and the total height is conserved up to rounding. Grid borders are closed.
    pub fn thermal_erode(&mut self, erosion: &Erosion) -> Result<ErosionReport, GenError> {
        let _span = tracing::trace_span!("regia_gen::thermal_erode").entered();
        crate::finite("erosion", &[erosion.talus, erosion.rate])?;
        if erosion.talus < 0.0 {
            return Err(GenError::InvalidInput(
                "erosion talus must not be negative".into(),
            ));
        }
        if !(erosion.rate > 0.0 && erosion.rate <= MAX_EROSION_RATE) {
            return Err(GenError::InvalidInput(format!(
                "erosion rate must be in (0, {MAX_EROSION_RATE}], got {}",
                erosion.rate
            )));
        }
        let (w, h) = (i64::from(self.width), i64::from(self.height));
        let limits = NEIGHBOURS.map(|(_, _, f)| f64::from(erosion.talus * self.spacing * f));
        let rate = f64::from(erosion.rate);
        let mut delta = vec![0.0f64; self.heights.len()];
        let mut moved = 0.0f64;
        let mut active = 0usize;
        for _ in 0..erosion.iterations {
            delta.iter_mut().for_each(|d| *d = 0.0);
            active = 0;
            for y in 0..h {
                for x in 0..w {
                    let i = (y * w + x) as usize;
                    let hi = f64::from(self.heights[i]);
                    let mut excess = [0.0f64; 8];
                    let mut total = 0.0f64;
                    let mut largest = 0.0f64;
                    for (k, &(dx, dy, _)) in NEIGHBOURS.iter().enumerate() {
                        let (nx, ny) = (x + dx, y + dy);
                        if nx < 0 || ny < 0 || nx >= w || ny >= h {
                            continue;
                        }
                        let e = hi - f64::from(self.heights[(ny * w + nx) as usize]) - limits[k];
                        if e > 0.0 {
                            excess[k] = e;
                            total += e;
                            largest = largest.max(e);
                        }
                    }
                    if total <= 0.0 {
                        continue;
                    }
                    active += 1;
                    let amount = rate * largest;
                    moved += amount;
                    for (k, &(dx, dy, _)) in NEIGHBOURS.iter().enumerate() {
                        if excess[k] > 0.0 {
                            let a = amount * excess[k] / total;
                            delta[i] -= a;
                            delta[((y + dy) * w + x + dx) as usize] += a;
                        }
                    }
                }
            }
            if active == 0 {
                break;
            }
            for (hv, d) in self.heights.iter_mut().zip(&delta) {
                *hv = (f64::from(*hv) + d) as f32;
            }
        }
        Ok(ErosionReport {
            moved,
            active_last_pass: active,
        })
    }

    /// The height difference quotients at a sample: central differences inside, one-sided at
    /// the borders, per metre: `(dh/dx, dh/dz)`.
    fn gradient(&self, col: u32, row: u32) -> (f32, f32) {
        let at = |c: u32, r: u32| self.heights[(r * self.width + c) as usize];
        let diff = |lo: u32, hi: u32, a: f32, b: f32| (b - a) / ((hi - lo) as f32 * self.spacing);
        let (c0, c1) = (col.saturating_sub(1), (col + 1).min(self.width - 1));
        let (r0, r1) = (row.saturating_sub(1), (row + 1).min(self.height - 1));
        (
            diff(c0, c1, at(c0, row), at(c1, row)),
            diff(r0, r1, at(col, r0), at(col, r1)),
        )
    }

    /// The slope (rise over run, `|∇h|`) at every sample, as an image the grid's size.
    pub fn slope_map(&self) -> FloatImage {
        let mut pixels = Vec::with_capacity(self.heights.len());
        for row in 0..self.height {
            for col in 0..self.width {
                let (gx, gz) = self.gradient(col, row);
                pixels.push((gx * gx + gz * gz).sqrt());
            }
        }
        FloatImage {
            width: self.width,
            height: self.height,
            pixels,
        }
    }

    /// The unit surface normal at every sample, `normalize(−dh/dx, 1, −dh/dz)` (+Y up).
    pub fn normals(&self) -> Vec<[f32; 3]> {
        let mut out = Vec::with_capacity(self.heights.len());
        for row in 0..self.height {
            for col in 0..self.width {
                let (gx, gz) = self.gradient(col, row);
                out.push(crate::build::normalize([-gx, 1.0, -gz]));
            }
        }
        out
    }

    /// A tangent-space normal map in the renderer's OpenGL (+Y up) convention, with U along +X
    /// and V (image rows, downward) along +Z: R = n·X, G = n·(−Z), B = n·Y, each `n · 0.5 + 0.5`
    /// rounded to 8 bits; A = 255.
    pub fn normal_map(&self) -> CookedImage {
        let mut rgba8 = Vec::with_capacity(self.heights.len() * 4);
        for n in self.normals() {
            let q = |v: f32| crate::quantize_u8(v * 0.5 + 0.5);
            rgba8.extend_from_slice(&[q(n[0]), q(-n[2]), q(n[1]), 255]);
        }
        CookedImage {
            width: self.width,
            height: self.height,
            rgba8,
        }
    }

    /// The engine's terrain heightmap form: 16-bit samples over `[min, max]` plus the world
    /// width, height scale (`max − min`) and offset (`min`); every sample rebuilds within
    /// `height_scale / 65535`. The terrain form is square: a grid with `width != height` is an
    /// error (`gen_invalid_input`).
    pub fn to_terrain(&self) -> Result<TerrainExport, GenError> {
        if self.width != self.height {
            return Err(GenError::InvalidInput(format!(
                "the terrain heightmap is square; this grid is {}x{}",
                self.width, self.height
            )));
        }
        let s = self.stats();
        let scale = s.max - s.min;
        let samples = self
            .heights
            .iter()
            .map(|&h| {
                if scale > 0.0 {
                    crate::quantize_u16((h - s.min) / scale)
                } else {
                    0
                }
            })
            .collect();
        Ok(TerrainExport {
            heightfield: regia_asset::Heightfield {
                width: self.width,
                height: self.height,
                samples,
            },
            world_width: (self.width - 1) as f32 * self.spacing,
            height_scale: scale,
            height_offset: s.min,
        })
    }

    /// The grid as a mesh: one vertex per sample at its world position with UV
    /// `(col / (width − 1), row / (height − 1))` and the [`Self::normals`] normal, two
    /// counter-clockwise triangles per cell seen from above, in `mesh_from_heightfield`'s order.
    pub fn to_mesh(&self) -> Result<CpuMesh, GenError> {
        let n = self.heights.len();
        crate::check_vertices(n)?;
        let mut positions = Vec::with_capacity(n);
        let mut uvs = Vec::with_capacity(n);
        let (cols, rows) = ((self.width - 1) as f32, (self.height - 1) as f32);
        for row in 0..self.height {
            for col in 0..self.width {
                let [x, z] = self.sample_position(col, row);
                positions.push([x, self.heights[(row * self.width + col) as usize], z]);
                uvs.push([col as f32 / cols, row as f32 / rows]);
            }
        }
        let w = self.width;
        let mut indices = Vec::with_capacity((self.width as usize - 1) * (self.height as usize - 1) * 6);
        for row in 0..self.height - 1 {
            for col in 0..self.width - 1 {
                let v00 = row * w + col;
                let (v01, v10) = (v00 + w, v00 + 1);
                let v11 = v01 + 1;
                indices.extend_from_slice(&[v00, v01, v10, v10, v01, v11]);
            }
        }
        Ok(CpuMesh {
            positions,
            normals: self.normals(),
            uvs,
            indices,
            ..Default::default()
        })
    }

    /// A BLAKE3 hash (hex) of the size, spacing and height bits; tests pin it across platforms.
    pub fn hash(&self) -> String {
        let mut h = blake3::Hasher::new();
        h.update(b"regia_gen.heightfield.v1");
        h.update(&self.width.to_le_bytes());
        h.update(&self.height.to_le_bytes());
        h.update(&self.spacing.to_bits().to_le_bytes());
        for v in &self.heights {
            h.update(&v.to_bits().to_le_bytes());
        }
        h.finalize().to_hex().to_string()
    }
}
