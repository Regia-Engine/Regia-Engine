// SPDX-License-Identifier: MPL-2.0
//! File-level front end of `regia_render::compare` (`specs/27` § 2, agent `seeing.compare`):
//! loads two images (layered EXR from the tracer or a capture, or display-encoded PNG), scores
//! them and writes the FLIP error map.
//!
//! A denoised trace (`regia_pt::denoise`, marked with [`DENOISER_KEY`]) is a preview and is
//! refused as the reference: a denoiser trades noise for bias, and scoring the real-time frame
//! against that bias would reward matching the denoiser rather than the light transport.

use std::path::Path;

use regia_render::compare::{self, CompareReport, FlipResult};

use crate::PtError;
use crate::output::DENOISER_KEY;

/// Largest decoded PNG a comparison will allocate, in bytes.
const MAX_PNG_BYTES: usize = 1 << 30;

/// A loaded image: linear radiance (EXR) or display-encoded `[0, 1]` RGB (PNG).
#[derive(Clone, Debug)]
pub struct LoadedImage {
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Pixels, row-major.
    pub rgb: Vec<[f32; 3]>,
    /// True for linear radiance (EXR), false for display-encoded (PNG).
    pub linear: bool,
    /// The denoiser that produced the image ([`DENOISER_KEY`] in the EXR header or PNG `tEXt`),
    /// `None` for a raw trace or any other image.
    pub denoiser: Option<String>,
}

/// Loads `path`. For an EXR, `aov` names the layer (`aov.R/G/B`), then plain `R/G/B` if that
/// layer is missing, then `aov.Y`. An empty `aov` reads plain `R/G/B`. There is no default
/// layer: callers that want beauty pass `"beauty"`. An 8-bit PNG is display-encoded RGB and
/// ignores `aov`.
///
/// # Errors
///
/// The file is missing, not EXR/PNG, or lacks the layer.
pub fn load(path: &Path, aov: &str) -> Result<LoadedImage, PtError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "exr" {
        let img = crate::exr::read(path).map_err(|e| PtError::Io(format!("{}: {e}", path.display())))?;
        let rgb = img
            .rgb(aov)
            .or_else(|| img.rgb(""))
            .or_else(|| {
                img.channel(&format!("{aov}.Y"))
                    .map(|y| y.iter().map(|v| [*v; 3]).collect())
            })
            .ok_or_else(|| PtError::Io(format!("{} has no `{aov}` layer", path.display())))?;
        let denoiser = img
            .attributes
            .iter()
            .find(|(k, _)| regia_core::brand::key_matches(k, DENOISER_KEY))
            .map(|(_, v)| v.clone());
        return Ok(LoadedImage {
            width: img.width,
            height: img.height,
            rgb,
            linear: true,
            denoiser,
        });
    }
    let file = std::fs::File::open(path).map_err(|e| PtError::Io(format!("{}: {e}", path.display())))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| PtError::Io(format!("{}: {e}", path.display())))?;
    let denoiser = reader
        .info()
        .uncompressed_latin1_text
        .iter()
        .find(|t| regia_core::brand::key_matches(&t.keyword, DENOISER_KEY))
        .map(|t| t.text.clone());
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| PtError::Io(format!("{}: image too large", path.display())))?;
    if size > MAX_PNG_BYTES {
        return Err(PtError::Io(format!("{}: image too large", path.display())));
    }
    let mut buf = vec![0u8; size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| PtError::Io(format!("{}: {e}", path.display())))?;
    let channels = info.color_type.samples();
    let rgb = buf[..info.buffer_size()]
        .chunks_exact(channels)
        .map(|p| {
            let g = f32::from(p[0]) / 255.0;
            if channels >= 3 {
                [g, f32::from(p[1]) / 255.0, f32::from(p[2]) / 255.0]
            } else {
                [g; 3]
            }
        })
        .collect();
    Ok(LoadedImage {
        width: info.width,
        height: info.height,
        rgb,
        linear: false,
        denoiser,
    })
}

