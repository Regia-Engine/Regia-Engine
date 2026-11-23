// SPDX-License-Identifier: MPL-2.0
//! The photoreal terrain material: per-layer texture sets (albedo / tangent-space normal /
//! height), whole-map macro maps (albedo + AO, world-space normal gradient) and the tiled
//! streaming plan for 8K bakes.
//!
//! Textures are generated procedurally here (`LayerStyle::bake`, `MacroBake::from_heightfield`)
//! so labs, tests and the evidence shots have a CC0-equivalent stand-in for DO27's CC0 layered
//! materials; the same `TerrainMaterial` consumed by [`crate::gpu::TerrainGpu::set_material`]
//! accepts images loaded from files (the v2 `LayerDesc` texture slots name them), because the
//! images are plain RGBA8 with no pipeline of their own.
//!
//! Everything is deterministic per seed. The layer textures tile seamlessly: every generator
//! samples lattice value noise with coordinates wrapped to the tile period, so texel (0, y)
//! and texel (size, y) evaluate the same field (verified in the tests).

use crate::TerrainMap;
use crate::noise::hash2;

/// A tightly packed RGBA8 image, row-major, top row first.
#[derive(Clone, Debug)]
pub struct Rgba8Image {
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// RGBA8 bytes, `width * height * 4`.
    pub data: Vec<u8>,
}

/// Mean normal-map tilt, in byte levels (of 127.5 per unit slope), under which
/// [`Rgba8Image::recentred_normal`] leaves a normal map alone (2 levels = 0.016 slope, under 1°).
pub const NORMAL_RECENTRE_MIN: i32 = 2;

/// Largest byte length `Rgba8Image::try_solid` allocates.
const MAX_SOLID_BYTES: u64 = 1 << 30;

impl Rgba8Image {
    /// A solid-colour image, or `None` when `width * height * 4` bytes exceed
    /// `MAX_SOLID_BYTES` (1 GiB) or do not fit `usize`.
    pub fn try_solid(width: u32, height: u32, rgba: [u8; 4]) -> Option<Self> {
        let texels = u64::from(width) * u64::from(height);
        if texels.checked_mul(4)? > MAX_SOLID_BYTES {
            return None;
        }
        Some(Self {
            width,
            height,
            data: rgba.repeat(usize::try_from(texels).ok()?),
        })
    }

    /// A solid-colour image. A request above the `try_solid` size limit yields an empty
    /// 0×0 image instead of aborting on the allocation.
    pub fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Self {
        Self::try_solid(width, height, rgba).unwrap_or(Self {
            width: 0,
            height: 0,
            data: Vec::new(),
        })
    }

    /// Whether `data` holds exactly `width * height * 4` bytes (checked in `u128`: with `u32`
    /// dimensions `u64` overflows at the `* 4`, so huge dimensions cannot wrap or panic).
    pub fn data_len_matches(&self) -> bool {
        u128::from(self.width) * u128::from(self.height) * 4 == self.data.len() as u128
    }

    /// Mean of a tangent-space normal map's X and Y bytes minus the flat value 128, rounded to
    /// whole byte levels. A tiling normal map is zero-mean (`[0, 0]`); a bias is a constant tilt
    /// that the anti-tiling rotation turns into a different tilt in every lattice cell
    /// (ADR-1428).
    pub fn normal_mean_shift(&self) -> [i32; 2] {
        let texels = u64::from(self.width) * u64::from(self.height);
        if texels == 0 || !self.data_len_matches() {
            return [0, 0];
        }
        let (mut sum_x, mut sum_y) = (0u64, 0u64);
        for px in self.data.as_chunks::<4>().0 {
            sum_x += u64::from(px[0]);
            sum_y += u64::from(px[1]);
        }
        let shift = |sum: u64| ((sum as f64 / texels as f64) - 128.0).round() as i32;
        [shift(sum_x), shift(sum_y)]
    }

    /// This normal map with its mean tilt removed ([`Self::normal_mean_shift`] subtracted from
    /// X and Y, clamped to a byte), or `None` when the mean is within `NORMAL_RECENTRE_MIN`
    /// levels of flat (the map is left untouched, byte for byte).
    pub fn recentred_normal(&self) -> Option<Self> {
        let [dx, dy] = self.normal_mean_shift();
        if dx.abs() < NORMAL_RECENTRE_MIN && dy.abs() < NORMAL_RECENTRE_MIN {
            return None;
        }
        let mut out = self.clone();
        for px in out.data.as_chunks_mut::<4>().0 {
            px[0] = (i32::from(px[0]) - dx).clamp(0, 255) as u8;
            px[1] = (i32::from(px[1]) - dy).clamp(0, 255) as u8;
        }
        Some(out)
    }

    /// Neutral albedo (opaque white).
    pub fn neutral_albedo(size: u32) -> Self {
        Self::solid(size, size, [255, 255, 255, 255])
    }

    /// Neutral tangent-space normal (flat, +Z) with mid height.
    pub fn neutral_normal(size: u32) -> Self {
        Self::solid(size, size, [128, 128, 255, 255])
    }

    /// Neutral height (mid grey).
    pub fn neutral_height(size: u32) -> Self {
        Self::solid(size, size, [128, 128, 128, 255])
    }

    /// The texel as an `[f32; 4]` in `0..=1` (test helper).
    #[cfg(test)]
    fn texel01(&self, x: u32, y: u32) -> [f32; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [
            self.data[i] as f32 / 255.0,
            self.data[i + 1] as f32 / 255.0,
            self.data[i + 2] as f32 / 255.0,
            self.data[i + 3] as f32 / 255.0,
        ]
    }

    /// Largest per-channel jump across the horizontal wrap (`column size−1 → column 0`)
    /// compared with the largest interior adjacent-column jump. A seamless tile's wrap jump
    /// is just another adjacent-pair difference (± byte rounding); a visible seam spikes
    /// above every interior jump.
    pub fn wrap_jump_ratio(&self) -> f32 {
        let w = self.width as usize;
        let h = self.height as usize;
        let mut interior = 0i32;
        for y in 0..h {
            for x in 0..w - 1 {
                for c in 0..4 {
                    let a = self.data[(y * w + x) * 4 + c] as i32;
                    let b = self.data[(y * w + x + 1) * 4 + c] as i32;
                    interior = interior.max((a - b).abs());
                }
            }
        }
        let mut wrap = 0i32;
        for y in 0..h {
            for c in 0..4 {
                let a = self.data[(y * w + w - 1) * 4 + c] as i32;
                let b = self.data[(y * w) * 4 + c] as i32;
                wrap = wrap.max((a - b).abs());
            }
        }
        wrap as f32 / interior.max(1) as f32
    }
}

/// Per-layer texture set: albedo (sRGB bytes, sampled through a linear decode), tangent-space
/// normal, height (R channel) for height-blended transitions, and an optional glTF ORM map.
#[derive(Clone, Debug)]
pub struct LayerTextures {
    /// Tiling albedo, RGBA8 (alpha unused, 255).
    pub albedo: Rgba8Image,
    /// Tiling tangent-space normal, RGBA8 (`xy*2-1`, z reconstructed).
    pub normal: Rgba8Image,
    /// Tiling height map, RGBA8 (R = height 0..=1).
    pub height: Rgba8Image,
    /// glTF ORM, linear RGBA8: R occlusion (of indirect light only, as glTF 2.0 defines it),
    /// G roughness, B metallic (Khronos glTF 2.0).
    /// Sampled only when [`Self::use_orm`] is set; otherwise the scalar layer roughness
    /// and full occlusion are used, which is the v2 look.
    pub orm: Rgba8Image,
    /// When true, the shader reads [`Self::orm`] for occlusion and roughness.
    pub use_orm: bool,
}

/// Layer styles the procedural CC0-style baker knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerStyle {
    /// Meadow grass: green blotches, fine speckle.
    Grass,
    /// Banded sedimentary rock with cracks.
    Rock,
    /// Dry dirt with clumps and pebbles.
    Dirt,
    /// Snow with subtle dunes and sparkles.
    Snow,
    /// Beach sand with wind ripples.
    Sand,
    /// Glacier / ice-cliff ice: bright cool white with fracture lines and faint layering.
    Ice,
}

/// Encodes a linear colour channel as an sRGB byte (IEC 61966-2-1 transfer function).
/// Albedo arrays are `Rgba8UnormSrgb`; the hardware applies the inverse on sample.
#[must_use]
pub fn linear_to_srgb8(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let s = if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0).round().clamp(0.0, 255.0) as u8
}

