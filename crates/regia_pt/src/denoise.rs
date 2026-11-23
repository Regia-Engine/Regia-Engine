// SPDX-License-Identifier: MPL-2.0
//! Preview denoising with Intel Open Image Denoise (the `denoise` feature; `oidn` crate, MIT,
//! over the OIDN library, Apache-2.0).
//!
//! A low-spp trace plus OIDN's `RT` filter (HDR beauty guided by the first-hit albedo and normal
//! AOVs) gives a look-iteration preview in a fraction of a converged trace's time. The result is
//! **a preview only**: it is marked with [`crate::output::DENOISER_KEY`] in its metadata, EXR
//! header and preview PNG, and [`crate::compare`] refuses it as a reference (`specs/27` § 2).
//! Only `beauty` is filtered; every other AOV keeps the raw trace's values.
//!
//! OIDN runs on the CPU device: the farm's GPUs are assigned per job, and OIDN's default device
//! picks any CUDA card it can see. The native library is found at build time through `OIDN_DIR`
//! (or pkg-config) and at run time through the loader path (`LD_LIBRARY_PATH=$OIDN_DIR/lib`).

use crate::{PtError, PtImage};

/// OIDN's filter quality.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DenoiseQuality {
    /// Highest quality (the default; previews are not real-time).
    #[default]
    High,
    /// Lower precision where the device supports it.
    Balanced,
    /// Fastest.
    Fast,
}

/// How [`denoise`] filters a trace.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DenoiseOptions {
    /// Guide the filter with the `albedo` and `normal` AOVs (OIDN's recommended setup; off
    /// filters the beauty alone).
    pub use_aux: bool,
    /// Filter quality.
    pub quality: DenoiseQuality,
}

impl Default for DenoiseOptions {
    fn default() -> Self {
        Self {
            use_aux: true,
            quality: DenoiseQuality::High,
        }
    }
}

/// The OIDN API the `oidn` crate binds (the native library must be this version or newer).
pub const OIDN_API: &str = "2.5";

/// The albedo guide: the first-hit base colour, and for background pixels (depth 0) the sky's
/// own colour normalised to its brightest channel, as OIDN asks for directly visible emitters.
fn albedo_guide(image: &PtImage) -> Vec<[f32; 3]> {
    image
        .albedo
        .iter()
        .zip(&image.depth)
        .zip(&image.beauty)
        .map(|((albedo, depth), beauty)| {
            if *depth > 0.0 {
                albedo.map(|c| c.clamp(0.0, 1.0))
            } else {
                let peak = beauty[0].max(beauty[1]).max(beauty[2]);
                if peak > 0.0 {
                    beauty.map(|c| (c / peak).clamp(0.0, 1.0))
                } else {
                    [0.0; 3]
                }
            }
        })
        .collect()
}

fn oidn_error(stage: &str, e: &oidn::Error) -> PtError {
    PtError::Gpu(format!("oidn {stage}: {e}"))
}

