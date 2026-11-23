// SPDX-License-Identifier: MPL-2.0
//! Overlay compositing guards (lane ui-look): translucent fills match sRGB-space (CSS / design
//! tool) compositing, destination alpha stays opaque (premultiplied blending), GPU clipping keeps
//! the visible rounded corners, 9-slice joins have no seam pixels at fractional rects and
//! scales, shadows follow CSS `box-shadow` semantics, and gradient angles point the documented way.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_render::RenderDevice;
use regia_ui::draw::{UiQuad, nine_slice};
use regia_ui::{OffscreenTarget, UiColor, UiRenderer};

/// The process's one shared device: a device per parallel test hung the NVIDIA driver under load.
fn device() -> RenderDevice {
    match RenderDevice::shared_headless() {
        Ok(d) => d,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    }
}

/// Renders `quads` over a uniform sRGB backdrop and returns the pixels.
fn render(
    device: &RenderDevice,
    renderer: &mut UiRenderer,
    w: u32,
    h: u32,
    backdrop: [u8; 3],
    quads: Vec<UiQuad>,
) -> Vec<u8> {
    let target = OffscreenTarget::new(device, w, h);
    let bytes: Vec<u8> = (0..w * h)
        .flat_map(|_| [backdrop[0], backdrop[1], backdrop[2], 255])
        .collect();
    assert!(target.upload(device, &bytes));
    renderer.set_quads(quads);
    target.draw_ui(device, renderer);
    target.read(device).expect("readback")
}

fn px(p: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * w + x) * 4) as usize;
    [p[i], p[i + 1], p[i + 2], p[i + 3]]
}

/// (fill sRGB, alpha, backdrop sRGB, tolerance in 8-bit steps or `None` = vs linear blending).
type Case = ([u8; 3], f32, [u8; 3], Option<i32>);

#[test]
fn translucent_fills_match_srgb_space_compositing() {
    let device = device();
    // (fill sRGB, alpha, backdrop sRGB, tolerance in 8-bit steps). The remap is exact for black
    // fills and white-over-black under a pure γ 2.2 curve; sRGB's linear toe moves dark
    // backdrops a few steps. Near-black HUD panels stay within the remap's analysed bound for
    // fills ≤ 10 % sRGB (8.3 / 255 worst case over a grid of backdrops and alphas; plain
    // linear blending is off by up to 72 / 255 there).
    // Saturated mid-light accents (`tol` = None) cannot be exact without knowing the backdrop;
    // there the error must stay well under plain linear blending's.
    let cases: [Case; 6] = [
        ([12, 17, 23], 0.82, [230, 140, 70], Some(8)),
        ([12, 17, 23], 0.55, [250, 250, 250], Some(8)),
        ([0, 0, 0], 0.4, [128, 180, 60], Some(5)),
        ([255, 255, 255], 0.5, [0, 0, 0], Some(2)),
        ([233, 185, 73], 0.5, [16, 20, 26], None),
        // Known limitation: a light fill over a *bright* backdrop is where the "light colours sit
        // on dark panels" assumption is wrong — 16.8 / 255 on red here, where linear blending
        // happens to be 9.3 off. Bounded so a regression shows; see reports/ui-look.md.
        ([95, 183, 255], 0.35, [200, 120, 60], Some(20)),
    ];
    let dec = |v: u8| regia_ui::UiColor::from_srgb8(v, v, v, 255).0[0];
    let enc = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        255.0
            * if v <= 0.003_130_8 {
                v * 12.92
            } else {
                1.055 * v.powf(1.0 / 2.4) - 0.055
            }
    };
    for (fill, alpha, back, tol) in cases {
        let mut r = UiRenderer::default();
        let color = UiColor::from_srgb8(fill[0], fill[1], fill[2], 255).with_alpha(alpha);
        let out = render(
            &device,
            &mut r,
            32,
            32,
            back,
            vec![UiQuad::fill([0.0, 0.0], [32.0, 32.0], color)],
        );
        let got = px(&out, 32, 16, 16);
        for ch in 0..3 {
            let css = f32::from(fill[ch]) * alpha + f32::from(back[ch]) * (1.0 - alpha);
            let err = (f32::from(got[ch]) - css).abs();
            let linear = enc(dec(fill[ch]) * alpha + dec(back[ch]) * (1.0 - alpha));
            let linear_err = (linear - css).abs();
            println!(
                "fill {fill:?} a={alpha} over {back:?} ch{ch}: got {} css {css:.1} (err {err:.1}; linear blend err {linear_err:.1})",
                got[ch]
            );
            let ok = match tol {
                Some(t) => err <= t as f32 + 0.5,
                None => err <= (0.6 * linear_err).max(3.0),
            };
            assert!(
                ok,
                "fill {fill:?} a={alpha} over {back:?}: channel {ch} = {}, CSS {css:.1}",
                got[ch]
            );
        }
        assert_eq!(got[3], 255, "premultiplied blending keeps an opaque frame opaque");
    }
}

#[test]
fn clipped_rounded_panel_keeps_visible_corners() {
    let device = device();
    let mut r = UiRenderer::default();
    let mut panel = UiQuad::fill([10.0, 10.0], [100.0, 60.0], UiColor::WHITE);
    panel.corner_radius = [16.0; 4];
    // Scrolled half out of a clip on the left: the right-hand corners stay rounded, the clip
    // edge is a hard vertical cut.
    let clipped = panel.clipped([60.0, 0.0, 200.0, 200.0]).unwrap();
    let out = render(&device, &mut r, 128, 80, [0, 0, 0], vec![clipped]);
    assert_eq!(px(&out, 128, 59, 40)[0], 0, "left of the clip is untouched");
    assert_eq!(
        px(&out, 128, 60, 40)[0],
        255,
        "clip edge is a hard cut at a pixel boundary"
    );
    assert_eq!(px(&out, 128, 60, 10)[0], 255, "no false corner at the clip edge");
    assert!(px(&out, 128, 109, 10)[0] < 8, "top-right corner stays rounded");
    assert!(px(&out, 128, 109, 69)[0] < 8, "bottom-right corner stays rounded");
    assert_eq!(px(&out, 128, 100, 40)[0], 255);
}