/// Decodes an sRGB byte to a linear channel value (inverse of [`linear_to_srgb8`]).
#[must_use]
pub fn srgb8_to_linear(b: u8) -> f32 {
    let s = f32::from(b) / 255.0;
    if s <= 0.040_45 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

/// Tileable lattice value noise on the unit square: `u`/`v` in `0..1`, lattice coordinates
/// wrapped to `period` so the field repeats exactly at the tile border.
fn tile_noise(u: f32, v: f32, period: u32, seed: u32) -> f32 {
    let x = u * period as f32;
    let y = v * period as f32;
    let xi = x.floor() as i32;
    let yi = y.floor() as i32;
    let fx = x - xi as f32;
    let fy = y - yi as f32;
    let fade = |t: f32| t * t * t * (t * (t * 6.0 - 15.0) + 10.0);
    let wrap = |i: i32| i.rem_euclid(period as i32);
    let v00 = hash2(wrap(xi), wrap(yi), seed);
    let v10 = hash2(wrap(xi + 1), wrap(yi), seed);
    let v01 = hash2(wrap(xi), wrap(yi + 1), seed);
    let v11 = hash2(wrap(xi + 1), wrap(yi + 1), seed);
    let a = v00 + (v10 - v00) * fade(fx);
    let b = v01 + (v11 - v01) * fade(fx);
    a + (b - a) * fade(fy)
}

/// Tileable fBm: octaves double the (wrapped) frequency.
fn tile_fbm(u: f32, v: f32, base_period: u32, octaves: u32, seed: u32) -> f32 {
    let mut sum = 0.0;
    let mut norm = 0.0;
    for octave in 0..octaves {
        let period = base_period << octave.min(4);
        sum += tile_noise(u, v, period, seed.wrapping_add(octave * 131)) / (octave as f32 + 1.0);
        norm += 1.0 / (octave as f32 + 1.0);
    }
    sum / norm
}

/// Encodes a tangent-space normal map (RGBA8) from a tileable height field sampled per texel.
///
/// `height_at` receives unit-square coordinates; the gradient is taken with wrapped finite
/// differences (texel centres, so texel `size` continues texel 0 exactly) and the encoded map
/// tiles with the height. `strength` scales the slope: the normal is
/// `normalize(−∂h/∂u · strength, −∂h/∂v · strength, 1)`.
fn normal_from_height<F: Fn(f32, f32) -> f32>(size: u32, strength: f32, height_at: F) -> Rgba8Image {
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    let texel = 1.0 / size as f32;
    for y in 0..size {
        for x in 0..size {
            let u = (x as f32 + 0.5) * texel;
            let v = (y as f32 + 0.5) * texel;
            let hx = height_at(u + texel, v) - height_at(u - texel, v);
            let hy = height_at(u, v + texel) - height_at(u, v - texel);
            let n = glam::Vec3::new(-hx * strength, -hy * strength, 1.0).normalize();
            data.extend_from_slice(&[
                ((n.x * 0.5 + 0.5) * 255.0).round().clamp(0.0, 255.0) as u8,
                ((n.y * 0.5 + 0.5) * 255.0).round().clamp(0.0, 255.0) as u8,
                ((n.z * 0.5 + 0.5) * 255.0).round().clamp(0.0, 255.0) as u8,
                255,
            ]);
        }
    }
    Rgba8Image {
        width: size,
        height: size,
        data,
    }
}

impl LayerStyle {
    /// Bakes a tileable CC0-style texture set at `size`×`size` (power of two, ≥ 64).
    ///
    /// # Panics
    ///
    /// Panics in tests/labs only — never called on user content (see [`TerrainMaterial`]).
    pub fn bake(self, size: u32, seed: u32) -> LayerTextures {
        let texel = 1.0 / size as f32;
        let mut albedo = Vec::with_capacity((size * size * 4) as usize);
        let mut height = Vec::with_capacity((size * size * 4) as usize);
        for y in 0..size {
            for x in 0..size {
                let u = (x as f32 + 0.5) * texel;
                let v = (y as f32 + 0.5) * texel;
                let (rgb, h) = self.colour_height(u, v, seed);
                // Albedo is uploaded as `Rgba8UnormSrgb` (like any CC0 albedo map), so the
                // linear colour is sRGB-encoded here. Storing linear bytes made the sampler
                // decode them a second time: grass at linear 0.23 came back as 0.04.
                albedo.extend(
                    rgb.iter()
                        .map(|&c| linear_to_srgb8(c))
                        .chain(std::iter::once(255u8)),
                );
                let hb = (h * 255.0).round().clamp(0.0, 255.0) as u8;
                height.extend_from_slice(&[hb, hb, hb, 255]);
            }
        }
        let height_img = Rgba8Image {
            width: size,
            height: size,
            data: height,
        };
        // Sample the same height field for the normal so cracks and clumps catch the light.
        // The z scale keeps the encoded tilt moderate: mean z stays in 0.6..0.98 (tested).
        let slope = match self {
            LayerStyle::Rock => 14.0,
            LayerStyle::Sand | LayerStyle::Snow => 4.0,
            LayerStyle::Ice => 9.0,
            _ => 7.0,
        };
        let normal = normal_from_height(size, slope, |u, v| self.colour_height(u, v, seed).1);
        // Neutral ORM (AO 1, roughness 1, metal 0). `use_orm` stays off so the scalar
        // roughness on the layer desc keeps the previous look until a caller opts in.
        let orm = Rgba8Image::solid(size, size, [255, 255, 0, 255]);
        LayerTextures {
            albedo: Rgba8Image {
                width: size,
                height: size,
                data: albedo,
            },
            normal,
            height: height_img,
            orm,
            use_orm: false,
        }
    }

    /// Linear RGB colour and height in `0..=1` at unit-square coordinates.
    fn colour_height(self, u: f32, v: f32, seed: u32) -> ([f32; 3], f32) {
        match self {
            LayerStyle::Grass => {
                let blotch = tile_fbm(u, v, 8, 3, seed);
                let speckle = tile_noise(u, v, 64, seed ^ 0x51f3);
                let blade = tile_fbm(u, v, 48, 2, seed ^ 0x9c2a);
                let g = 0.55 + 0.65 * blotch + 0.18 * speckle * blade;
                let rgb = [0.10 * g * 1.25, 0.23 * g, 0.07 * g * 0.8];
                (rgb, 0.45 + 0.35 * blotch + 0.2 * blade * speckle)
            }
            LayerStyle::Rock => {
                let warp = tile_fbm(u, v, 4, 2, seed ^ 0x77aa);
                // Integer wave counts per tile keep the strata exactly periodic.
                let strata = ((v + warp * 0.28) * 4.0 * std::f32::consts::TAU).sin() * 0.5 + 0.5;
                let rough = tile_fbm(u, v, 16, 3, seed);
                let crack = 1.0 - (tile_fbm(u, v, 12, 2, seed ^ 0x3117) - 0.5).abs() * 4.4;
                let crack = crack.clamp(0.0, 1.0);
                // Linear albedo ~0.12–0.30: weathered granite/sandstone, not chalk.
                let l = (0.18 + 0.15 * strata * rough + 0.09 * rough) * (1.0 - 0.55 * crack);
                let rgb = [l * 1.06, l * 1.0, l * 0.93];
                (rgb, (0.5 + 0.5 * strata * rough - 0.6 * crack).clamp(0.0, 1.0))
            }
            LayerStyle::Dirt => {
                let clump = tile_fbm(u, v, 10, 3, seed);
                let pebble = tile_noise(u, v, 40, seed ^ 0x66b1);
                let grain = tile_noise(u, v, 96, seed ^ 0x2244);
                // Linear albedo ~0.15–0.30 (dry soil), a warm brown rather than ochre.
                let l = 0.17 + 0.13 * clump + 0.06 * grain;
                let rgb = [l * 1.25, l * 1.0, l * 0.75];
                (rgb, (0.4 + 0.4 * clump + 0.25 * pebble).clamp(0.0, 1.0))
            }
            LayerStyle::Snow => {
                let dune = tile_fbm(u, v, 6, 3, seed);
                let sparkle = tile_noise(u, v, 90, seed ^ 0x0af5).powi(6);
                let l = (0.82 + 0.13 * dune + 0.5 * sparkle).min(1.0);
                let rgb = [l * 0.97, l * 0.985, l];
                (rgb, 0.55 + 0.3 * dune)
            }
            LayerStyle::Sand => {
                let warp = tile_fbm(u, v, 4, 2, seed ^ 0x1c3e);
                // Nine ripple wavelengths per tile (integer → exactly periodic).
                let ripple = ((u + warp * 0.22) * 9.0 * std::f32::consts::TAU).sin() * 0.5 + 0.5;
                let grain = tile_noise(u, v, 80, seed ^ 0x4411);
                let l = 0.60 + 0.12 * ripple + 0.08 * grain;
                let rgb = [l * 1.12, l * 1.0, l * 0.78];
                (rgb, 0.4 + 0.45 * ripple)
            }
            LayerStyle::Ice => {
                let warp = tile_fbm(u, v, 4, 2, seed ^ 0x2e1c);
                // Faint annual layering (integer count per tile keeps it periodic).
                let layering = ((v + warp * 0.18) * 6.0 * std::f32::consts::TAU).sin() * 0.5 + 0.5;
                let body = tile_fbm(u, v, 8, 3, seed);
                let frac = 1.0 - (tile_fbm(u, v, 10, 2, seed ^ 0x7a31) - 0.5).abs() * 6.0;
                let frac = frac.clamp(0.0, 1.0);
                // Linear albedo ~0.45–0.75: wind-polished ice, darker in the fractures.
                let l = (0.56 + 0.14 * body + 0.06 * layering) * (1.0 - 0.38 * frac);
                let rgb = [l * 0.92, l * 0.97, l * 1.02];
                (rgb, (0.55 + 0.3 * body - 0.5 * frac).clamp(0.0, 1.0))
            }
        }
    }
}

/// Knobs of the photoreal material path (all live in one uniform on the GPU).
#[derive(Clone, Copy, Debug)]
pub struct MaterialParams {
    /// World metres covered by the macro set; `0` disables macro variation.
    pub macro_world_size: f32,
    /// Macro albedo/AO blend amount, `0..=1`.
    pub macro_strength: f32,
    /// Macro world-normal perturb strength, `0..=1`.
    pub macro_normal_strength: f32,
    /// Anti-tiling cell size in metres; `0` disables stochastic tiling.
    pub anti_tiling_cell: f32,
    /// Distance where micro-normal detail starts fading, metres.
    pub detail_fade_start: f32,
    /// Distance where micro-normal detail has fully faded, metres.
    pub detail_fade_end: f32,
    /// Slope (radians from up) above which layers switch to triplanar automatically.
    pub triplanar_auto_slope: f32,
}

impl Default for MaterialParams {
    fn default() -> Self {
        Self {
            macro_world_size: 0.0,
            macro_strength: 0.6,
            macro_normal_strength: 0.25,
            anti_tiling_cell: 4.0,
            detail_fade_start: 60.0,
            detail_fade_end: 220.0,
            triplanar_auto_slope: 0.55,
        }
    }
}

/// A terrain's full photoreal material: per-layer texture sets plus macro maps.
///
/// All layer textures must share one size (the GPU packs them into equal array layers);
/// the layer count is capped at [`crate::layers::MAX_LAYERS`].
#[derive(Clone, Debug)]
pub struct TerrainMaterial {
    /// One texture set per layer, bottom first (`None` slots fall back to flat colour).
    pub layers: Vec<Option<LayerTextures>>,
    /// Whole-map macro variation (albedo+AO and world-normal tiles).
    pub macro_set: Option<MacroBake>,
    /// Knobs.
    pub params: MaterialParams,
}

impl TerrainMaterial {
    /// The evidence/lab set: grass / rock / dirt / snow, `size`² textures, no macro.
    pub fn procedural_cc0(size: u32, seed: u32, params: MaterialParams) -> Self {
        let styles = [
            LayerStyle::Grass,
            LayerStyle::Rock,
            LayerStyle::Dirt,
            LayerStyle::Snow,
        ];
        Self {
            layers: styles.iter().map(|&s| Some(s.bake(size, seed))).collect(),
            macro_set: None,
            params,
        }
    }

    /// Validates sizes and ranges.
    ///
    /// # Errors
    ///
    /// Returns [`MaterialError`] when layer sizes differ, a dimension is zero / not a power of
    /// two, or a parameter is out of range.
    pub fn validate(&self) -> Result<(), MaterialError> {
        let mut size: Option<u32> = None;
        for (index, slot) in self.layers.iter().enumerate() {
            let Some(set) = slot else { continue };
            for image in [&set.albedo, &set.normal, &set.height, &set.orm] {
                if image.width == 0 || image.height == 0 || !image.width.is_power_of_two() {
                    return Err(MaterialError::BadLayerSize {
                        index,
                        width: image.width,
                    });
                }
                if image.width != image.height {
                    return Err(MaterialError::NotSquare {
                        index,
                        width: image.width,
                        height: image.height,
                    });
                }
                if !image.data_len_matches() {
                    return Err(MaterialError::BadDataLen { index });
                }
                match size {
                    None => size = Some(image.width),
                    Some(s) if s == image.width => {}
                    Some(s) => {
                        return Err(MaterialError::MixedSizes {
                            index,
                            first: s,
                            width: image.width,
                        });
                    }
                }
            }
        }
        if self.layers.len() > crate::layers::MAX_LAYERS {
            return Err(MaterialError::TooManyLayers {
                count: self.layers.len(),
                max: crate::layers::MAX_LAYERS,
            });
        }
        let p = &self.params;
        for value in [p.macro_strength, p.macro_normal_strength] {
            if !(0.0..=1.0).contains(&value) || !value.is_finite() {
                return Err(MaterialError::BadParameter("macro strength", value));
            }
        }
        if p.anti_tiling_cell < 0.0 || !p.anti_tiling_cell.is_finite() {
            return Err(MaterialError::BadParameter(
                "anti_tiling_cell",
                p.anti_tiling_cell,
            ));
        }
        if !(0.0..=20000.0).contains(&p.detail_fade_start) || !(0.0..=20000.0).contains(&p.detail_fade_end) {
            return Err(MaterialError::BadParameter("detail fade", p.detail_fade_start));
        }
        if p.detail_fade_end < p.detail_fade_start {
            return Err(MaterialError::BadParameter("detail_fade_end", p.detail_fade_end));
        }
        if p.triplanar_auto_slope < 0.0 || p.triplanar_auto_slope >= std::f32::consts::FRAC_PI_2 {
            return Err(MaterialError::BadParameter(
                "triplanar_auto_slope",
                p.triplanar_auto_slope,
            ));
        }
        if let Some(m) = &self.macro_set {
            m.validate()?;
        }
        Ok(())
    }
}

/// Whole-map macro variation baked from the heightfield: per-tile albedo (+AO in alpha) and a
/// world-space normal gradient (XY = downhill direction encoded `*0.5+0.5`), plus a coarse
/// whole-map fallback layer used when a tile is not yet streamed in.
///
/// The tiles form a `tiles × tiles` grid covering `world_size` metres. Each tile carries its
/// own mip chain once cooked (KTX2/BCn through `regia_asset_pipeline`), which is what keeps
/// an 8K bake within budget: only the tiles near the camera need resident mips.
#[derive(Clone, Debug)]
pub struct MacroBake {
    /// World metres covered by the whole grid.
    pub world_size: f32,
    /// Grid resolution (tiles per side).
    pub tiles: u32,
    /// Texels per tile edge (power of two).
    pub tile_size: u32,
    /// `tiles*tiles` albedo tiles, row-major, alpha = AO.
    pub albedo: Vec<Rgba8Image>,
    /// `tiles*tiles` world-normal tiles (R = world −X gradient, G = world −Z gradient).
    pub normal: Vec<Rgba8Image>,
    /// Coarse whole-map albedo fallback (`tile_size`² downsampled bake).
    pub coarse_albedo: Rgba8Image,
    /// Coarse whole-map normal fallback.
    pub coarse_normal: Rgba8Image,
}

/// Largest macro bake side in texels (`tiles * tile_size`); the 8K path fits with margin.
const MAX_BAKE_SIDE: u32 = 16384;

/// Most macro tiles per grid side. The residency mask (one bit per tile, ADR-3367) travels to
/// the shader as eight `u32` words (256 bits) with their running resident counts, so a 16 × 16
/// grid keeps residency per tile; a resident grid of 256 tiles is also the WebGPU minimum of
/// texture array layers.
pub const MAX_MACRO_TILES_PER_SIDE: u32 = 16;

/// Words of the macro residency mask: `MAX_MACRO_TILES_PER_SIDE²` bits.
pub const MACRO_RESIDENCY_WORDS: usize = (MAX_MACRO_TILES_PER_SIDE * MAX_MACRO_TILES_PER_SIDE / 32) as usize;

/// The shader's macro residency for the resident tiles `resident` (flat indices
/// `x + y · tiles`; any order, duplicates and indices past the grid ignored): bit `i` of the
/// mask is set when tile `i` is resident, and `prefix[w]` counts the resident tiles in the
/// words before `w`. Resident tile `i` sits at layer `prefix[i / 32] + popcount(mask[i / 32] &
/// ((1 << i % 32) − 1))` of the tile array, which is its rank in [`macro_upload_order`].
#[must_use]
pub fn macro_residency(resident: &[u32]) -> ([u32; MACRO_RESIDENCY_WORDS], [u32; MACRO_RESIDENCY_WORDS]) {
    let mut mask = [0u32; MACRO_RESIDENCY_WORDS];
    for &i in resident {
        if let Some(word) = mask.get_mut((i / 32) as usize) {
            *word |= 1 << (i % 32);
        }
    }
    let mut prefix = [0u32; MACRO_RESIDENCY_WORDS];
    let mut running = 0;
    for (p, m) in prefix.iter_mut().zip(mask) {
        *p = running;
        running += m.count_ones();
    }
    (mask, prefix)
}

/// The order the resident macro tiles are uploaded into the tile array: ascending flat index,
/// each once, only tiles of a `tiles × tiles` grid. The shader finds a tile's layer by its rank
/// in the residency mask ([`macro_residency`]), so the upload must follow this order.
#[must_use]
pub fn macro_upload_order(resident: &[u32], tiles: u32) -> Vec<u32> {
    let count = tiles.saturating_mul(tiles);
    let mut plan: Vec<u32> = resident.iter().copied().filter(|&i| i < count).collect();
    plan.sort_unstable();
    plan.dedup();
    plan
}

impl MacroBake {
    /// Bakes macro tiles from `map`: heightfield normals at texel resolution (so distant LOD
    /// levels keep per-fragment shading), large-scale colour variation, and concavity AO.
    ///
    /// # Errors
    ///
    /// Returns [`MaterialError`] for bad dimensions.
    pub fn from_heightfield(
        map: &TerrainMap,
        tiles: u32,
        tile_size: u32,
        seed: u32,
    ) -> Result<Self, MaterialError> {
        if tiles == 0
            || tiles > MAX_MACRO_TILES_PER_SIDE
            || !tiles.is_power_of_two()
            || tile_size < 8
            || !tile_size.is_power_of_two()
        {
            return Err(MaterialError::BadTileGrid { tiles, tile_size });
        }
        let world = map.world_width();
        let half = world * 0.5;
        let Some(total) = tiles.checked_mul(tile_size).filter(|&t| t <= MAX_BAKE_SIDE) else {
            return Err(MaterialError::BadTileGrid { tiles, tile_size });
        };
        let texel_world = world / total as f32;
        // Sample the whole bake at once, then cut tiles out of it.
        let mut heights = vec![0.0f32; (total * total) as usize];
        for ty in 0..total {
            for tx in 0..total {
                let x = -half + (tx as f32 + 0.5) * texel_world;
                let z = -half + (ty as f32 + 0.5) * texel_world;
                heights[(ty * total + tx) as usize] = map.sample_bilinear(x, z);
            }
        }
        let grad = |tx: u32, ty: u32| -> (f32, f32) {
            let l = heights[(ty * total + tx.min(total - 1).saturating_sub(1)) as usize];
            let r = heights[(ty * total + (tx + 1).min(total - 1)) as usize];
            let t = heights[(ty.min(total - 1).saturating_sub(1) * total + tx) as usize];
            let b = heights[((ty + 1).min(total - 1) * total + tx) as usize];
            // Downhill direction (matches the shading normal's −gradient), world XZ.
            (l - r, t - b)
        };
        let blur = box_blur(&heights, total, 24);
        let mut albedo_tiles = Vec::with_capacity((tiles * tiles) as usize);
        let mut normal_tiles = Vec::with_capacity((tiles * tiles) as usize);
        for tz in 0..tiles {
            for txx in 0..tiles {
                let mut albedo = Vec::with_capacity((tile_size * tile_size * 4) as usize);
                let mut normal = Vec::with_capacity((tile_size * tile_size * 4) as usize);
                for py in 0..tile_size {
                    for px in 0..tile_size {
                        let tx = txx * tile_size + px;
                        let ty = tz * tile_size + py;
                        let h = heights[(ty * total + tx) as usize];
                        let (gx, gz) = grad(tx, ty);
                        // Colour variation: warm/cool low-frequency drift plus an altitude tint.
                        let var = tile_fbm(tx as f32 / total as f32, ty as f32 / total as f32, 16, 3, seed);
                        let h01 = (h - map.height_offset()) / map.height_scale().max(1.0);
                        // A relative tint about 1 (the shader divides by the map mean): broad
                        // ±20 % drift, the variation the AO used to supply now that AO only
                        // occludes indirect light.
                        let tint = 0.62 + 0.75 * var + 0.06 * h01;
                        let rgb = [tint * 1.02, tint, tint * (0.97 - 0.06 * h01)];
                        // Concavity AO: valleys (below their blurred surround) darken.
                        let concavity = (blur[(ty * total + tx) as usize] - h) / (texel_world * 48.0);
                        let ao = (1.0 - concavity.clamp(0.0, 1.0) * 0.55).max(0.35);
                        // RGB is sampled through an sRGB view (decoded to linear); alpha
                        // (AO) is linear in any RGBA8 sRGB format.
                        albedo.extend(
                            rgb.iter()
                                .map(|&c| linear_to_srgb8(c))
                                .chain(std::iter::once((ao * 255.0).round().clamp(0.0, 255.0) as u8)),
                        );
                        let enc = |g: f32| ((g.clamp(-1.0, 1.0) * 0.5 + 0.5) * 255.0).round() as u8;
                        // R follows the geometric normal's X convention (h_l − h_r): the
                        // shader adds the decoded delta straight onto that normal.
                        normal.extend_from_slice(&[
                            enc(gx.clamp(-1.0, 1.0) / (texel_world * 8.0)),
                            enc(gz.clamp(-1.0, 1.0) / (texel_world * 8.0)),
                            128,
                            255,
                        ]);
                    }
                }
                albedo_tiles.push(Rgba8Image {
                    width: tile_size,
                    height: tile_size,
                    data: albedo,
                });
                normal_tiles.push(Rgba8Image {
                    width: tile_size,
                    height: tile_size,
                    data: normal,
                });
            }
        }
        // Coarse whole-map fallback, sampled from the full-resolution field with a stride.
        let stride = (tile_size / 2).max(1);
        let coarse = total / stride;
        let mut albedo_data = Vec::with_capacity((coarse * coarse * 4) as usize);
        let mut normal_data = Vec::with_capacity((coarse * coarse * 4) as usize);
        for cy in 0..coarse {
            for cx in 0..coarse {
                let tx = (cx * stride).min(total - 1);
                let ty = (cy * stride).min(total - 1);
                let h = heights[(ty * total + tx) as usize];
                let (gx, gz) = grad(tx, ty);
                let var = tile_fbm(tx as f32 / total as f32, ty as f32 / total as f32, 16, 3, seed);
                let h01 = (h - map.height_offset()) / map.height_scale().max(1.0);
                let tint = 0.62 + 0.75 * var + 0.06 * h01;
                let rgb = [tint * 1.02, tint, tint * (0.97 - 0.06 * h01)];
                let concavity = (blur[(ty * total + tx) as usize] - h) / (texel_world * 48.0);
                let ao = (1.0 - concavity.clamp(0.0, 1.0) * 0.55).max(0.35);
                albedo_data.extend(
                    rgb.iter()
                        .map(|&c| linear_to_srgb8(c))
                        .chain(std::iter::once((ao * 255.0).round().clamp(0.0, 255.0) as u8)),
                );
                let enc = |g: f32| ((g.clamp(-1.0, 1.0) * 0.5 + 0.5) * 255.0).round() as u8;
                normal_data.extend_from_slice(&[
                    enc(gx.clamp(-1.0, 1.0) / (texel_world * 8.0)),
                    enc(gz.clamp(-1.0, 1.0) / (texel_world * 8.0)),
                    128,
                    255,
                ]);
            }
        }
        Ok(Self {
            world_size: world,
            tiles,
            tile_size,
            albedo: albedo_tiles,
            normal: normal_tiles,
            coarse_albedo: Rgba8Image {
                width: coarse,
                height: coarse,
                data: albedo_data,
            },
            coarse_normal: Rgba8Image {
                width: coarse,
                height: coarse,
                data: normal_data,
            },
        })
    }

    /// Validates the grid shapes.
    ///
    /// # Errors
    ///
    /// Returns [`MaterialError`] when the tile counts or dimensions do not add up.
    pub fn validate(&self) -> Result<(), MaterialError> {
        let grid_ok = self.tiles != 0
            && self.tiles <= MAX_MACRO_TILES_PER_SIDE
            && self.tiles.is_power_of_two()
            && self.tile_size >= 8
            && self.tile_size.is_power_of_two()
            && self
                .tiles
                .checked_mul(self.tile_size)
                .is_some_and(|t| t <= MAX_BAKE_SIDE);
        if !grid_ok {
            return Err(MaterialError::BadTileGrid {
                tiles: self.tiles,
                tile_size: self.tile_size,
            });
        }
        let expected = (self.tiles * self.tiles) as usize;
        if self.albedo.len() != expected || self.normal.len() != expected {
            return Err(MaterialError::BadTileCount {
                albedo: self.albedo.len(),
                normal: self.normal.len(),
                expected,
            });
        }
        for image in self.albedo.iter().chain(self.normal.iter()) {
            if image.width != self.tile_size || image.height != self.tile_size {
                return Err(MaterialError::BadTileSize {
                    width: image.width,
                    expected: self.tile_size,
                });
            }
        }
        for image in self
            .albedo
            .iter()
            .chain(self.normal.iter())
            .chain([&self.coarse_albedo, &self.coarse_normal])
        {
            if image.width == 0 || image.height == 0 || !image.data_len_matches() {
                return Err(MaterialError::BadTileSize {
                    width: image.width,
                    expected: self.tile_size,
                });
            }
        }
        if !self.world_size.is_finite() || self.world_size <= 0.0 {
            return Err(MaterialError::BadParameter("world_size", self.world_size));
        }
        Ok(())
    }

    /// Distance-priority streaming plan: which tile indices to keep resident so the tiles
    /// nearest the camera upload first within `budget_bytes` (RGBA8 accounting, mips ×4/3).
    ///
    /// Returns tile indices ordered by priority (centre distance, then row for determinism);
    /// the caller uploads as many as fit the budget. The coarse fallback layer always stays
    /// resident and is not part of the plan.
    pub fn streaming_plan(&self, camera_xz: [f32; 2], budget_bytes: u64) -> Vec<u32> {
        let tile_world = self.world_size / self.tiles as f32;
        let half = self.world_size * 0.5;
        let centre = |i: u32| {
            let (tx, tz) = (i % self.tiles, i / self.tiles);
            let cx = -half + (tx as f32 + 0.5) * tile_world;
            let cz = -half + (tz as f32 + 0.5) * tile_world;
            ((cx - camera_xz[0]).powi(2) + (cz - camera_xz[1]).powi(2), i)
        };
        let mut order: Vec<u32> = (0..self.tiles * self.tiles).collect();
        order.sort_by(|&a, &b| {
            let (da, ia) = centre(a);
            let (db, ib) = centre(b);
            da.total_cmp(&db).then(ia.cmp(&ib))
        });
        let tile_bytes = (self.tile_size * self.tile_size * 4) as f64 * (4.0 / 3.0); // + mip chain
        let fit = ((budget_bytes as f64 / tile_bytes).floor() as u32).clamp(1, order.len() as u32);
        order.truncate(fit as usize);
        order
    }
}

/// Separable box blur of a square field with clamped borders (border texels repeated, macro AO).
fn box_blur(field: &[f32], n: u32, radius: u32) -> Vec<f32> {
    let mut tmp = vec![0.0f32; field.len()];
    let mut out = vec![0.0f32; field.len()];
    let window = (radius * 2 + 1) as f32;
    for y in 0..n {
        // Seed with the full clamped window: indices `-radius..=-1` repeat the border texel.
        let mut acc = field[(y * n) as usize] * radius as f32;
        for dx in 0..=radius {
            acc += field[(y * n + dx.min(n - 1)) as usize];
        }
        for x in 0..n {
            tmp[(y * n + x) as usize] = acc / window;
            let add = field[(y * n + (x + radius + 1).min(n - 1)) as usize];
            let sub = field[(y * n + x.saturating_sub(radius)) as usize];
            acc += add - sub;
        }
    }
    for x in 0..n {
        // Same clamped seed on columns (row 0's border texels repeat into `-radius..=-1`).
        let mut acc = tmp[x as usize] * radius as f32;
        for dy in 0..=radius {
            acc += tmp[(dy.min(n - 1) * n + x) as usize];
        }
        for y in 0..n {
            out[(y * n + x) as usize] = acc / window;
            let add = tmp[((y + radius + 1).min(n - 1) * n + x) as usize];
            let sub = tmp[(y.saturating_sub(radius) * n + x) as usize];
            acc += add - sub;
        }
    }
    out
}

/// Builds a full mip chain (level 0 = `image`, each level halved, box filter, RGBA8).
///
/// The chain is the CPU mirror of what the texture cook produces (KTX2 mip chains); the GPU
/// samples it with explicit LOD so distance filtering matches the cooked path.
pub fn mip_chain(image: &Rgba8Image) -> Vec<Rgba8Image> {
    let mut mips = vec![image.clone()];
    while mips.last().is_some_and(|m| m.width > 1 && m.height > 1) {
        let last = mips.last().expect("checked");
        let w = last.width.div_ceil(2);
        let h = last.height.div_ceil(2);
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let (x2, y2) = ((x * 2).min(last.width - 1), (y * 2).min(last.height - 1));
                let idx = |xx: u32, yy: u32| ((yy * last.width + xx) * 4) as usize;
                for c in 0..4 {
                    let sum = last.data[idx(x2, y2) + c] as u32
                        + last.data[idx((x2 + 1).min(last.width - 1), y2) + c] as u32
                        + last.data[idx(x2, (y2 + 1).min(last.height - 1)) + c] as u32
                        + last.data[idx((x2 + 1).min(last.width - 1), (y2 + 1).min(last.height - 1)) + c]
                            as u32;
                    data.push(((sum + 2) / 4) as u8);
                }
            }
        }
        mips.push(Rgba8Image {
            width: w,
            height: h,
            data,
        });
    }
    mips
}

