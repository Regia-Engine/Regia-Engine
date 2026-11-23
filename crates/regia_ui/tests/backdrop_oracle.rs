// SPDX-License-Identifier: MPL-2.0
//! The GPU backdrop blur against [`cpu_backdrop_blur`], the ideal Gaussian on the CPU (ADR-1836,
//! ADR-2129). Needs a software adapter (lavapipe). Deterministic: a fixed frame, no clock.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_render::RenderDevice;
use regia_ui::{OffscreenTarget, TextureRef, UiColor, UiQuad, UiRenderer, cpu_backdrop_blur};

fn device() -> RenderDevice {
    match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    }
}

fn linear(byte: u8) -> f32 {
    let v = f32::from(byte) / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn encode(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    255.0
        * if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        }
}

/// One blurred quad over `frame` (sRGB RGBA8), read back as sRGB RGBA8.
fn gpu_blur(frame: &[u8], size: u32, rect: [f32; 4], sigma: f32) -> Vec<u8> {
    let device = device();
    let target = OffscreenTarget::new(&device, size, size);
    assert!(target.upload(&device, frame));
    let mut renderer = UiRenderer::default();
    renderer.set_quads(vec![UiQuad {
        texture: TextureRef::Backdrop,
        blur: sigma,
        ..UiQuad::fill([rect[0], rect[1]], [rect[2], rect[3]], UiColor::WHITE)
    }]);
    target.draw_ui(&device, &mut renderer);
    let stats = renderer.backdrop_stats();
    assert_eq!(
        (stats.quads, stats.blurred, stats.skipped),
        (1, 1, 0),
        "{stats:?}"
    );
    target.read(&device).expect("readback")
}

/// The oracle's answer in sRGB bytes (float, so a comparison can see sub-byte differences).
fn cpu_blur(frame: &[u8], size: u32, rect: [f32; 4], sigma: f32) -> Vec<[f32; 4]> {
    let mut pixels: Vec<[f32; 4]> = frame
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| [linear(p[0]), linear(p[1]), linear(p[2]), f32::from(p[3]) / 255.0])
        .collect();
    cpu_backdrop_blur(&mut pixels, size, size, rect, sigma);
    pixels
        .iter()
        .map(|p| [encode(p[0]), encode(p[1]), encode(p[2]), p[3] * 255.0])
        .collect()
}

/// A frame with what a real game has behind a panel: soft gradients, hard edges, fine detail.
fn scene_frame(size: u32) -> Vec<u8> {
    let mut seed = 0x2545_f491u32;
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed >> 24) as u8
    };
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let grain = noise() / 8;
            let wave = ((x as f32 * 0.07).sin() * (y as f32 * 0.05).cos() * 0.5 + 0.5) * 180.0;
            let r = wave as u8 + grain;
            let g = (x * 200 / size) as u8 + grain;
            let b = if (x / 37 + y / 23).is_multiple_of(2) {
                200
            } else {
                30
            };
            out.extend_from_slice(&[r, g, b, 255]);
        }
    }
    out
}

/// (mean, peak) absolute difference in sRGB bytes between the GPU image and the oracle in `rect`.
fn compare(gpu: &[u8], cpu: &[[f32; 4]], size: u32, rect: [u32; 4]) -> (f32, f32) {
    let (mut sum, mut peak, mut count) = (0.0f32, 0.0f32, 0u32);
    for y in rect[1]..rect[1] + rect[3] {
        for x in rect[0]..rect[0] + rect[2] {
            let at = (y * size + x) as usize;
            for c in 0..3 {
                let d = (f32::from(gpu[at * 4 + c]) - cpu[at][c]).abs();
                sum += d;
                peak = peak.max(d);
                count += 1;
            }
        }
    }
    (sum / count as f32, peak)
}

fn assert_matches_oracle(sigma: f32, max_mean: f32, max_peak: f32) {
    const SIZE: u32 = 256;
    let frame = scene_frame(SIZE);
    let rect = [48.0, 64.0, 160.0, 128.0];
    let gpu = gpu_blur(&frame, SIZE, rect, sigma);
    let cpu = cpu_blur(&frame, SIZE, rect, sigma);
    let (mean, peak) = compare(&gpu, &cpu, SIZE, [48, 64, 160, 128]);
    println!("sigma {sigma}: mean {mean:.3} peak {peak:.3} sRGB bytes");
    assert!(
        mean <= max_mean,
        "sigma {sigma}: mean difference {mean} > {max_mean}"
    );
    assert!(
        peak <= max_peak,
        "sigma {sigma}: peak difference {peak} > {max_peak}"
    );
    // Outside the quad the frame is untouched.
    for y in 0..SIZE {
        for x in [0u32, 47, 208, 255] {
            let at = ((y * SIZE + x) * 4) as usize;
            assert_eq!(gpu[at..at + 3], frame[at..at + 3], "{x},{y}");
        }
    }
}

#[test]
fn full_resolution_blur_matches_the_oracle_to_rounding() {
    assert_matches_oracle(1.5, 0.7, 1.6);
    assert_matches_oracle(4.0, 0.7, 1.6);
}