#[test]
fn nine_slice_joins_have_no_seam_pixels() {
    let device = device();
    // A uniform translucent image: any gap shows the backdrop, any overlap double-blends, any
    // texel bleed or mip halo changes the colour, so every covered pixel must be identical.
    let mut r = UiRenderer::default();
    let tex: Vec<u8> = (0..64 * 64).flat_map(|_| [200u8, 60, 40, 140]).collect();
    r.queue_image_rgba(9, 64, 64, tex);
    for (scale, rect) in [
        (0.7037f32, [10.3f32, 7.6, 141.45, 77.2]),
        (1.0, [5.5, 4.5, 120.0, 90.0]),
        (1.3333, [12.25, 3.75, 151.2, 99.9]),
        (2.0, [3.0, 2.0, 170.0, 110.0]),
    ] {
        let base = UiQuad::fill([rect[0], rect[1]], [rect[2], rect[3]], UiColor::WHITE);
        let quads = nine_slice(base, 9, [0.0, 0.0, 1.0, 1.0], [20.0; 4], [64, 64], scale);
        let out = render(&device, &mut r, 192, 128, [30, 60, 90], quads);
        let (x0, y0) = (rect[0].round() as u32, rect[1].round() as u32);
        let (x1, y1) = (
            (rect[0] + rect[2]).round() as u32,
            (rect[1] + rect[3]).round() as u32,
        );
        let expect = px(&out, 192, x0 + 1, y0 + 1);
        let mut bad = Vec::new();
        for y in 0..128 {
            for x in 0..192 {
                let inside = x >= x0 && x < x1 && y >= y0 && y < y1;
                let p = px(&out, 192, x, y);
                let want = if inside { expect } else { [30, 60, 90, 255] };
                if p != want && bad.len() < 8 {
                    bad.push((x, y, p));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "scale {scale}: seam / bleed pixels {bad:?} (expected {expect:?})"
        );
    }
}

#[test]
fn shadow_is_soft_and_not_under_its_caster() {
    let device = device();
    let mut r = UiRenderer::default();
    let mut shadow = UiQuad::fill([40.0, 40.0], [60.0, 40.0], UiColor::BLACK.with_alpha(0.8));
    shadow.blur = 12.0;
    shadow.corner_radius = [6.0; 4];
    let out = render(&device, &mut r, 140, 120, [200, 200, 200], vec![shadow]);
    let row: Vec<u8> = (0..140).map(|x| px(&out, 140, x, 60)[0]).collect();
    // Under the caster (offset 0): untouched (CSS clips the shadow to outside the box).
    assert_eq!(row[70], 200);
    // Outside: monotonic falloff back to the backdrop, darkest at the caster edge.
    let left: Vec<u8> = row[18..40].to_vec();
    assert!(
        left.windows(2).all(|w| w[0] >= w[1]),
        "falloff not monotonic: {left:?}"
    );
    assert!(row[39] < 140, "shadow too faint at the edge: {}", row[39]);
    assert!(row[20] > 190, "shadow does not fade out by 3 sigma: {}", row[20]);
    // A 1-px dither keeps the gradient from banding but stays within ±1 of a smooth falloff.
    assert!(row[0] == 200 && row[139] == 200);
}

#[test]
fn gradient_angles_point_as_documented() {
    let device = device();
    let mut r = UiRenderer::default();
    let quad = |angle: f32| UiQuad {
        color2: UiColor::WHITE,
        gradient: true,
        angle,
        ..UiQuad::fill([0.0, 0.0], [64.0, 64.0], UiColor::BLACK)
    };
    let down = render(&device, &mut r, 64, 64, [0, 0, 0], vec![quad(0.0)]);
    assert!(
        px(&down, 64, 32, 2)[0] < 40 && px(&down, 64, 32, 61)[0] > 215,
        "0° = top→bottom"
    );
    let right = render(&device, &mut r, 64, 64, [0, 0, 0], vec![quad(90.0)]);
    assert!(
        px(&right, 64, 2, 32)[0] < 40 && px(&right, 64, 61, 32)[0] > 215,
        "90° = left→right"
    );
}

#[test]
fn image_mips_keep_minified_images_stable() {
    // A 1-px checkerboard drawn at 1/4 size must average to mid grey (mips), not alias to
    // whichever texel the sample lands on.
    let device = device();
    let mut r = UiRenderer::default();
    let tex: Vec<u8> = (0..64u32 * 64)
        .flat_map(|i| {
            let v = if (i % 64 + i / 64) % 2 == 0 { 255u8 } else { 0 };
            [v, v, v, 255]
        })
        .collect();
    r.queue_image_rgba(3, 64, 64, tex);
    let quad = UiQuad {
        texture: regia_ui::TextureRef::Image(3),
        ..UiQuad::fill([0.0, 0.0], [16.0, 16.0], UiColor::WHITE)
    };
    let out = render(&device, &mut r, 16, 16, [0, 0, 0], vec![quad]);
    for y in 2..14 {
        for x in 2..14 {
            let v = px(&out, 16, x, y)[0];
            // Linear-light average of black and white = 0.5 → sRGB 188.
            assert!(
                (180..=196).contains(&v),
                "({x},{y}) = {v}: minified image aliases"
            );
        }
    }
}