/// A photoreal material cooked to KTX2/BCn blobs (one blob per texture, produced by
/// `regia_asset_pipeline::terrain_cook`). The GPU side uploads these directly — block
/// compression is sampled by the hardware, never decoded on the CPU.
///
/// Slot alignment mirrors [`TerrainMaterial::layers`]; the macro tile vectors follow the
/// [`MacroBake`] grid order.
///
/// `B` is the blob type (ADR-8024): `Vec<u8>` (the default) for a material the host cooked in
/// memory, `regia_asset::PakPayload` for one read from a pak
/// (`crate::stream::PakTileSource::load_cooked_material`), whose blobs are views of the mapped
/// pak, or `&[u8]` for a borrowed view ([`Self::view`]). The GPU upload takes any of them.
#[derive(Clone, Debug, Default)]
pub struct CookedMaterialKtx2<B = Vec<u8>> {
    /// KTX2 albedo blobs (BC1/BC3/BC7 sRGB), one per layer slot.
    pub layer_albedo: Vec<B>,
    /// KTX2 tangent-space normal blobs (BC5), one per layer slot.
    pub layer_normal: Vec<B>,
    /// KTX2 height blobs (BC4, R channel), one per layer slot.
    pub layer_height: Vec<B>,
    /// KTX2 glTF ORM blobs (BC7 linear: R occlusion, G roughness, B metallic).
    /// Empty disables the ORM sample and keeps the scalar roughness.
    pub layer_orm: Vec<B>,
    /// KTX2 macro albedo tiles (BC1/BC3/BC7 sRGB, AO in alpha when BC3/BC7).
    pub macro_albedo_tiles: Vec<B>,
    /// KTX2 macro world-normal tiles (BC5).
    pub macro_normal_tiles: Vec<B>,
    /// KTX2 coarse whole-map albedo fallback.
    pub macro_coarse_albedo: Option<B>,
    /// KTX2 coarse whole-map normal fallback.
    pub macro_coarse_normal: Option<B>,
}