/// Denoises `image`'s beauty with OIDN's `RT` filter and returns the preview (every other AOV
/// unchanged, `metadata.denoiser` and `metadata.denoise_seconds` set).
///
/// # Errors
///
/// The image is empty or inconsistent, or OIDN could not create its device or failed to
/// filter (for example the library's CPU module is missing).
pub fn denoise(image: &PtImage, options: &DenoiseOptions) -> Result<PtImage, PtError> {
    let _span =
        tracing::info_span!("regia_pt::denoise", width = image.width, height = image.height).entered();
    let pixels = (image.width as usize) * (image.height as usize);
    if pixels == 0 || image.beauty.len() != pixels {
        return Err(PtError::Scene(format!(
            "cannot denoise a {}x{} image with {} beauty pixels",
            image.width,
            image.height,
            image.beauty.len()
        )));
    }
    if options.use_aux
        && (image.albedo.len() != pixels || image.normal.len() != pixels || image.depth.len() != pixels)
    {
        return Err(PtError::Scene(
            "albedo/normal/depth AOVs do not match the beauty".into(),
        ));
    }
    let started = std::time::Instant::now();
    let device = oidn::Device::cpu().map_err(|e| oidn_error("device", &e))?;
    let mut filter = oidn::RayTracing::try_new(&device).map_err(|e| oidn_error("filter", &e))?;
    filter
        // No `clean_aux`: oidn 2.5.1 sets it under the name `clean_aux`, which OIDN (`cleanAux`)
        // ignores, so the filter always runs its noisy-aux model; measured identical either way.
        .hdr(true)
        .filter_quality(match options.quality {
            DenoiseQuality::High => oidn::Quality::High,
            DenoiseQuality::Balanced => oidn::Quality::Balanced,
            DenoiseQuality::Fast => oidn::Quality::Fast,
        })
        .image_dimensions(image.width as usize, image.height as usize);
    if options.use_aux {
        let albedo = albedo_guide(image);
        let albedo = device
            .create_buffer(bytemuck::cast_slice(&albedo))
            .map_err(|e| oidn_error("albedo buffer", &e))?;
        let normal = device
            .create_buffer(bytemuck::cast_slice(&image.normal))
            .map_err(|e| oidn_error("normal buffer", &e))?;
        filter
            .albedo_normal_buffer(albedo, normal)
            .map_err(|e| oidn_error("aux", &e))?;
    }
    let mut out = vec![[0.0f32; 3]; pixels];
    filter
        .filter(
            bytemuck::cast_slice(&image.beauty),
            bytemuck::cast_slice_mut(&mut out),
        )
        .map_err(|e| oidn_error("filter", &e))?;
    device.get_error().map_err(|e| oidn_error("execute", &e))?;
    // The filter's output is finite and non-negative for valid HDR input; keep it that way for
    // the EXR and the comparison metrics regardless.
    for p in &mut out {
        for c in p.iter_mut() {
            if !c.is_finite() || *c < 0.0 {
                *c = 0.0;
            }
        }
    }
    let mut preview = image.clone();
    preview.beauty = out;
    let quality = match options.quality {
        DenoiseQuality::High => "high",
        DenoiseQuality::Balanced => "balanced",
        DenoiseQuality::Fast => "fast",
    };
    let aux = if options.use_aux {
        "albedo+normal"
    } else {
        "no aux"
    };
    preview.metadata.denoiser = format!("oidn {OIDN_API} RT hdr {aux} {quality}");
    preview.metadata.denoise_seconds = started.elapsed().as_secs_f64();
    Ok(preview)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PtMetadata;

    /// A flat grey wall split into two albedo halves, with per-pixel noise of known amplitude.
    fn noisy(width: u32, height: u32) -> (PtImage, Vec<[f32; 3]>) {
        let n = (width * height) as usize;
        let mut state = 0x1234_5678u32;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as f32 / u32::MAX as f32
        };
        let mut clean = Vec::with_capacity(n);
        let mut beauty = Vec::with_capacity(n);
        let mut albedo = Vec::with_capacity(n);
        for i in 0..n {
            let a = if (i as u32 % width) < width / 2 { 0.2 } else { 0.7 };
            let c = [a * 100.0; 3];
            let k = 0.4 + 1.2 * rand();
            clean.push(c);
            beauty.push(c.map(|v| v * k));
            albedo.push([a; 3]);
        }
        let image = PtImage {
            width,
            height,
            beauty,
            albedo,
            normal: vec![[0.0, 0.0, 1.0]; n],
            depth: vec![5.0; n],
            direct: vec![[0.0; 3]; n],
            indirect_diffuse: vec![[0.0; 3]; n],
            indirect_specular: vec![[0.0; 3]; n],
            emissive: vec![[0.0; 3]; n],
            volumetric: vec![[0.0; 3]; n],
            sky: vec![[0.0; 3]; n],
            shadow: vec![1.0; n],
            roughness: vec![0.5; n],
            variance: vec![[0.0; 3]; n],
            spp: 4,
            spp_map: Vec::new(),
            metadata: PtMetadata {
                spp: 4,
                ..PtMetadata::default()
            },
        };
        (image, clean)
    }

    fn rel_rmse(a: &[[f32; 3]], b: &[[f32; 3]]) -> f64 {
        let (mut e, mut m) = (0.0f64, 0.0f64);
        for (p, q) in a.iter().zip(b) {
            for k in 0..3 {
                e += f64::from(p[k] - q[k]).powi(2);
                m += f64::from(q[k]);
            }
        }
        let n = (a.len() * 3) as f64;
        (e / n).sqrt() / (m / n)
    }

    #[test]
    fn removes_noise_and_marks_the_preview() {
        let (image, clean) = noisy(96, 64);
        let before = rel_rmse(&image.beauty, &clean);
        let preview = denoise(&image, &DenoiseOptions::default()).expect("oidn denoise");
        let after = rel_rmse(&preview.beauty, &clean);
        assert!(after < before * 0.35, "rel RMSE {before:.3} → {after:.3}");
        assert!(
            preview.metadata.denoiser.starts_with("oidn 2.5 RT"),
            "{}",
            preview.metadata.denoiser
        );
        assert!(preview.metadata.denoise_seconds > 0.0);
        assert_eq!(preview.albedo, image.albedo, "aux AOVs stay raw");
    }

    #[test]
    fn rejects_inconsistent_images() {
        let (mut image, _) = noisy(8, 8);
        image.normal.pop();
        assert!(denoise(&image, &DenoiseOptions::default()).is_err());
        image.width = 0;
        assert!(denoise(&image, &DenoiseOptions::default()).is_err());
    }

    #[test]
    fn background_albedo_is_the_normalised_sky() {
        let (mut image, _) = noisy(2, 1);
        image.depth[1] = 0.0;
        image.beauty[1] = [10.0, 20.0, 40.0];
        let guide = albedo_guide(&image);
        assert_eq!(guide[0], [0.2; 3]);
        assert_eq!(guide[1], [0.25, 0.5, 1.0]);
    }
}
