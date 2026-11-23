// SPDX-License-Identifier: MPL-2.0
//! Backdrop blur (ADR-1836): a `TextureRef::Backdrop` quad blurs the frame drawn so far, only
//! inside its rect, and later quads draw over the result. Needs a software adapter (lavapipe).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_render::RenderDevice;
use regia_ui::{OffscreenTarget, TextureRef, UiColor, UiQuad, UiRenderer};

const SIZE: u32 = 128;

fn device() -> RenderDevice {
    match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    }
}

fn backdrop(pos: [f32; 2], size: [f32; 2], sigma: f32, radius: f32) -> UiQuad {
    UiQuad {
        texture: TextureRef::Backdrop,
        blur: sigma,
        corner_radius: [radius; 4],
        ..UiQuad::fill(pos, size, UiColor::WHITE)
    }
}

/// Renders `quads` over a black-left / white-right frame and returns the red channel per pixel.
fn render_split(quads: Vec<UiQuad>) -> Vec<u8> {
    let device = device();
    let target = OffscreenTarget::new(&device, SIZE, SIZE);
    let pixels: Vec<u8> = (0..SIZE * SIZE)
        .flat_map(|i| {
            let v = if i % SIZE < SIZE / 2 { 0 } else { 255 };
            [v, v, v, 255]
        })
        .collect();
    assert!(target.upload(&device, &pixels));
    let mut renderer = UiRenderer::default();
    renderer.set_quads(quads);
    target.draw_ui(&device, &mut renderer);
    let out = target.read(&device).expect("readback");
    out.as_chunks::<4>().0.iter().map(|p| p[0]).collect()
}

fn at(img: &[u8], x: u32, y: u32) -> u8 {
    img[(y * SIZE + x) as usize]
}

#[test]
fn a_hard_edge_under_the_quad_becomes_a_smooth_ramp() {
    let img = render_split(vec![backdrop([32.0, 32.0], [64.0, 64.0], 4.0, 0.0)]);
    // Inside the quad the step is a monotonic ramp that crosses the middle at the edge.
    let row: Vec<u8> = (36..92).map(|x| at(&img, x, 64)).collect();
    assert!(row.windows(2).all(|w| w[0] <= w[1]), "not monotonic: {row:?}");
    assert!(
        at(&img, 63, 64) > 20 && at(&img, 63, 64) < 235,
        "{}",
        at(&img, 63, 64)
    );
    assert!(
        at(&img, 64, 64) > 20 && at(&img, 64, 64) < 235,
        "{}",
        at(&img, 64, 64)
    );
    let mid = at(&img, 64, 64);
    assert!((150..=225).contains(&mid), "linear-light blur of the step: {mid}");
    // Far from the edge (> 3σ) the colours are unchanged.
    assert!(at(&img, 40, 64) <= 2 && at(&img, 90, 64) >= 253);
    // Outside the quad nothing changed: the hard edge is still hard.
    assert_eq!((at(&img, 63, 10), at(&img, 64, 10)), (0, 255));
    assert_eq!((at(&img, 63, 120), at(&img, 64, 120)), (0, 255));
}

#[test]
fn a_large_sigma_uses_the_downsampled_path_and_stays_symmetric() {
    let img = render_split(vec![backdrop([8.0, 8.0], [112.0, 112.0], 20.0, 0.0)]);
    let row: Vec<u8> = (10..118).map(|x| at(&img, x, 64)).collect();
    assert!(row.windows(2).all(|w| w[0] <= w[1]), "not monotonic: {row:?}");
    // The ramp is wide: well off the edge on both sides it has not saturated.
    assert!(at(&img, 50, 64) > 3, "{}", at(&img, 50, 64));
    assert!(at(&img, 78, 64) < 252, "{}", at(&img, 78, 64));
    // Symmetric about the edge (sum of linear values near 1): sRGB 0.5 is about 188.
    let (a, b) = (at(&img, 52, 64) as i32, at(&img, 75, 64) as i32);
    assert!(a > 0 && b < 255 && b > a);
}

#[test]
fn quads_before_the_backdrop_are_blurred_and_quads_after_draw_over_it() {
    let white = UiQuad::fill([60.0, 20.0], [8.0, 8.0], UiColor([1.0, 1.0, 1.0, 1.0]));
    let tint = UiQuad::fill([40.0, 90.0], [20.0, 10.0], UiColor([1.0, 0.0, 0.0, 1.0]));
    let img = render_split(vec![white, backdrop([20.0, 8.0], [90.0, 100.0], 3.0, 0.0), tint]);
    // The small white square sits on the black half and is smeared by the blur.
    let centre = at(&img, 63, 23);
    assert!(centre > 80 && centre < 250, "square centre {centre}");
    // The red tint after the blur is untouched by it.
    assert_eq!(at(&img, 50, 95), 255, "red channel of the tint");
}

#[test]
fn a_rounded_corner_leaves_the_frame_untouched_outside_the_curve() {
    // A step at x = 64 reaches the quad's top-left corner region only through the blur; with a
    // big radius the corner pixel is outside the shape and keeps the sharp source.
    let img = render_split(vec![backdrop([56.0, 56.0], [40.0, 40.0], 6.0, 20.0)]);
    // (56, 56) is the far corner of a 20 px radius: outside the rounded rect, still black/white.
    assert_eq!(at(&img, 56, 56), 0);
    // The vertical edge midpoint of the quad (inside) is blurred.
    let inside = at(&img, 64, 76);
    assert!(inside > 20 && inside < 235, "{inside}");
}

#[test]
fn a_quad_off_the_target_or_without_sigma_draws_nothing() {
    let none = render_split(vec![]);
    let off = render_split(vec![backdrop([500.0, 500.0], [30.0, 30.0], 4.0, 0.0)]);
    let zero = render_split(vec![backdrop([20.0, 20.0], [90.0, 90.0], 0.0, 0.0)]);
    assert_eq!(none, off);
    assert_eq!(none, zero);
}

#[test]
fn without_the_target_texture_the_blur_is_skipped() {
    let device = device();
    let target = OffscreenTarget::new(&device, SIZE, SIZE);
    let pixels: Vec<u8> = (0..SIZE * SIZE)
        .flat_map(|i| {
            let v = if i % SIZE < SIZE / 2 { 0 } else { 255 };
            [v, v, v, 255]
        })
        .collect();
    assert!(target.upload(&device, &pixels));
    let mut renderer = UiRenderer::default();
    renderer.set_quads(vec![backdrop([32.0, 32.0], [64.0, 64.0], 4.0, 0.0)]);
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("no target"),
        });
    renderer.draw(
        &device,
        &mut encoder,
        target.view(),
        OffscreenTarget::FORMAT,
        SIZE,
        SIZE,
    );
    device.queue.submit([encoder.finish()]);
    let out = target.read(&device).expect("readback");
    let r = |x: u32, y: u32| out[((y * SIZE + x) * 4) as usize];
    assert_eq!(
        (r(63, 64), r(64, 64)),
        (0, 255),
        "hard edge survives without a texture"
    );
}