impl<B: AsRef<[u8]>> CookedMaterialKtx2<B> {
    /// The same material as borrowed slices of these blobs: only the slot vectors are
    /// allocated (one pointer per blob), never a blob's bytes.
    #[must_use]
    pub fn view(&self) -> CookedMaterialKtx2<&[u8]> {
        fn all<B: AsRef<[u8]>>(blobs: &[B]) -> Vec<&[u8]> {
            blobs.iter().map(AsRef::as_ref).collect()
        }
        CookedMaterialKtx2 {
            layer_albedo: all(&self.layer_albedo),
            layer_normal: all(&self.layer_normal),
            layer_height: all(&self.layer_height),
            layer_orm: all(&self.layer_orm),
            macro_albedo_tiles: all(&self.macro_albedo_tiles),
            macro_normal_tiles: all(&self.macro_normal_tiles),
            macro_coarse_albedo: self.macro_coarse_albedo.as_ref().map(AsRef::as_ref),
            macro_coarse_normal: self.macro_coarse_normal.as_ref().map(AsRef::as_ref),
        }
    }

    /// Bytes of every blob together.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        let view = self.view();
        let lists = [
            &view.layer_albedo,
            &view.layer_normal,
            &view.layer_height,
            &view.layer_orm,
            &view.macro_albedo_tiles,
            &view.macro_normal_tiles,
        ];
        let listed: usize = lists.iter().flat_map(|list| list.iter()).map(|b| b.len()).sum();
        let coarse =
            view.macro_coarse_albedo.map_or(0, <[u8]>::len) + view.macro_coarse_normal.map_or(0, <[u8]>::len);
        (listed + coarse) as u64
    }
}

