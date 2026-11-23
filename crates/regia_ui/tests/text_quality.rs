// SPDX-License-Identifier: MPL-2.0
//! Text quality vs a reference rasterization (`specs/18` § 2, lane ui-look): the same swash
//! coverage masks composited on the CPU in sRGB space — how FreeType-style rasterizers, browsers
//! and design tools show grayscale-AA text — against the GPU overlay on an sRGB target.
//!
//! Metrics, per size and polarity, over the text box:
//! - **ink ratio**: Σ normalized ink (|out − bg| / |fg − bg|, sRGB) of GPU ÷ reference. Below 1
//!   the strokes look thinner (lighter for dark text), above 1 bolder. Linear-light compositing
//!   of raw coverage thins dark-on-light text and bolds light-on-dark text.
//! - **edge error**: Σ |ink_gpu − ink_ref| ÷ Σ ink_ref: how far the antialiased edge profile
//!   (and so edge contrast / stem darkness) departs from the reference, pixel by pixel.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_render::RenderDevice;
use regia_ui::atlas::PAGE_SIZE;
use regia_ui::draw::{TextureRef, UiQuad};
use regia_ui::text::{Text, TextSpan, TextStyle, TextSystem, WrapMode};
use regia_ui::{Localization, OffscreenTarget, UiColor, UiRenderer};

const W: u32 = 360;
const H: u32 = 56;
const SAMPLE: &str = "Hamburgefontsiv 0123 Squad lead";

struct Polarity {
    name: &'static str,
    fg: [u8; 3],
    bg: [u8; 3],
}

const POLARITIES: [Polarity; 2] = [
    Polarity {
        name: "light-on-dark",
        fg: [232, 237, 242],
        bg: [16, 20, 26],
    },
    Polarity {
        name: "dark-on-light",
        fg: [16, 20, 26],
        bg: [242, 242, 242],
    },
];

fn srgb(c: [u8; 3]) -> UiColor {
    UiColor::from_srgb8(c[0], c[1], c[2], 255)
}

/// Returns (gpu pixels, reference pixels) for `px`-sized text of one polarity.
fn render_pair(
    device: &RenderDevice,
    sys: &mut TextSystem,
    px: f32,
    pol: &Polarity,
    blend_gamma: f32,
) -> (Vec<u8>, Vec<u8>) {
    let loc = Localization::default();
    let text = Text {
        spans: vec![TextSpan::lit(SAMPLE).with_style(TextStyle {
            font_px: px,
            color: srgb(pol.fg),
            ..Default::default()
        })],
        wrap: WrapMode::None,
        ..Default::default()
    };
    let shaped = sys.shape(&text, 1.0, None, &loc);
    let mut renderer = UiRenderer::default();
    renderer.settings.blend_gamma = blend_gamma;
    let mut quads = vec![UiQuad::fill([0.0, 0.0], [W as f32, H as f32], srgb(pol.bg))];
    // Reference: coverage accumulated per pixel, then composited in sRGB space.
    let mut coverage = vec![0.0f32; (W * H) as usize];
    let (x0, y0) = (8i32, 6i32);
    for glyph in &shaped.glyphs {
        let key = glyph.cache_key;
        let image = sys.glyph_image(key).expect("glyph image");
        let Some((place, offset)) = renderer.atlas_mut().get_or_insert(key, || Some(image.clone())) else {
            continue;
        };
        let gx = x0 + glyph.pen[0] + offset[0];
        let gy = y0 + glyph.pen[1] + offset[1];
        quads.push(UiQuad {
            uv: [
                place.rect[0] as f32 / PAGE_SIZE as f32,
                place.rect[1] as f32 / PAGE_SIZE as f32,
                (place.rect[0] + place.rect[2]) as f32 / PAGE_SIZE as f32,
                (place.rect[1] + place.rect[3]) as f32 / PAGE_SIZE as f32,
            ],
            texture: TextureRef::MonoAtlas(place.page),
            ..UiQuad::fill(
                [gx as f32, gy as f32],
                [place.rect[2] as f32, place.rect[3] as f32],
                srgb(pol.fg),
            )
        });
        let (iw, ih) = (image.placement.width as i32, image.placement.height as i32);
        for y in 0..ih {
            for x in 0..iw {
                let (px_x, px_y) = (gx + x, gy + y);
                if px_x < 0 || px_y < 0 || px_x >= W as i32 || px_y >= H as i32 {
                    continue;
                }
                let c = f32::from(image.data[(y * iw + x) as usize]) / 255.0;
                let slot = &mut coverage[(px_y as u32 * W + px_x as u32) as usize];
                *slot = 1.0 - (1.0 - *slot) * (1.0 - c);
            }
        }
    }
    renderer.set_quads(quads);
    let target = OffscreenTarget::new(device, W, H);
    target.clear(device, [0.0, 0.0, 0.0, 1.0]);
    target.draw_ui(device, &mut renderer);
    let gpu = target.read(device).expect("readback");
    let mut reference = Vec::with_capacity((W * H * 4) as usize);
    for c in coverage {
        for ch in 0..3 {
            let (f, b) = (f32::from(pol.fg[ch]), f32::from(pol.bg[ch]));
            reference.push((b + (f - b) * c).round() as u8);
        }
        reference.push(255);
    }
    (gpu, reference)
}