fn display(img: &LoadedImage, ev100: f32) -> Vec<[f32; 3]> {
    if img.linear {
        compare::display_encode(&img.rgb, ev100)
    } else {
        img.rgb.clone()
    }
}

/// Peak signal-to-noise ratio of `test` against `reference`, dB, on the display-encoded pair (the
/// images FLIP and SSIM score: linear inputs through the runtime's AgX at `ev100`, values in
/// `0..=1`), with the peak 1 and the mean squared error over every RGB channel:
/// `10 log10(1 / MSE)`. `None` when the images are equal or differ in size (ADR-2315: the
/// `psnr_db` of `seeing.compare`).
#[must_use]
pub fn psnr_db(reference: &LoadedImage, test: &LoadedImage, ev100: f32) -> Option<f64> {
    if (reference.width, reference.height) != (test.width, test.height) || reference.rgb.is_empty() {
        return None;
    }
    let (a, b) = (display(reference, ev100), display(test, ev100));
    let sum: f64 = a
        .iter()
        .zip(&b)
        .flat_map(|(x, y)| (0..3).map(move |c| (f64::from(x[c]) - f64::from(y[c])).powi(2)))
        .sum();
    let mse = sum / (3 * a.len()) as f64;
    (mse > 0.0 && mse.is_finite()).then(|| 10.0 * (1.0 / mse).log10())
}

fn srgb_decode(v: f32) -> f32 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// Scores `test` against `reference`. FLIP and SSIM run on both images display-encoded (linear
/// inputs through the runtime's AgX at `ev100`); RMSE runs on linear radiance when both are
/// linear, else on the decoded display values. A denoised `test` is fine (that is how a preview's
/// error is measured); a denoised `reference` is refused.
///
/// # Errors
///
/// [`PtError::DenoisedReference`] when `reference` is denoised; the sizes differ.
pub fn compare_images(
    reference: &LoadedImage,
    test: &LoadedImage,
    ev100: f32,
    ppd: f32,
) -> Result<(CompareReport, FlipResult), PtError> {
    if let Some(denoiser) = &reference.denoiser {
        return Err(PtError::DenoisedReference(denoiser.clone()));
    }
    if (reference.width, reference.height) != (test.width, test.height) {
        return Err(PtError::Scene(format!(
            "image sizes differ: {}x{} vs {}x{}",
            reference.width, reference.height, test.width, test.height
        )));
    }
    let (w, h) = (reference.width, reference.height);
    if reference.linear && test.linear {
        return Ok(compare::compare_linear(
            &reference.rgb,
            &test.rgb,
            w,
            h,
            ev100,
            ppd,
        ));
    }
    let (ra, ta) = (display(reference, ev100), display(test, ev100));
    let f = compare::flip(&ra, &ta, w, h, ppd);
    let s = compare::ssim(&ra, &ta, w, h);
    let lin = |v: &[[f32; 3]]| v.iter().map(|c| c.map(srgb_decode)).collect::<Vec<_>>();
    let (rl, tl) = (lin(&ra), lin(&ta));
    let (e, rel) = compare::rmse(&rl, &tl);
    let mean = |v: &[[f32; 3]]| {
        (v.iter()
            .map(|c| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2])
            .sum::<f32>())
            / v.len().max(1) as f32
    };
    Ok((
        CompareReport {
            width: w,
            height: h,
            flip_mean: f.mean,
            flip_p95: f.p95,
            ssim: s,
            rmse: e,
            rel_rmse: rel,
            mean_reference: mean(&rl),
            mean_test: mean(&tl),
        },
        f,
    ))
}

