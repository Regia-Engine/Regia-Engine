// SPDX-License-Identifier: MPL-2.0
//! Seeded 2-D noise for texture synthesis and heightfields: value noise, gradient noise (Perlin
//! 2002's quintic fade over hashed unit gradients), cellular noise (Worley 1996, one jittered
//! feature point per cell: the nearest distance F1 or the edge distance F2 − F1), summed over
//! octaves as fBm or ridged multifractal (Musgrave), optionally tileable.
//!
//! Bit-identical on every platform: lattice values and gradients come from an integer hash
//! (SplitMix64's finalizer), pixel coordinates are exact rationals (an integer cell and an exact
//! fraction), and the arithmetic is `f64` `+ - * /` and `sqrt` only, which IEEE 754 defines
//! exactly. No platform transcendental is used.

use serde::{Deserialize, Serialize};

use crate::{FloatImage, GenError};

/// The most lattice cells across an image in the finest octave (`cells × lacunarity^(octaves−1)`).
pub const MAX_NOISE_CELLS: u64 = 1 << 20;

/// The most octaves.
pub const MAX_OCTAVES: u32 = 16;

/// The kind of base noise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseKind {
    /// Hashed lattice values in `[0, 1)` blended with the quintic fade.
    Value,
    /// Gradient (Perlin) noise: hashed unit gradients, quintic fade, mapped to `[0, 1]`.
    Gradient,
    /// Cellular (Worley) noise: distance to the nearest feature point, in cells, capped at 1.
    Cellular,
    /// Cellular edge noise: second-nearest minus nearest distance (0 on cell borders), capped at 1.
    CellularEdge,
}

/// How octaves combine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fractal {
    /// Fractional Brownian motion: the amplitude-weighted mean of the octaves.
    #[default]
    Fbm,
    /// Ridged multifractal: each octave becomes `(1 − |2v − 1|)²` before the weighted mean, so
    /// the mid values form sharp ridges.
    Ridged,
}

/// Parameters of a noise field. Coordinates are in pixels of a `width × height` image: the
/// first octave has `cells` lattice cells across each axis, each further octave `lacunarity`
/// times more, with `gain` times the amplitude. A non-square image stretches the cells.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoiseParams {
    /// The base noise.
    pub kind: NoiseKind,
    /// The seed (default 0).
    #[serde(default)]
    pub seed: u64,
    /// Lattice cells across the image in the first octave, at least 1 (default 4).
    #[serde(default = "default_cells")]
    pub cells: u32,
    /// Octaves, `1..=16` (default 1).
    #[serde(default = "default_octaves")]
    pub octaves: u32,
    /// How octaves combine (default fBm).
    #[serde(default)]
    pub fractal: Fractal,
    /// Cell multiplier between octaves, `1..=8` (default 2). An integer, so every octave tiles.
    #[serde(default = "default_lacunarity")]
    pub lacunarity: u32,
    /// Amplitude multiplier between octaves, in `(0, 1]` (default 0.5).
    #[serde(default = "default_gain")]
    pub gain: f32,
    /// Wrap the lattice at the image size so the image tiles seamlessly (default false).
    #[serde(default)]
    pub tile: bool,
}

fn default_cells() -> u32 {
    4
}
fn default_octaves() -> u32 {
    1
}
fn default_lacunarity() -> u32 {
    2
}
fn default_gain() -> f32 {
    0.5
}

impl NoiseParams {
    /// Single-octave noise of `kind` with the defaults (seed 0, 4 cells, fBm, lacunarity 2,
    /// gain 0.5, not tiled).
    pub fn new(kind: NoiseKind) -> Self {
        Self {
            kind,
            seed: 0,
            cells: default_cells(),
            octaves: default_octaves(),
            fractal: Fractal::Fbm,
            lacunarity: default_lacunarity(),
            gain: default_gain(),
            tile: false,
        }
    }
}

/// SplitMix64's output function (Vigna, public domain): a 64-bit integer mixer.
pub(crate) fn mix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn hash2(seed: u64, x: i64, y: i64) -> u64 {
    mix64(seed ^ mix64((x as u64) ^ mix64((y as u64).wrapping_add(0x632B_E59B_D9B4_E019))))
}

