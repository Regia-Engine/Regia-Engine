// SPDX-License-Identifier: MPL-2.0
//! The GI denoiser (ADR-864) on path-traced frames, scored against a converged trace.
//!
//! The synthetic scenarios of `regia_render/tests/gi_denoise.rs` have exact truths but
//! made-up noise. Here the noise is the path tracer's own: sixteen one-sample frames of the
//! Cornell box's indirect diffuse light (different seeds, pixel centres, so the guides are
//! exact), demodulated by the first-hit albedo as Schied et al. do, denoised, remodulated and
//! compared with a 512-sample trace of the same view. The bounds are conservative (set before
//! the first GPU run); the test prints the measured values so they can be tightened.

use std::sync::OnceLock;

use regia_pt::{PathTracer, PtImage, PtSettings, scenes};
use regia_render::RenderDevice;
use regia_render::gi_denoise::{GiDenoiseInput, GiDenoiseSettings, GiDenoiser, metrics};

const SIZE: u32 = 48;
const FRAMES: u32 = 16;

/// One frame's raw and denoised colors.
type RawAndDenoised = (Vec<[f32; 3]>, Vec<[f32; 3]>);

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::headless().expect("headless device");
        PathTracer::new(&device).expect("compile the path tracer")
    })
}

fn trace(spp: u32, seed: u32) -> PtImage {
    let scene = scenes::cornell_box(SIZE, SIZE, true);
    let settings = PtSettings {
        spp,
        max_bounces: 8,
        seed,
        rr_start: 3,
        jitter: false,
        samples_per_submit: 64,
    };
    tracer().render(&scene, &settings).expect("render")
}

/// `indirect_diffuse / albedo`, per channel (0 where the albedo is black).
fn demodulate(image: &PtImage) -> Vec<[f32; 3]> {
    image
        .indirect_diffuse
        .iter()
        .zip(&image.albedo)
        .map(|(c, a)| [0, 1, 2].map(|k| if a[k] > 1e-3 { c[k] / a[k] } else { 0.0 }))
        .collect()
}

fn remodulate(signal: &[[f32; 3]], albedo: &[[f32; 3]]) -> Vec<[f32; 3]> {
    signal
        .iter()
        .zip(albedo)
        .map(|(s, a)| [s[0] * a[0], s[1] * a[1], s[2] * a[2]])
        .collect()
}

#[test]
fn denoised_one_sample_frames_approach_the_converged_trace() {
    let reference = trace(512, 7);
    let n = (SIZE * SIZE) as usize;
    // Pixels with a surface whose albedo is not black (the light is emissive, albedo 0).
    let mask: Vec<bool> = reference
        .depth
        .iter()
        .zip(&reference.albedo)
        .map(|(z, a)| *z > 0.0 && regia_render::gi_denoise::luminance(*a) > 0.01)
        .collect();
    let mut denoiser = GiDenoiser::new(GiDenoiseSettings::default()).expect("denoiser");
    let motion = vec![[0.0f32; 2]; n];
    let mut sum = vec![[0.0f32; 3]; n];
    let mut previous: Option<RawAndDenoised> = None;
    let mut raw_flicker = 0.0f32;
    let mut flicker = 0.0f32;
    let mut pairs = 0u32;
    let mut last = None;
    for frame in 0..FRAMES {
        let image = trace(1, 100 + frame);
        let guide: Vec<[f32; 4]> = image
            .normal
            .iter()
            .zip(&image.depth)
            .map(|(nrm, z)| [nrm[0], nrm[1], nrm[2], *z])
            .collect();
        let signal = demodulate(&image);
        let out = denoiser
            .denoise(&GiDenoiseInput {
                width: SIZE,
                height: SIZE,
                noisy: &signal,
                guide: &guide,
                motion: &motion,
            })
            .expect("denoise");
        let filtered = remodulate(&out.color, &image.albedo);
        for (s, c) in sum.iter_mut().zip(&image.indirect_diffuse) {
            for (a, b) in s.iter_mut().zip(c) {
                *a += b;
            }
        }
        if frame >= FRAMES / 2
            && let Some((prev_raw, prev_out)) = previous.as_ref()
        {
            raw_flicker += metrics::temporal_flicker(prev_raw, &image.indirect_diffuse, &motion, &mask, SIZE)
                .expect("raw flicker");
            flicker += metrics::temporal_flicker(prev_out, &filtered, &motion, &mask, SIZE).expect("flicker");
            pairs += 1;
        }
        previous = Some((image.indirect_diffuse.clone(), filtered.clone()));
        last = Some((image, filtered));
    }
    let (image, filtered) = last.expect("frames ran");
    let average: Vec<[f32; 3]> = sum.iter().map(|s| s.map(|v| v / FRAMES as f32)).collect();
    let truth = &reference.indirect_diffuse;
    let raw = metrics::relative_rmse(&image.indirect_diffuse, truth, &mask).expect("raw");
    let averaged = metrics::relative_rmse(&average, truth, &mask).expect("average");
    let denoised = metrics::relative_rmse(&filtered, truth, &mask).expect("denoised");
    let bias = metrics::mean_bias(&filtered, truth, &mask).expect("bias");
    let raw_flicker = raw_flicker / pairs as f32;
    let flicker = flicker / pairs as f32;
    eprintln!(
        "gi_denoise on the path-traced Cornell box ({SIZE}x{SIZE}, {FRAMES} frames of 1 spp vs 512 spp): \
         rel rmse raw {raw:.4}, {FRAMES}-frame average {averaged:.4}, denoised {denoised:.4}; bias {bias:+.4}; \
         flicker raw {raw_flicker:.4}, denoised {flicker:.4}"
    );
    assert!(denoised < 0.5 * raw, "denoised {denoised} vs one sample {raw}");
    assert!(
        denoised < averaged,
        "denoised {denoised} should beat a plain {FRAMES}-sample average {averaged}"
    );
    // The luminance weight trims the bright tail of the noise, so the filter darkens a little
    // (-1.4 % on the synthetic scenes); the path tracer's tail is longer.
    assert!(bias.abs() < 0.2, "bias {bias}");
    assert!(
        flicker < 0.25 * raw_flicker,
        "flicker {flicker} vs raw {raw_flicker}"
    );
}