/// (ink ratio, edge error) of `gpu` against `reference` (green channel).
fn metrics(gpu: &[u8], reference: &[u8], pol: &Polarity) -> (f32, f32) {
    let (f, b) = (f32::from(pol.fg[1]), f32::from(pol.bg[1]));
    let ink = |v: u8| ((f32::from(v) - b) / (f - b)).clamp(0.0, 1.0);
    let (mut sum_gpu, mut sum_ref, mut sum_err) = (0.0f32, 0.0f32, 0.0f32);
    for (g, r) in gpu.as_chunks::<4>().0.iter().zip(reference.as_chunks::<4>().0) {
        let (ig, ir) = (ink(g[1]), ink(r[1]));
        sum_gpu += ig;
        sum_ref += ir;
        sum_err += (ig - ir).abs();
    }
    (sum_gpu / sum_ref.max(1e-6), sum_err / sum_ref.max(1e-6))
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn text_weight_and_edges_match_gamma_space_reference() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let mut sys = TextSystem::embedded_only();
    let mut failures = Vec::new();
    for px in [11.0f32, 14.0, 18.0, 28.0] {
        for pol in &POLARITIES {
            let (gpu, reference) =
                render_pair(&device, &mut sys, px, pol, regia_ui::render::DEFAULT_BLEND_GAMMA);
            let (ratio, err) = metrics(&gpu, &reference, pol);
            let (gpu_lin, _) = render_pair(&device, &mut sys, px, pol, 1.0);
            let (ratio_lin, err_lin) = metrics(&gpu_lin, &reference, pol);
            println!(
                "{px:>4} px {:<14} ink ratio {ratio:.3} edge err {err:.3}   (linear blend: ratio {ratio_lin:.3} err {err_lin:.3})",
                pol.name
            );
            if !(0.95..=1.05).contains(&ratio) || err > 0.06 {
                failures.push(format!(
                    "{px} px {}: ratio {ratio:.3}, edge err {err:.3}",
                    pol.name
                ));
            }
            if let Ok(dir) = regia_core::brand::env_var("REGIA_EVIDENCE_DIR") {
                std::fs::create_dir_all(&dir).expect("evidence dir");
                for (tag, px_data) in [("gpu", &gpu), ("ref", &reference), ("linear", &gpu_lin)] {
                    let img = image::RgbaImage::from_raw(W, H, px_data.clone()).expect("pixels");
                    img.save(std::path::Path::new(&dir).join(format!("text_{}_{px}_{tag}.png", pol.name)))
                        .expect("save");
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "text drifts from the gamma-space reference: {failures:?}"
    );
}

#[test]
fn linear_blending_is_detectably_thin_for_dark_text() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // The metric must discriminate: plain linear-light compositing thins dark text.
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let mut sys = TextSystem::embedded_only();
    let pol = &POLARITIES[1];
    let (gpu, reference) = render_pair(&device, &mut sys, 14.0, pol, 1.0);
    let (ratio, _) = metrics(&gpu, &reference, pol);
    assert!(
        ratio < 0.92,
        "linear blending ink ratio {ratio:.3}: metric no longer sees the thinning"
    );
}
