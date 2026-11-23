// SPDX-License-Identifier: MPL-2.0
//! Grass multipliers from biomes (spec 65 § 8).
//!
//! For a grass class `c`, the multiplier at a position is the claim-weighted sum over the owner and
//! second claim of each biome's density for `c`, plus the unclaimed weight times the `outside`
//! value. `outside` is 1 when every biome that names `c` says `Keep` (the default) and 0 when any
//! says `Clear`; a biome that does not name `c` counts as `outside` too. Classes no biome names get
//! no multiplier.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use crate::biome::def::GrassOutside;
use crate::biome::{BiomeWorld, ClaimAt};
use crate::data::MaskData;

/// The classes the captured biomes name, with their `outside` value.
pub fn grass_classes(world: &BiomeWorld) -> BTreeMap<String, f32> {
    let mut out: BTreeMap<String, f32> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for area in &world.areas {
        for (path, def) in &area.biomes {
            if !seen.insert(path.clone()) {
                continue;
            }
            for g in &def.grass {
                let v = match g.outside {
                    GrassOutside::Keep => 1.0,
                    GrassOutside::Clear => 0.0,
                };
                let slot = out.entry(g.class.clone()).or_insert(1.0);
                *slot = slot.min(v);
            }
        }
    }
    out
}

/// The multiplier of class `class` at `(x, z)`.
pub fn multiplier_at(world: &BiomeWorld, class: &str, outside: f32, x: f32, z: f32) -> f32 {
    let c = world.claims_at(x, z);
    let mut sum = 0.0f32;
    let mut claimed = 0.0f32;
    for claim in [c.owner, c.second].into_iter().flatten() {
        let Some((_, def)) = world
            .areas
            .get(claim.area)
            .and_then(|a| a.biomes.get(claim.biome))
        else {
            continue;
        };
        let d = def
            .grass
            .iter()
            .find(|g| g.class == class)
            .map_or(outside, |g| g.density as f32);
        sum += claim.weight * d;
        claimed += claim.weight;
    }
    (sum + (1.0 - claimed).max(0.0) * outside).clamp(0.0, 1.0)
}

/// One multiplier image per class over `rect` with `texel_m` texels (texel centers sampled).
pub fn grass_masks(world: &BiomeWorld, rect: [f32; 4], texel_m: f32) -> Vec<(String, MaskData)> {
    match grass_masks_in(world, rect, texel_m, |len, f| {
        Ok::<_, std::convert::Infallible>(vec![f(0..len)])
    }) {
        Ok(masks) => masks,
        Err(never) => match never {},
    }
}

/// Per class, the multiplier of each texel in a range of row-major texel indices.
type TexelColumns<'a> = dyn Fn(Range<usize>) -> Vec<Vec<f32>> + Sync + 'a;

/// [`grass_masks`] with the texels split by `map`: it gets the texel count and a function that
/// fills a range of row-major texels, and returns the ranges' results in texel order (the
/// `biome.grass_mask` node passes `NodeCtx::map_chunks`, ADR-7252). The images are the same for any
/// split.
pub(crate) fn grass_masks_in<E>(
    world: &BiomeWorld,
    rect: [f32; 4],
    texel_m: f32,
    map: impl FnOnce(usize, &TexelColumns<'_>) -> Result<Vec<Vec<Vec<f32>>>, E>,
) -> Result<Vec<(String, MaskData)>, E> {
    let _span = tracing::info_span!("biome_grass_masks").entered();
    let texel = if texel_m.is_finite() && texel_m > 0.0 {
        texel_m
    } else {
        1.0
    };
    let w = (((rect[2] - rect[0]) / texel).ceil().max(1.0) as u32).min(4096);
    let h = (((rect[3] - rect[1]) / texel).ceil().max(1.0) as u32).min(4096);
    let tw = (rect[2] - rect[0]) / w as f32;
    let th = (rect[3] - rect[1]) / h as f32;
    let classes: Vec<(String, f32)> = grass_classes(world).into_iter().collect();
    if classes.is_empty() {
        return Ok(Vec::new());
    }
    // `Some(density)` when the biome names the class, else the class-level `outside` value.
    let density: Vec<Vec<Vec<Option<f32>>>> = world
        .areas
        .iter()
        .map(|area| {
            area.biomes
                .iter()
                .map(|(_, def)| {
                    classes
                        .iter()
                        .map(|(class, _)| {
                            def.grass
                                .iter()
                                .find(|g| g.class == *class)
                                .map(|g| g.density as f32)
                        })
                        .collect()
                })
                .collect()
        })
        .collect();
    let fill = |range: Range<usize>| -> Vec<Vec<f32>> {
        let mut columns: Vec<Vec<f32>> = classes.iter().map(|_| Vec::with_capacity(range.len())).collect();
        for t in range {
            let (row, col) = (t / w as usize, t % w as usize);
            let z = rect[1] + (row as f32 + 0.5) * th;
            let x = rect[0] + (col as f32 + 0.5) * tw;
            let claim = world.claims_at(x, z);
            for (ci, (_, outside)) in classes.iter().enumerate() {
                columns[ci].push(class_multiplier(&claim, &density, ci, *outside));
            }
        }
        columns
    };
    let texels = (w as usize) * (h as usize);
    let parts = map(texels, &fill)?;
    let mut columns: Vec<Vec<f32>> = classes.iter().map(|_| Vec::with_capacity(texels)).collect();
    for part in parts {
        for (column, values) in columns.iter_mut().zip(part) {
            column.extend(values);
        }
    }
    Ok(classes
        .into_iter()
        .zip(columns)
        .filter_map(|((class, _), values)| MaskData::new(rect, w, h, values, false).ok().map(|m| (class, m)))
        .collect())
}

/// The same sum as [`multiplier_at`], from one claim and a density table built for the run.
fn class_multiplier(claim: &ClaimAt, density: &[Vec<Vec<Option<f32>>>], class: usize, outside: f32) -> f32 {
    let mut sum = 0.0f32;
    let mut claimed = 0.0f32;
    for hit in [claim.owner, claim.second].into_iter().flatten() {
        let d = density
            .get(hit.area)
            .and_then(|area| area.get(hit.biome))
            .and_then(|row| row.get(class).copied().flatten())
            .unwrap_or(outside);
        sum += hit.weight * d;
        claimed += hit.weight;
    }
    (sum + (1.0 - claimed).max(0.0) * outside).clamp(0.0, 1.0)
}
