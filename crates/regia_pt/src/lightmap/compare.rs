// SPDX-License-Identifier: MPL-2.0
//! How close two bakes of the same layout are (ADR-2125): the peak signal-to-noise ratio of the
//! decoded irradiance over the texels both baked, and the relative difference per channel. The GPU
//! baker is held to the CPU baker with it ([`LIGHTMAP_GPU_PSNR_FLOOR_DB`]), and
//! `regia bake-lightmaps --compare <file>` prints it.
//!
//! PSNR is the textbook definition, `10 log10(peak² / MSE)`, with the peak the reference's
//! brightest baked channel (lightmap irradiance has no fixed white point) and the mean squared
//! error taken over the RGB channels of every texel both maps baked.

use regia_asset::lightmap::{Coverage, Lightmap};

use crate::PtError;

/// The PSNR a GPU bake keeps against the CPU bake of the same scene and settings, decibels: the
/// documented tolerance of the GPU baker (`book/agents/lightmaps.md`, ADR-2125). Both bakers draw
/// the same random numbers, so what is left is float rounding and the few rays it sends the other
/// way at an edge.
pub const LIGHTMAP_GPU_PSNR_FLOOR_DB: f64 = 40.0;

/// How close `test` is to `reference` ([`compare_lightmaps`]).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct LightmapComparison {
    /// Texels both maps baked (coverage `BAKED`), the ones compared.
    pub texels: usize,
    /// Texels baked in one map and not in the other.
    pub coverage_mismatch: usize,
    /// The reference's brightest baked channel, the PSNR's peak.
    pub peak: f64,
    /// Mean squared error of the irradiance channels.
    pub mse: f64,
    /// Peak signal-to-noise ratio, dB; `None` when the maps are equal (no error) or the
    /// reference is black.
    pub psnr_db: Option<f64>,
    /// Mean relative difference per channel (`|a - b| / max(|a|, |b|)`, 0 below 1e-4).
    pub mean_rel: f64,
    /// 99th percentile of the relative difference.
    pub p99_rel: f64,
    /// Largest relative difference.
    pub max_rel: f64,
}

impl LightmapComparison {
    /// Whether the maps are equal (`mse` 0) or at least `floor_db` apart in PSNR, with the same
    /// texels baked.
    #[must_use]
    pub fn within(&self, floor_db: f64) -> bool {
        self.coverage_mismatch == 0 && (self.mse == 0.0 || self.psnr_db.is_some_and(|p| p >= floor_db))
    }
}

/// Compares `test` with `reference`, texel by texel, over the texels both baked.
///
/// # Errors
///
/// [`PtError::Scene`] when the maps differ in size or share no baked texel.
pub fn compare_lightmaps(reference: &Lightmap, test: &Lightmap) -> Result<LightmapComparison, PtError> {
    let size = |m: &Lightmap| (m.width, m.height, m.pages());
    if size(reference) != size(test) {
        return Err(PtError::Scene(format!(
            "lightmap_compare_size: the reference is {}x{} on {} page(s) and the other map {}x{} on {}",
            reference.width,
            reference.height,
            reference.pages(),
            test.width,
            test.height,
            test.pages()
        )));
    }
    let mut rel: Vec<f64> = Vec::new();
    let (mut sq, mut peak, mut texels, mut mismatch) = (0.0f64, 0.0f64, 0usize, 0usize);
    // Every page's rows, stacked (ADR-3300).
    for y in 0..reference.height * reference.pages() {
        for x in 0..reference.width {
            let (ra, ta) = (
                reference.coverage_at(x, y) == Coverage::BAKED,
                test.coverage_at(x, y) == Coverage::BAKED,
            );
            if ra != ta {
                mismatch += 1;
            }
            if !(ra && ta) {
                continue;
            }
            texels += 1;
            let (a, b) = (reference.irradiance_at(x, y), test.irradiance_at(x, y));
            for k in 0..3 {
                let (a, b) = (f64::from(a[k]), f64::from(b[k]));
                peak = peak.max(a.abs());
                sq += (a - b) * (a - b);
                let top = a.abs().max(b.abs());
                // RGB9E5 cannot tell values below 1 / 65536 of the texel's brightest channel.
                rel.push(if top < 1e-4 { 0.0 } else { (a - b).abs() / top });
            }
        }
    }
    if texels == 0 {
        return Err(PtError::Scene(
            "lightmap_compare_empty: the maps share no baked texel".into(),
        ));
    }
    rel.sort_by(f64::total_cmp);
    let mse = sq / (texels * 3) as f64;
    let psnr_db = (mse > 0.0 && peak > 0.0).then(|| 10.0 * (peak * peak / mse).log10());
    Ok(LightmapComparison {
        texels,
        coverage_mismatch: mismatch,
        peak,
        mse,
        psnr_db,
        mean_rel: rel.iter().sum::<f64>() / rel.len() as f64,
        p99_rel: rel[rel.len() * 99 / 100],
        max_rel: rel.last().copied().unwrap_or(0.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lightmap::{
        LightmapBakeSettings, LightmapScene, MeshLightmapRequest, bake_lightmap, mesh_layout, quad_box_scene,
    };

    fn bake(seed: u32) -> Lightmap {
        let qb = quad_box_scene();
        let scene = LightmapScene::from_pt_scene(&qb.scene).expect("flatten");
        let request = MeshLightmapRequest {
            instance: qb.floor,
            name: "floor",
            uv: &qb.floor_uv,
            size: [16, 16],
            padding: 2,
        };
        let settings = LightmapBakeSettings {
            samples: 16,
            seed,
            threads: 1,
            ..LightmapBakeSettings::default()
        };
        let layout = mesh_layout(&scene, &request).expect("layout");
        bake_lightmap(&scene, layout, &[qb.floor], &settings, |_, _| {})
            .expect("bake")
            .0
    }

    /// A map equals itself; another seed is noise of the same light, finite PSNR, not equal;
    /// a darkened copy is far off; a size change is refused.
    #[test]
    fn the_comparison_tells_equal_noisy_and_wrong_apart() {
        let a = bake(1);
        let same = compare_lightmaps(&a, &a).expect("compares");
        assert_eq!(same.mse, 0.0);
        assert_eq!(same.psnr_db, None);
        assert!(same.within(LIGHTMAP_GPU_PSNR_FLOOR_DB));
        assert!(same.texels > 50, "{same:?}");

        let b = bake(2);
        let noisy = compare_lightmaps(&a, &b).expect("compares");
        let db = noisy.psnr_db.expect("finite");
        assert!(db > 10.0 && db < 80.0, "{noisy:?}");
        assert!(noisy.mean_rel > 0.0);

        // Twice the exposure decodes every texel at half the light.
        // A clone then a field write: `Lightmap` has a private lookup cache, so a struct update
        // from outside `regia_asset` does not compile.
        let mut dark = a.clone();
        dark.exposure = a.exposure * 2.0;
        let off = compare_lightmaps(&a, &dark).expect("compares");
        assert!(
            off.psnr_db.expect("finite") < LIGHTMAP_GPU_PSNR_FLOOR_DB,
            "{off:?}"
        );
        assert!(!off.within(LIGHTMAP_GPU_PSNR_FLOOR_DB));
        // Every lit channel is off by half of itself.
        assert!(off.mean_rel > 0.3 && off.max_rel <= 0.5 + 1e-3, "{off:?}");

        let mut wide = a.clone();
        wide.width = a.width + 1;
        let e = compare_lightmaps(&a, &wide).expect_err("size");
        assert!(e.to_string().contains("lightmap_compare_size"), "{e}");
    }
}