/// Errors returned by the photoreal material.
#[derive(Clone, Debug, thiserror::Error)]
pub enum MaterialError {
    /// A layer texture has a zero or non-power-of-two size.
    #[error("layer {index} texture width {width} must be a positive power of two")]
    BadLayerSize {
        /// Offending layer.
        index: usize,
        /// Offending width.
        width: u32,
    },
    /// A layer texture is not square.
    #[error("layer {index} texture is {width}x{height}, must be square")]
    NotSquare {
        /// Offending layer.
        index: usize,
        /// Width.
        width: u32,
        /// Height.
        height: u32,
    },
    /// The data buffer does not match the dimensions.
    #[error("layer {index} texture data length does not match its dimensions")]
    BadDataLen {
        /// Offending layer.
        index: usize,
    },
    /// Layer textures of different sizes cannot share one array.
    #[error("layer {index} is {width}² but earlier layers were {first}²")]
    MixedSizes {
        /// Offending layer.
        index: usize,
        /// First size seen.
        first: u32,
        /// Offending size.
        width: u32,
    },
    /// Too many layers.
    #[error("{count} texture sets exceeds the maximum of {max}")]
    TooManyLayers {
        /// Count found.
        count: usize,
        /// Maximum allowed.
        max: usize,
    },
    /// A material parameter is out of range.
    #[error("material parameter {0} = {1} is out of range")]
    BadParameter(&'static str, f32),
    /// The tile grid or tile size is invalid.
    #[error("macro tile grid {tiles}x{tile_size} is invalid (both powers of two, tile >= 8)")]
    BadTileGrid {
        /// Tiles per side.
        tiles: u32,
        /// Texels per tile edge.
        tile_size: u32,
    },
    /// The tile vector lengths do not match the grid.
    #[error("macro tile count mismatch: {albedo} albedo / {normal} normal, expected {expected}")]
    BadTileCount {
        /// Albedo tiles found.
        albedo: usize,
        /// Normal tiles found.
        normal: usize,
        /// Expected count.
        expected: usize,
    },
    /// A tile has the wrong size.
    #[error("macro tile is {width}², expected {expected}²")]
    BadTileSize {
        /// Found size.
        width: u32,
        /// Expected size.
        expected: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::Heightfield;

    fn small_map() -> TerrainMap {
        let n = 64u32;
        let mut heights = Vec::with_capacity((n * n) as usize);
        for row in 0..n {
            for col in 0..n {
                let x = col as f32 / n as f32 * 64.0 - 32.0;
                let z = row as f32 / n as f32 * 64.0 - 32.0;
                heights.push(20.0 * (-(x * x + z * z) / 500.0).exp());
            }
        }
        TerrainMap::from_heights(n, n, heights, 64.0, 24.0, -2.0).expect("map")
    }

    #[test]
    fn solid_refuses_sizes_that_overflow_or_exceed_the_cap() {
        assert!(Rgba8Image::try_solid(1 << 31, 1 << 31, [0, 0, 0, 255]).is_none());
        assert!(Rgba8Image::try_solid(65536, 65536, [0, 0, 0, 255]).is_none());
        let img = Rgba8Image::solid(u32::MAX, u32::MAX, [1, 2, 3, 4]);
        assert_eq!((img.width, img.height, img.data.len()), (0, 0, 0));
        let ok = Rgba8Image::solid(2, 3, [1, 2, 3, 4]);
        assert_eq!(ok.data.len(), 24);
    }

    #[test]
    fn generated_layers_tile_seamlessly() {
        for style in [
            LayerStyle::Grass,
            LayerStyle::Rock,
            LayerStyle::Dirt,
            LayerStyle::Snow,
            LayerStyle::Sand,
            LayerStyle::Ice,
        ] {
            let set = style.bake(128, 7);
            // The wrap jump (last → first column) must not exceed the interior jumps (± byte
            // rounding): no discontinuity spike where the tile repeats.
            let ratio = set.albedo.wrap_jump_ratio();
            assert!(ratio <= 1.2, "{style:?} albedo seams: wrap/interior = {ratio}");
            let ratio = set.height.wrap_jump_ratio();
            assert!(ratio <= 1.2, "{style:?} height seams: wrap/interior = {ratio}");
            let ratio = set.normal.wrap_jump_ratio();
            assert!(ratio <= 1.2, "{style:?} normal seams: wrap/interior = {ratio}");
            // Vertical wrap too.
            let flipped = {
                let mut f = set.albedo.clone();
                f.data = set
                    .albedo
                    .data
                    .chunks((set.albedo.width * 4) as usize)
                    .rev()
                    .flatten()
                    .copied()
                    .collect();
                f
            };
            assert!(flipped.wrap_jump_ratio() < 6.0, "{style:?} vertical seams");
        }
    }

    /// The baked albedo is sRGB-encoded: decoding the bytes the way the `Rgba8UnormSrgb`
    /// array does returns the linear colour the style asked for. Linear bytes (round 3)
    /// decode to a fraction of it — grass came back ~5x too dark.
    #[test]
    fn layer_albedo_bytes_are_srgb_encoded() {
        for style in [LayerStyle::Grass, LayerStyle::Rock, LayerStyle::Dirt] {
            let size = 64u32;
            let set = style.bake(size, 7);
            let mut want = 0.0f64;
            let mut got = 0.0f64;
            for y in 0..size {
                for x in 0..size {
                    let u = (x as f32 + 0.5) / size as f32;
                    let v = (y as f32 + 0.5) / size as f32;
                    let (rgb, _) = style.colour_height(u, v, 7);
                    let i = ((y * size + x) * 4) as usize;
                    for (c, &linear) in rgb.iter().enumerate() {
                        want += f64::from(linear);
                        got += f64::from(srgb8_to_linear(set.albedo.data[i + c]));
                    }
                }
            }
            let ratio = got / want;
            assert!(
                (0.97..1.03).contains(&ratio),
                "{style:?}: decoded albedo / linear color = {ratio:.3}"
            );
        }
    }

    #[test]
    fn bakes_are_deterministic_per_seed() {
        let a = LayerStyle::Rock.bake(64, 9);
        let b = LayerStyle::Rock.bake(64, 9);
        let c = LayerStyle::Rock.bake(64, 10);
        assert_eq!(a.albedo.data, b.albedo.data);
        assert_ne!(a.albedo.data, c.albedo.data);
    }

    #[test]
    fn normals_encode_moderate_detail() {
        let set = LayerStyle::Grass.bake(96, 3);
        // Flat areas decode near (0,0,1); bumpy areas tilt. Mean z stays readable detail.
        let mut sum_z = 0.0f32;
        let mut count = 0u32;
        for y in 0..set.normal.height {
            for x in 0..set.normal.width {
                let t = set.normal.texel01(x, y);
                let v = glam::Vec3::new(t[0] * 2.0 - 1.0, t[1] * 2.0 - 1.0, t[2] * 2.0 - 1.0);
                sum_z += v.normalize_or_zero().z;
                count += 1;
            }
        }
        let mean_z = sum_z / count as f32;
        assert!(mean_z > 0.55, "mean normal z {mean_z} — normals too steep");
        assert!(mean_z < 0.999, "mean normal z {mean_z} — normals flat, no detail");
    }

    #[test]
    fn material_validation_rejects_mixed_sizes() {
        let mut material = TerrainMaterial::procedural_cc0(64, 1, MaterialParams::default());
        assert!(material.validate().is_ok());
        material.layers[0] = Some(LayerStyle::Rock.bake(128, 1));
        assert!(matches!(
            material.validate(),
            Err(MaterialError::MixedSizes { .. })
        ));
        let mut bad = TerrainMaterial::procedural_cc0(64, 1, MaterialParams::default());
        bad.params.detail_fade_end = 1.0; // < start
        assert!(matches!(
            bad.validate(),
            Err(MaterialError::BadParameter("detail_fade_end", _))
        ));
    }

    #[test]
    fn data_len_check_survives_maximal_dimensions() {
        let image = Rgba8Image {
            width: u32::MAX,
            height: u32::MAX,
            data: Vec::new(),
        };
        assert!(!image.data_len_matches());
        assert!(Rgba8Image::solid(2, 3, [1, 2, 3, 4]).data_len_matches());
    }

    #[test]
    fn macro_bake_rejects_grids_the_residency_mask_cannot_hold() {
        let map = small_map();
        // 32 x 32 = 1024 tiles: past the 256-bit residency mask (ADR-3367).
        assert!(matches!(
            MacroBake::from_heightfield(&map, 2 * MAX_MACRO_TILES_PER_SIDE, 8, 1),
            Err(MaterialError::BadTileGrid { .. })
        ));
        let mut bake = MacroBake::from_heightfield(&map, 2, 8, 1).expect("2x2 bake");
        bake.tiles = 2 * MAX_MACRO_TILES_PER_SIDE;
        assert!(matches!(bake.validate(), Err(MaterialError::BadTileGrid { .. })));
        // 8 x 8 and 16 x 16 now fit (the old f32 mask lane stopped at 4 x 4).
        assert!(MacroBake::from_heightfield(&map, 8, 8, 1).is_ok());
        let full = MacroBake::from_heightfield(&map, MAX_MACRO_TILES_PER_SIDE, 8, 1).expect("16x16");
        assert!(full.validate().is_ok());
        assert_eq!(full.albedo.len(), 256);
    }

    #[test]
    fn macro_residency_ranks_every_tile_of_a_sixteen_grid() {
        // Every tile but (5, 9) of a 16 x 16 grid, out of order and with a duplicate.
        let missing = 9 * 16 + 5;
        let mut resident: Vec<u32> = (0..256).rev().filter(|&i| i != missing).collect();
        resident.push(3);
        resident.push(999);
        let plan = macro_upload_order(&resident, 16);
        assert_eq!(plan.len(), 255);
        assert!(plan.windows(2).all(|w| w[0] < w[1]));
        let (mask, prefix) = macro_residency(&plan);
        assert_eq!(mask.iter().map(|m| m.count_ones()).sum::<u32>(), 255);
        let layer_of = |i: u32| -> Option<u32> {
            let (w, b) = ((i / 32) as usize, i % 32);
            (mask[w] & (1 << b) != 0).then(|| prefix[w] + (mask[w] & ((1u32 << b) - 1)).count_ones())
        };
        assert_eq!(layer_of(missing), None);
        for (layer, &tile) in plan.iter().enumerate() {
            assert_eq!(layer_of(tile), Some(layer as u32), "tile {tile}");
        }
        // The neighbours of the missing tile keep their own layers.
        for n in [missing - 1, missing + 1, missing - 16, missing + 16] {
            assert!(layer_of(n).is_some(), "neighbour {n}");
        }
        // A 4 x 4 grid packs as the old single mask did (bit i = tile i, layer = rank).
        let (small, small_prefix) = macro_residency(&[0, 5, 15]);
        assert_eq!(small[0], 1 | 1 << 5 | 1 << 15);
        assert_eq!(small_prefix, [0, 3, 3, 3, 3, 3, 3, 3]);
    }

    #[test]
    fn macro_bake_rejects_wrapping_grid() {
        let map = small_map();
        // 256 * 256 = 65536 per side: the old u32 `total * total` wrapped to 0.
        for (tiles, tile_size) in [(256, 256), (64, 1024), (1024, 64)] {
            assert!(matches!(
                MacroBake::from_heightfield(&map, tiles, tile_size, 1),
                Err(MaterialError::BadTileGrid { .. })
            ));
        }
    }

    /// Naive reference: the 2D box average of the window `± radius` with every index clamped
    /// to the field (border texels repeated), computed as a straight sum.
    fn reference_box_blur(field: &[f32], n: u32, radius: u32) -> Vec<f32> {
        let n = n as i64;
        let radius = radius as i64;
        let at = |x: i64, y: i64| field[(y.clamp(0, n - 1) * n + x.clamp(0, n - 1)) as usize];
        let mut out = vec![0.0f32; field.len()];
        for y in 0..n {
            for x in 0..n {
                let mut acc = 0.0f64;
                for dy in -radius..=radius {
                    for dx in -radius..=radius {
                        acc += f64::from(at(x + dx, y + dy));
                    }
                }
                let window = ((radius * 2 + 1) * (radius * 2 + 1)) as f64;
                out[(y * n + x) as usize] = (acc / window) as f32;
            }
        }
        out
    }

    #[test]
    fn box_blur_keeps_a_constant_field_and_matches_a_clamped_box() {
        // A constant field must come back unchanged (the macro AO flat-ground invariant).
        let n = 8u32;
        let constant = vec![7.5f32; (n * n) as usize];
        let blurred = box_blur(&constant, n, 24);
        assert!(
            blurred.iter().all(|&v| (v - 7.5).abs() < 1e-3),
            "constant field must blur to itself, got {blurred:?}"
        );
        // An impulse against the −X border (and one against the −Z border, after the
        // vertical pass) must stay non-negative and match the reference clamped box:
        // a non-negative field can never average negative.
        for radius in [1u32, 3] {
            for index in [0usize, 2] {
                // 0: the −X/−Z corner (both passes under-seeded); 2: the −Z border away from
                // −X (the vertical pass's own seed, which a −X-only field would hide).
                let mut field = vec![0.0f32; 25];
                field[index] = 10.0;
                let blurred = box_blur(&field, 5, radius);
                assert!(
                    blurred.iter().all(|&v| v >= -1e-6),
                    "radius {radius}: box blur of a non-negative field went negative: {blurred:?}"
                );
                let reference = reference_box_blur(&field, 5, radius);
                for (i, (&got, &want)) in blurred.iter().zip(&reference).enumerate() {
                    assert!(
                        (got - want).abs() < 1e-3,
                        "radius {radius}, texel {i}: got {got}, reference clamped box {want}"
                    );
                }
            }
        }
    }

    #[test]
    fn macro_bake_darkens_a_valley_below_a_high_border() {
        // A 16 × 16 heightfield, 64 m wide: a 600 m plateau on −X, a flat 0 m valley on +X.
        // The valley floor next to the plateau sits far below its true local average (the
        // plateau dominates the ±24-texel window), so the concavity AO must darken it. The
        // under-seeded blur (radius extra border copies missing) sat `radius/window` of the
        // border height too low near the border and baked the valley fully lit.
        let n = 16u32;
        let world = 64.0f32;
        let heights = (0..n * n).map(|i| if i % n < 4 { 600.0 } else { 0.0 }).collect();
        let map = TerrainMap::from_heights(n, n, heights, world, 600.0, 0.0).expect("map");
        let bake = MacroBake::from_heightfield(&map, 1, 16, 7).expect("bake");
        let tile = &bake.albedo[0];
        assert_eq!((tile.width, tile.height), (16, 16));
        // Valley floor texel well inside the +X half: unoccluded would be 255.
        let i = ((8 * 16 + 10) * 4) as usize;
        assert!(
            tile.data[i + 3] < 200,
            "valley below a high border baked AO {}, expected heavy concavity occlusion",
            tile.data[i + 3]
        );
        // Control: a constant field has zero concavity everywhere and must stay at 255.
        let flat = vec![3.0f32; (n * n) as usize];
        let flat_map = TerrainMap::from_heights(n, n, flat, world, 600.0, 0.0).expect("map");
        let flat_bake = MacroBake::from_heightfield(&flat_map, 1, 16, 7).expect("bake");
        assert!(
            flat_bake.albedo[0]
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .all(|px| px[3] == 255),
            "flat ground must bake no AO"
        );
    }

    #[test]
    fn macro_bake_validate_rejects_empty_or_degenerate_images() {
        let map = small_map();
        let mut bake = MacroBake::from_heightfield(&map, 1, 8, 1).expect("bake");
        assert!(bake.validate().is_ok());
        bake.albedo[0].data.clear();
        assert!(bake.validate().is_err());
        let mut zero = MacroBake::from_heightfield(&map, 1, 8, 1).expect("bake");
        zero.tile_size = 0;
        assert!(matches!(zero.validate(), Err(MaterialError::BadTileGrid { .. })));
    }

    #[test]
    fn rgba8_data_len_check_does_not_wrap() {
        let image = Rgba8Image {
            width: 32768,
            height: 32768,
            data: Vec::new(),
        };
        // 32768 * 32768 * 4 wraps to 0 in u32.
        assert!(!image.data_len_matches());
        assert!(Rgba8Image::solid(4, 4, [1, 2, 3, 4]).data_len_matches());
    }

    #[test]
    fn macro_bake_shapes_and_streaming() {
        let map = small_map();
        let bake = MacroBake::from_heightfield(&map, 2, 64, 5).expect("bake");
        assert_eq!(bake.albedo.len(), 4);
        assert_eq!(bake.normal.len(), 4);
        assert!(bake.validate().is_ok());
        // AO and variation are populated (not all-equal tiles).
        let distinct: std::collections::HashSet<u8> = bake.albedo[0].data.iter().copied().collect();
        assert!(distinct.len() > 8, "macro tile has no variation");

        // Streaming: a budget of ~2 tiles keeps the two nearest the camera.
        let tile_bytes = (64u64 * 64 * 4) * 4 / 3;
        let near = bake.streaming_plan([20.0, 20.0], tile_bytes * 2 + 8);
        assert_eq!(near.len(), 2, "budget fits two tiles: {near:?}");
        // Camera in the +X/+Z quadrant: tile 3 is that quadrant, then the adjacent 1 and 2.
        assert_eq!(near[0], 3, "nearest tile first: {near:?}");
        assert!(
            near.contains(&1) || near.contains(&2),
            "then an adjacent tile: {near:?}"
        );
        let far = bake.streaming_plan([0.0, 0.0], u64::MAX / 8);
        assert_eq!(far.len(), 4, "a huge budget keeps everything");
    }

    #[test]
    fn macro_bake_matches_the_heightfield() {
        // The baked normal gradient must follow the geometric normal convention (x ∝ h_l − h_r):
        // rising toward +X encodes R < 0.5, rising toward −X encodes R > 0.5.
        let bake_for = |rise_per_col: f32| {
            let n = 64u32;
            let mut heights = Vec::with_capacity((n * n) as usize);
            for _row in 0..n {
                for col in 0..n {
                    heights.push(col as f32 * rise_per_col);
                }
            }
            let map = TerrainMap::from_heights(n, n, heights, 63.0, 128.0, 0.0).expect("map");
            MacroBake::from_heightfield(&map, 1, 64, 2).expect("bake")
        };
        let mid = (64 / 2 * 64 + 64 / 2) as usize * 4;
        let uphill_pos_x = bake_for(2.0).normal[0].data[mid] as f32 / 255.0;
        assert!(
            uphill_pos_x < 0.5,
            "uphill toward +X must encode R < 0.5 (h_l − h_r), got {uphill_pos_x}"
        );
        let uphill_neg_x = bake_for(-2.0).normal[0].data[mid] as f32 / 255.0;
        assert!(
            uphill_neg_x > 0.5,
            "uphill toward −X must encode R > 0.5 (h_l − h_r), got {uphill_neg_x}"
        );
    }

    #[test]
    fn heightfield_consumed_as_u16_source_of_truth() {
        // The baker must work from a cooked heightfield too (the DO27 path).
        let n = 32u32;
        let samples: Vec<u16> = (0..n * n).map(|i| (i * 97 % 65536) as u16).collect();
        let map = TerrainMap::new(
            &Heightfield {
                width: n,
                height: n,
                samples,
            },
            100.0,
            40.0,
            -5.0,
        )
        .expect("map");
        let bake = MacroBake::from_heightfield(&map, 2, 32, 3).expect("bake");
        assert_eq!(bake.albedo.len(), 4);
        assert!(bake.validate().is_ok());
    }

    #[test]
    fn mip_chain_boxes_and_terminates() {
        // 4×4 with one bright texel: the 2×2 level averages each quadrant, the 1×1 the field.
        let mut data = vec![0u8; 4 * 4 * 4];
        data[3] = 255; // alpha of texel (0,0)
        let image = Rgba8Image {
            width: 4,
            height: 4,
            data,
        };
        let mips = mip_chain(&image);
        assert_eq!(mips.len(), 3);
        assert_eq!(mips[1].width, 2);
        // Quadrant (0,0) has the only bright alpha: 255/4 ≈ 64.
        assert_eq!(mips[1].data[3], 64);
        assert_eq!(mips[2].width, 1);
        assert_eq!(mips[2].data[3], 16);
        // Odd sizes round up without reading past the edge.
        let odd = Rgba8Image {
            width: 3,
            height: 3,
            data: vec![128; 3 * 3 * 4],
        };
        let mips = mip_chain(&odd);
        assert_eq!(mips.len(), 3); // 3 → 2 → 1
        assert_eq!(mips[1].width, 2);
        assert!(mips[1].data.iter().all(|&v| v == 128));
    }

    /// A 4×4 normal map whose X and Y bytes are `x`, `y` plus alternating ±`ripple` (zero-mean).
    fn normal_with_mean(x: u8, y: u8, ripple: u8) -> Rgba8Image {
        let mut data = Vec::new();
        for i in 0..16u32 {
            let sign = if i % 2 == 0 { 1i32 } else { -1 };
            let off = sign * i32::from(ripple);
            data.extend_from_slice(&[(i32::from(x) + off) as u8, (i32::from(y) + off) as u8, 250, 255]);
        }
        Rgba8Image {
            width: 4,
            height: 4,
            data,
        }
    }

    #[test]
    fn a_flat_or_nearly_flat_normal_map_is_left_alone() {
        assert_eq!(Rgba8Image::neutral_normal(8).normal_mean_shift(), [0, 0]);
        assert!(Rgba8Image::neutral_normal(8).recentred_normal().is_none());
        // One level off (under `NORMAL_RECENTRE_MIN`) on both axes: untouched.
        let near = normal_with_mean(129, 127, 20);
        assert_eq!(near.normal_mean_shift(), [1, -1]);
        assert!(near.recentred_normal().is_none());
    }

    #[test]
    fn a_biased_normal_map_is_recentred_to_zero_mean() {
        // The showcase snow's bias: both axes about 19 levels over flat.
        let biased = normal_with_mean(147, 147, 10);
        assert_eq!(biased.normal_mean_shift(), [19, 19]);
        let fixed = biased.recentred_normal().expect("recentred");
        assert_eq!(fixed.normal_mean_shift(), [0, 0]);
        // The ripple around the mean is kept; Z and alpha are untouched.
        assert_eq!(fixed.data[0], 138);
        assert_eq!(fixed.data[4], 118);
        assert!(
            fixed
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[2] == 250 && p[3] == 255)
        );
        // A negative bias on one axis only moves that axis.
        let one = normal_with_mean(128, 100, 5)
            .recentred_normal()
            .expect("recentred");
        assert_eq!(one.normal_mean_shift(), [0, 0]);
        assert_eq!(one.data[0], 133);
    }

    #[test]
    fn recentring_clamps_and_survives_degenerate_images() {
        // One dark texel in a bright map: the shift (+106) takes it under 0, which clamps there.
        let mut hot = normal_with_mean(250, 128, 0);
        hot.data[0] = 0;
        assert_eq!(hot.normal_mean_shift(), [106, 0]);
        let fixed = hot.recentred_normal().expect("recentred");
        assert_eq!(fixed.data[0], 0);
        assert_eq!(fixed.data[4], 144);
        // An empty or mis-sized image has no shift and is not touched.
        let empty = Rgba8Image {
            width: 0,
            height: 0,
            data: Vec::new(),
        };
        assert_eq!(empty.normal_mean_shift(), [0, 0]);
        assert!(empty.recentred_normal().is_none());
        let short = Rgba8Image {
            width: 4,
            height: 4,
            data: vec![200; 8],
        };
        assert_eq!(short.normal_mean_shift(), [0, 0]);
    }

    #[test]
    fn baked_styles_are_already_zero_mean() {
        // The procedural sets (and so every golden that bakes them) pass through the upload
        // byte for byte.
        for style in [
            LayerStyle::Grass,
            LayerStyle::Rock,
            LayerStyle::Dirt,
            LayerStyle::Snow,
            LayerStyle::Sand,
            LayerStyle::Ice,
        ] {
            let shift = style.bake(128, 7).normal.normal_mean_shift();
            eprintln!("{style:?} normal mean shift {shift:?}");
            assert!(
                shift.iter().all(|s| s.abs() < NORMAL_RECENTRE_MIN),
                "{style:?} normal map mean shift {shift:?}"
            );
        }
    }
}