#[test]
fn half_resolution_blur_matches_the_oracle() {
    assert_matches_oracle(5.0, 1.5, 6.0);
    assert_matches_oracle(16.0, 1.0, 3.0);
}

#[test]
fn quarter_resolution_blur_matches_the_oracle() {
    assert_matches_oracle(17.0, 1.5, 4.0);
    assert_matches_oracle(32.0, 1.5, 4.0);
}

/// A 1 px line behind a panel must not flicker as it moves: at factor 4 the energy that reaches the
/// panel center has to stay the same from row to row and column to column.
#[test]
fn a_thin_line_keeps_its_brightness_as_it_moves_under_a_wide_blur() {
    const SIZE: u32 = 192;
    const SIGMA: f32 = 20.0;
    let rect = [32.0, 32.0, 128.0, 128.0];
    let lit = |horizontal: bool, at: u32| -> Vec<u8> {
        let mut frame = vec![0u8; (SIZE * SIZE * 4) as usize];
        for i in 0..SIZE {
            let (x, y) = if horizontal { (i, at) } else { (at, i) };
            let p = ((y * SIZE + x) * 4) as usize;
            frame[p..p + 3].copy_from_slice(&[255, 255, 255]);
        }
        for a in frame.iter_mut().skip(3).step_by(4) {
            *a = 255;
        }
        frame
    };
    let center = (96 * SIZE + 96) as usize * 4;
    for horizontal in [true, false] {
        let values: Vec<f32> = (92..100)
            .map(|at| linear(gpu_blur(&lit(horizontal, at), SIZE, rect, SIGMA)[center]))
            .collect();
        let oracle = cpu_blur(&lit(horizontal, 96), SIZE, rect, SIGMA)[center / 4][0];
        let expected = {
            let v = oracle / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        let (lo, hi) = values
            .iter()
            .fold((f32::MAX, 0.0f32), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
        println!("horizontal {horizontal}: {values:?}, oracle {expected}");
        assert!(
            lo > expected * 0.7,
            "a line vanished: {values:?}, oracle {expected}"
        );
        assert!(
            hi < expected * 1.3,
            "a line doubled: {values:?}, oracle {expected}"
        );
    }
}

/// Two panels of different sigma classes in one frame each get their own blur, on the first frame
/// and on the next (their scratch textures are kept per factor).
#[test]
fn panels_of_different_sigmas_in_one_frame_both_match_the_oracle() {
    const SIZE: u32 = 256;
    let frame = scene_frame(SIZE);
    let device = device();
    let target = OffscreenTarget::new(&device, SIZE, SIZE);
    let quad = |rect: [f32; 4], sigma| UiQuad {
        texture: TextureRef::Backdrop,
        blur: sigma,
        ..UiQuad::fill([rect[0], rect[1]], [rect[2], rect[3]], UiColor::WHITE)
    };
    let a = [8.0, 8.0, 64.0, 64.0];
    let b = [136.0, 136.0, 96.0, 96.0];
    let mut renderer = UiRenderer::default();
    renderer.set_quads(vec![quad(a, 3.0), quad(b, 24.0)]);
    // The oracle applies the same two blurs in the same order.
    let mut cpu: Vec<[f32; 4]> = frame
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| [linear(p[0]), linear(p[1]), linear(p[2]), f32::from(p[3]) / 255.0])
        .collect();
    cpu_backdrop_blur(&mut cpu, SIZE, SIZE, a, 3.0);
    cpu_backdrop_blur(&mut cpu, SIZE, SIZE, b, 24.0);
    let cpu: Vec<[f32; 4]> = cpu
        .iter()
        .map(|p| [encode(p[0]), encode(p[1]), encode(p[2]), p[3] * 255.0])
        .collect();
    for _ in 0..2 {
        assert!(target.upload(&device, &frame));
        target.draw_ui(&device, &mut renderer);
        let stats = renderer.backdrop_stats();
        assert_eq!((stats.blurred, stats.passes), (2, 5), "{stats:?}");
        assert!(
            stats.shaded_pixels > 0 && stats.samples > stats.shaded_pixels,
            "{stats:?}"
        );
        let gpu = target.read(&device).expect("readback");
        let (mean_a, _) = compare(&gpu, &cpu, SIZE, [8, 8, 64, 64]);
        let (mean_b, _) = compare(&gpu, &cpu, SIZE, [136, 136, 96, 96]);
        assert!(mean_a < 1.0 && mean_b < 2.0, "{mean_a} {mean_b}");
    }
}

/// Without a readable target the stats say so; nothing panics.
#[test]
fn a_quad_with_no_target_counts_as_skipped() {
    let device = device();
    let target = OffscreenTarget::new(&device, 64, 64);
    let mut renderer = UiRenderer::default();
    renderer.set_quads(vec![UiQuad {
        texture: TextureRef::Backdrop,
        blur: 4.0,
        ..UiQuad::fill([4.0, 4.0], [32.0, 32.0], UiColor::WHITE)
    }]);
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
        64,
        64,
    );
    device.queue.submit([encoder.finish()]);
    let stats = renderer.backdrop_stats();
    assert_eq!(
        (stats.quads, stats.blurred, stats.skipped),
        (1, 0, 1),
        "{stats:?}"
    );
    assert_eq!(stats.samples, 0);
}