/// Loads both files, scores them ([`compare_images`]) and writes the FLIP map to `map_png`
/// when given.
///
/// # Errors
///
/// Loading, size mismatch or writing the map failed.
pub fn compare_files(
    reference: &Path,
    test: &Path,
    aov: &str,
    ev100: f32,
    ppd: f32,
    map_png: Option<&Path>,
) -> Result<CompareReport, PtError> {
    let a = load(reference, aov)?;
    let b = load(test, aov)?;
    let (report, flip) = compare_images(&a, &b, ev100, ppd)?;
    if let Some(path) = map_png {
        crate::output::write_png(
            path,
            report.width,
            report.height,
            &compare::flip_heatmap_rgba8(&flip.map),
        )?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exr::ExrChannel;

    #[test]
    fn exr_pair_round_trips_through_the_metrics() {
        let dir = std::env::temp_dir().join(format!("regia_pt_cmp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let (w, h) = (24u32, 16u32);
        let write = |name: &str, scale: f32| {
            let plane = |k: f32| {
                (0..w * h)
                    .map(|i| (i % w) as f32 * 10.0 * k * scale + 50.0)
                    .collect()
            };
            let channels = vec![
                ExrChannel {
                    name: "beauty.R".into(),
                    data: plane(1.0),
                },
                ExrChannel {
                    name: "beauty.G".into(),
                    data: plane(0.9),
                },
                ExrChannel {
                    name: "beauty.B".into(),
                    data: plane(0.8),
                },
            ];
            let p = dir.join(name);
            crate::exr::write(&p, w, h, &channels, &[]).expect("write");
            p
        };
        let a = write("a.exr", 1.0);
        let b = write("b.exr", 1.0);
        let c = write("c.exr", 1.5);
        let same = compare_files(&a, &b, "beauty", 8.0, compare::DEFAULT_PPD, None).expect("same");
        assert!(same.flip_mean < 1e-6 && same.rmse == 0.0);
        let map = dir.join("map.png");
        let diff = compare_files(&a, &c, "beauty", 8.0, compare::DEFAULT_PPD, Some(&map)).expect("diff");
        assert!(diff.flip_mean > 0.0 && diff.rel_rmse > 0.1);
        assert!(map.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_denoised_image_is_refused_as_the_reference_only() {
        let plane = vec![[1.0f32, 2.0, 3.0]; 16];
        let raw = LoadedImage {
            width: 4,
            height: 4,
            rgb: plane.clone(),
            linear: true,
            denoiser: None,
        };
        let preview = LoadedImage {
            denoiser: Some("oidn 2.5 RT hdr".into()),
            ..raw.clone()
        };
        let refused = compare_images(&preview, &raw, 0.0, compare::DEFAULT_PPD);
        assert!(
            matches!(&refused, Err(PtError::DenoisedReference(d)) if d == "oidn 2.5 RT hdr"),
            "{refused:?}"
        );
        assert!(
            refused
                .err()
                .is_some_and(|e| e.to_string().contains("never a reference"))
        );
        let (report, _) =
            compare_images(&raw, &preview, 0.0, compare::DEFAULT_PPD).expect("test side is fine");
        assert!(report.flip_mean < 1e-6);
    }

    /// `psnr_db` against its closed form on display-encoded (PNG-like) images: every channel off
    /// by 0.1 is an MSE of 0.01, so 20 dB; equal images and a size mismatch have none.
    #[test]
    fn psnr_matches_its_closed_form() {
        let img = |v: f32, n: usize| LoadedImage {
            width: n as u32,
            height: 1,
            rgb: vec![[v; 3]; n],
            linear: false,
            denoiser: None,
        };
        let p = psnr_db(&img(0.5, 4), &img(0.6, 4), 0.0).expect("they differ");
        assert!((p - 20.0).abs() < 1e-4, "{p}");
        assert_eq!(psnr_db(&img(0.5, 4), &img(0.5, 4), 0.0), None, "equal images");
        assert_eq!(psnr_db(&img(0.5, 4), &img(0.6, 2), 0.0), None, "sizes differ");
        // Linear radiance goes through the display transform first: brighter is still lower PSNR.
        let lin = |v: f32| LoadedImage {
            linear: true,
            ..img(v, 4)
        };
        let near = psnr_db(&lin(100.0), &lin(110.0), 8.0).expect("near");
        let far = psnr_db(&lin(100.0), &lin(400.0), 8.0).expect("far");
        assert!(near > far, "{near} vs {far}");
    }
}