/// The top 53 bits of `h` as a double in `[0, 1)`.
pub(crate) fn unit(h: u64) -> f64 {
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// Perlin's quintic fade `6t⁵ − 15t⁴ + 10t³`.
fn fade(t: f64) -> f64 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

const D: f64 = std::f64::consts::FRAC_1_SQRT_2;
/// Eight unit gradients: the axes and the diagonals.
const GRADS: [(f64, f64); 8] = [
    (1.0, 0.0),
    (-1.0, 0.0),
    (0.0, 1.0),
    (0.0, -1.0),
    (D, D),
    (-D, D),
    (D, -D),
    (-D, -D),
];

struct Octave {
    cells: i64,
    seed: u64,
    amplitude: f64,
}

/// A validated noise field over a `width × height` image; [`NoiseSampler::sample`] evaluates
/// any integer pixel, including pixels outside the image (a tiled field repeats there).
pub struct NoiseSampler {
    kind: NoiseKind,
    fractal: Fractal,
    tile: bool,
    width: u32,
    height: u32,
    octaves: Vec<Octave>,
    norm: f64,
}

impl NoiseSampler {
    /// Validates `params` for a `width × height` image.
    pub fn new(params: &NoiseParams, width: u32, height: u32) -> Result<Self, GenError> {
        if width == 0 || height == 0 {
            return Err(GenError::InvalidInput(format!(
                "noise size {width}x{height} has a zero side"
            )));
        }
        if !(1..=MAX_OCTAVES).contains(&params.octaves) {
            return Err(GenError::InvalidInput(format!(
                "octaves must be 1..={MAX_OCTAVES}, got {}",
                params.octaves
            )));
        }
        if params.cells == 0 {
            return Err(GenError::InvalidInput("cells must be at least 1".into()));
        }
        if !(1..=8).contains(&params.lacunarity) {
            return Err(GenError::InvalidInput(format!(
                "lacunarity must be 1..=8, got {}",
                params.lacunarity
            )));
        }
        if !(params.gain.is_finite() && params.gain > 0.0 && params.gain <= 1.0) {
            return Err(GenError::InvalidInput(format!(
                "gain must be in (0, 1], got {}",
                params.gain
            )));
        }
        let mut octaves = Vec::with_capacity(params.octaves as usize);
        let mut cells = u64::from(params.cells);
        let mut amplitude = 1.0f64;
        let mut norm = 0.0f64;
        for o in 0..params.octaves {
            if cells > MAX_NOISE_CELLS {
                return Err(GenError::TooLarge {
                    what: "noise cells in the finest octave",
                    value: cells as usize,
                    limit: MAX_NOISE_CELLS as usize,
                });
            }
            octaves.push(Octave {
                cells: cells as i64,
                seed: mix64(params.seed ^ mix64(u64::from(o).wrapping_add(0xA076_1D64_78BD_642F))),
                amplitude,
            });
            norm += amplitude;
            cells = cells.saturating_mul(u64::from(params.lacunarity));
            amplitude *= f64::from(params.gain);
        }
        Ok(Self {
            kind: params.kind,
            fractal: params.fractal,
            tile: params.tile,
            width,
            height,
            octaves,
            norm,
        })
    }

    /// The field at the centre of pixel `(x, y)`, in `[0, 1]`.
    pub fn sample(&self, x: i64, y: i64) -> f32 {
        let mut sum = 0.0f64;
        for o in &self.octaves {
            let (cx, fx) = coord(x, o.cells, self.width);
            let (cy, fy) = coord(y, o.cells, self.height);
            let v = match self.kind {
                NoiseKind::Value => self.value(o, cx, cy, fx, fy),
                NoiseKind::Gradient => self.gradient(o, cx, cy, fx, fy),
                NoiseKind::Cellular | NoiseKind::CellularEdge => self.cellular(o, cx, cy, fx, fy),
            };
            let v = match self.fractal {
                Fractal::Fbm => v,
                Fractal::Ridged => {
                    let r = 1.0 - (2.0 * v - 1.0).abs();
                    r * r
                }
            };
            sum += o.amplitude * v;
        }
        ((sum / self.norm) as f32).clamp(0.0, 1.0)
    }

    fn lattice(&self, o: &Octave, x: i64, y: i64) -> u64 {
        let (x, y) = if self.tile {
            (x.rem_euclid(o.cells), y.rem_euclid(o.cells))
        } else {
            (x, y)
        };
        hash2(o.seed, x, y)
    }

    fn value(&self, o: &Octave, cx: i64, cy: i64, fx: f64, fy: f64) -> f64 {
        let v = |i: i64, j: i64| unit(self.lattice(o, cx + i, cy + j));
        let (sx, sy) = (fade(fx), fade(fy));
        lerp(lerp(v(0, 0), v(1, 0), sx), lerp(v(0, 1), v(1, 1), sx), sy)
    }

    fn gradient(&self, o: &Octave, cx: i64, cy: i64, fx: f64, fy: f64) -> f64 {
        let g = |i: i64, j: i64| {
            let (gx, gy) = GRADS[(self.lattice(o, cx + i, cy + j) >> 61) as usize];
            gx * (fx - i as f64) + gy * (fy - j as f64)
        };
        let (sx, sy) = (fade(fx), fade(fy));
        let n = lerp(lerp(g(0, 0), g(1, 0), sx), lerp(g(0, 1), g(1, 1), sx), sy);
        // Unit-gradient 2-D gradient noise stays within ±√2/2.
        0.5 + n * D
    }

    fn cellular(&self, o: &Octave, cx: i64, cy: i64, fx: f64, fy: f64) -> f64 {
        let (mut f1, mut f2) = (f64::INFINITY, f64::INFINITY);
        for j in -1i64..=1 {
            for i in -1i64..=1 {
                let h = self.lattice(o, cx + i, cy + j);
                let dx = i as f64 + unit(h) - fx;
                let dy = j as f64 + unit(mix64(h)) - fy;
                let d2 = dx * dx + dy * dy;
                if d2 < f1 {
                    f2 = f1;
                    f1 = d2;
                } else if d2 < f2 {
                    f2 = d2;
                }
            }
        }
        let v = match self.kind {
            NoiseKind::CellularEdge => f2.sqrt() - f1.sqrt(),
            _ => f1.sqrt(),
        };
        v.min(1.0)
    }
}

/// The lattice cell and the exact fraction within it of pixel `p`'s centre, with `cells` cells
/// across `size` pixels: the centre is at `(2p + 1) · cells / (2 · size)` cells.
fn coord(p: i64, cells: i64, size: u32) -> (i64, f64) {
    let n = (2 * i128::from(p) + 1) * i128::from(cells);
    let d = 2 * i128::from(size);
    (n.div_euclid(d) as i64, n.rem_euclid(d) as f64 / d as f64)
}

/// A `width × height` image of the noise field.
pub fn noise_image(params: &NoiseParams, width: u32, height: u32) -> Result<FloatImage, GenError> {
    let _span = tracing::trace_span!("regia_gen::noise_image").entered();
    crate::image::check_size(width, height)?;
    let sampler = NoiseSampler::new(params, width, height)?;
    FloatImage::from_fn(width, height, |x, y| sampler.sample(i64::from(x), i64::from(y)))
}
