// SPDX-License-Identifier: MPL-2.0
//! A serializable heightfield recipe: a flat grid and operations on it. This is the form the
//! agent method `gen.heightfield` takes (`book/agents/gen-textures.md`).

use serde::{Deserialize, Serialize};

use crate::{Erosion, GenError, Heightfield, NoiseParams, Pad};

/// A heightfield: grid size, spacing and operations applied to a flat field at height 0.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeightfieldRecipe {
    /// Samples per row, at least 2.
    pub width: u32,
    /// Rows, at least 2.
    pub height: u32,
    /// Metres between samples (default 1).
    #[serde(default = "one")]
    pub spacing: f32,
    /// Operations, applied in order.
    #[serde(default)]
    pub ops: Vec<HeightOp>,
}

/// An operation on the heightfield.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum HeightOp {
    /// [`Heightfield::add_noise`].
    Noise {
        /// The noise.
        noise: NoiseParams,
        /// Metres the noise's `[0, 1]` spans.
        amplitude: f32,
        /// Metres added everywhere (default 0).
        #[serde(default)]
        base: f32,
    },
    /// [`Heightfield::terrace`].
    Terrace {
        /// Levels, at least 2.
        steps: u32,
        /// How far samples move onto their level, `[0, 1]` (default 1).
        #[serde(default = "one")]
        strength: f32,
    },
    /// [`Heightfield::flatten_pad`].
    Flatten(Pad),
    /// [`Heightfield::clamp`].
    Clamp {
        /// Lowest height.
        min: f32,
        /// Highest height.
        max: f32,
    },
    /// [`Heightfield::normalize`].
    Normalize {
        /// The new minimum.
        min: f32,
        /// The new maximum.
        max: f32,
    },
    /// [`Heightfield::thermal_erode`].
    Erode(Erosion),
}

fn one() -> f32 {
    1.0
}

/// Size limits of [`build_heightfield`] (an agent call is bounded by them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeightfieldLimits {
    /// Largest side in samples.
    pub side: u32,
    /// Most operations.
    pub ops: usize,
    /// Most work: [`heightfield_work`] of the recipe, checked before anything is built.
    pub work: u64,
}

impl Default for HeightfieldLimits {
    /// The agent's limits: side 1025, 32 operations, 2^26 work units (about a second and a half;
    /// for example 513 × 513 samples × 255 erosion passes, or 1025 × 1025 samples with 8 octaves
    /// of noise and 40 erosion passes).
    fn default() -> Self {
        Self {
            side: 1025,
            ops: 32,
            work: 1 << 26,
        }
    }
}

fn too_large(what: &'static str, value: u64, limit: u64) -> Result<(), GenError> {
    if value > limit {
        Err(GenError::TooLarge {
            what,
            value: usize::try_from(value).unwrap_or(usize::MAX),
            limit: usize::try_from(limit).unwrap_or(usize::MAX),
        })
    } else {
        Ok(())
    }
}

/// The estimated cost of building `recipe`, in sample-passes of erosion (about 20 ns each on a
/// desktop core): an erode operation costs samples × iterations, a noise fill samples × octaves × 2
/// (× 3 again for cellular kinds), every other operation the sample count.
pub fn heightfield_work(recipe: &HeightfieldRecipe) -> u64 {
    let samples = u64::from(recipe.width) * u64::from(recipe.height);
    recipe
        .ops
        .iter()
        .map(|op| match op {
            HeightOp::Erode(e) => samples.saturating_mul(u64::from(e.iterations)),
            HeightOp::Noise { noise, .. } => {
                let per = match noise.kind {
                    crate::NoiseKind::Cellular | crate::NoiseKind::CellularEdge => 6,
                    _ => 2,
                };
                samples.saturating_mul(u64::from(noise.octaves) * per)
            }
            _ => samples,
        })
        .fold(0, u64::saturating_add)
}

/// Builds `recipe` within `limits`.
pub fn build_heightfield(
    recipe: &HeightfieldRecipe,
    limits: &HeightfieldLimits,
) -> Result<Heightfield, GenError> {
    let _span = tracing::trace_span!("regia_gen::build_heightfield").entered();
    too_large(
        "heightfield side",
        u64::from(recipe.width.max(recipe.height)),
        u64::from(limits.side),
    )?;
    too_large("operation count", recipe.ops.len() as u64, limits.ops as u64)?;
    too_large(
        "estimated heightfield work",
        heightfield_work(recipe),
        limits.work,
    )?;
    let mut hf = Heightfield::flat(recipe.width, recipe.height, recipe.spacing)?;
    for op in &recipe.ops {
        match op {
            HeightOp::Noise {
                noise,
                amplitude,
                base,
            } => hf.add_noise(noise, *amplitude, *base)?,
            HeightOp::Terrace { steps, strength } => hf.terrace(*steps, *strength)?,
            HeightOp::Flatten(pad) => hf.flatten_pad(pad)?,
            HeightOp::Clamp { min, max } => hf.clamp(*min, *max)?,
            HeightOp::Normalize { min, max } => hf.normalize(*min, *max)?,
            HeightOp::Erode(e) => {
                hf.thermal_erode(e)?;
            }
        }
    }
    Ok(hf)
}
