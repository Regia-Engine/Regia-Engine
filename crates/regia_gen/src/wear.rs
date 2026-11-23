// SPDX-License-Identifier: MPL-2.0
//! A deterministic edge-wear and grime recipe: the mesh's edge and cavity masks
//! ([`curvature_masks`]) broken up by seeded tileable noise ([`noise_image`]) into masks a
//! material can use (paint chipped off the edges, dirt in the corners).
//!
//! - `breakup` = tileable gradient fBm (4 octaves, `breakup_cells` cells, the seed),
//! - `wear` = 0 where the edge mask is 0, else the smoothstep of
//!   `edge + (breakup − 0.5) · 2 · breakup_strength` over `[1 − amount − 0.05, 1 − amount + 0.05]`,
//! - `grime` = `clamp(blur(cavity, grime_blur, 2 passes) · (0.5 + dirt) · 2 · grime, 0, 1)` with
//!   `dirt` a tileable value fBm (3 octaves) from a seed derived from the seed.

use regia_asset::CookedImage;
use serde::{Deserialize, Serialize};

use crate::{
    CpuMesh, CurvatureOptions, FloatImage, Fractal, GenError, NoiseKind, NoiseParams, blur, curvature_masks,
    noise_image, pack_rgba8,
};

/// Options of [`wear_masks`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WearOptions {
    /// Seed of the breakup and dirt noise (default 0).
    pub seed: u64,
    /// The edge and cavity masks' options.
    pub curvature: CurvatureOptions,
    /// How much of the edge band wears through, `[0, 1]` (default 0.5).
    pub amount: f32,
    /// Noise cells across the texture, at least 1 (default 8).
    pub breakup_cells: u32,
    /// How strongly the noise breaks up the worn band, `[0, 1]` (default 0.6).
    pub breakup_strength: f32,
    /// Grime strength, `[0, 1]` (default 0.5).
    pub grime: f32,
    /// Blur radius of the cavity mask before it becomes grime, in texels (default 2).
    pub grime_blur: u32,
}

impl Default for WearOptions {
    fn default() -> Self {
        Self {
            seed: 0,
            curvature: CurvatureOptions::default(),
            amount: 0.5,
            breakup_cells: 8,
            breakup_strength: 0.6,
            grime: 0.5,
            grime_blur: 2,
        }
    }
}

/// The masks [`wear_masks`] makes, all the requested size.
#[derive(Clone, Debug, PartialEq)]
pub struct WearMasks {
    /// Worn edges: 1 where the top layer is gone.
    pub wear: FloatImage,
    /// Dirt gathered in concave corners.
    pub grime: FloatImage,
    /// The edge (convexity) mask the wear came from.
    pub edge: FloatImage,
    /// The cavity (concavity) mask the grime came from.
    pub cavity: FloatImage,
}

impl WearMasks {
    /// One RGBA8 texture: R wear, G grime, B edge, A cavity.
    pub fn packed(&self) -> Result<CookedImage, GenError> {
        pack_rgba8([&self.wear, &self.grime, &self.edge, &self.cavity])
    }
}

fn unit_range(what: &str, v: f32) -> Result<(), GenError> {
    if v.is_finite() && (0.0..=1.0).contains(&v) {
        Ok(())
    } else {
        Err(GenError::InvalidInput(format!(
            "{what} must be in [0, 1], got {v}"
        )))
    }
}

/// Bakes the wear and grime masks of `mesh` (which needs UVs) at `width × height`.
pub fn wear_masks(
    mesh: &CpuMesh,
    width: u32,
    height: u32,
    options: &WearOptions,
) -> Result<WearMasks, GenError> {
    let _span = tracing::trace_span!("regia_gen::wear_masks").entered();
    let o = options;
    unit_range("wear amount", o.amount)?;
    unit_range("wear breakup_strength", o.breakup_strength)?;
    unit_range("wear grime", o.grime)?;
    let masks = curvature_masks(mesh, width, height, &o.curvature)?;
    let breakup = noise_image(
        &NoiseParams {
            kind: NoiseKind::Gradient,
            seed: o.seed,
            cells: o.breakup_cells,
            octaves: 4,
            fractal: Fractal::Fbm,
            lacunarity: 2,
            gain: 0.5,
            tile: true,
        },
        width,
        height,
    )?;
    let dirt = noise_image(
        &NoiseParams {
            kind: NoiseKind::Value,
            seed: crate::noise::mix64(o.seed ^ 0x0067_7269_6D65),
            cells: o.breakup_cells,
            octaves: 3,
            fractal: Fractal::Fbm,
            lacunarity: 2,
            gain: 0.5,
            tile: true,
        },
        width,
        height,
    )?;
    let at = 1.0 - o.amount;
    let wear: Vec<f32> = masks
        .edge
        .pixels
        .iter()
        .zip(&breakup.pixels)
        .map(|(&e, &b)| {
            if e <= 0.0 {
                return 0.0;
            }
            let s = e + (b - 0.5) * 2.0 * o.breakup_strength;
            let t = ((s - (at - 0.05)) / 0.1).clamp(0.0, 1.0);
            t * t * (3.0 - 2.0 * t)
        })
        .collect();
    let soft = blur(&masks.cavity, o.grime_blur, 2, false)?;
    let grime: Vec<f32> = soft
        .pixels
        .iter()
        .zip(&dirt.pixels)
        .map(|(&c, &d)| (c * (0.5 + d) * 2.0 * o.grime).clamp(0.0, 1.0))
        .collect();
    Ok(WearMasks {
        wear: FloatImage {
            width,
            height,
            pixels: wear,
        },
        grime: FloatImage {
            width,
            height,
            pixels: grime,
        },
        edge: masks.edge,
        cavity: masks.cavity,
    })
}
