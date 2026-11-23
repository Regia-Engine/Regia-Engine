// SPDX-License-Identifier: MPL-2.0
//! Spatiotemporal blue noise in a frame (ADR-2075), on Desktop High's stochastic passes: stochastic
//! SSR, SSGI and SSAO.
//!
//! The knob off is the old frame. The knob on is another frame, and the same one every run, reaches
//! each pass by itself, and leaves less grain in the resolved frames.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, ScreenPassReport, SsaoMode,
};

const W: u32 = 128;
const H: u32 = 96;

fn scene(taa: bool) -> LabScene {
    let (fv, fi) = regia_render::box_mesh([6.0, 0.05, 6.0]);
    let (sv, si) = regia_render::uv_sphere(0.5, 32, 16);
    let (bv, bi) = regia_render::box_mesh([0.6, 0.6, 0.6]);
    let post = PostSettings {
        exposure_ev: 11.0,
        taa,
        ..PostSettings::golden_deterministic()
    };
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 1.2, 3.5],
        target: [0.0, 0.3, 0.0],
        fov_y: 0.8,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 11.0,
        ambient_lux: 400.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new([0.1, -0.4, -0.9], [1.0, 0.97, 0.92], 30_000.0),
        local_lights: Vec::new(),
        materials: vec![
            // A rough metal floor: stochastic SSR draws its rays from the noise.
            LabMaterial {
                base_color: [0.9, 0.9, 0.9, 1.0],
                metallic: 1.0,
                roughness: 0.3,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.9, 0.35, 0.08, 1.0],
                roughness: 0.5,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![
            LabMesh {
                vertices: regia_render::translate_mesh(&fv, [0.0, -0.05, 0.0]),
                indices: fi,
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: regia_render::translate_mesh(&sv, [0.0, 0.5, 0.0]),
                indices: si,
                material: 1,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: regia_render::translate_mesh(&bv, [-1.2, 0.6, -0.4]),
                indices: bi,
                material: 1,
                ..LabMesh::unskinned()
            },
        ],
    }
}

fn desktop_high(blue_noise: bool) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.volumetric_clouds = false;
    q.volumetric_fog = false;
    q.blue_noise = blue_noise;
    q
}

/// The display bytes of frame `frames` of one renderer.
fn last_frame(
    device: &RenderDevice,
    lab: &LabScene,
    quality: FrameQuality,
    frames: u32,
) -> (Vec<u8>, FrameRenderer) {
    let scene = lab.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    let mut pixels = Vec::new();
    for _ in 0..frames {
        pixels = renderer.render_offscreen(device, &scene).expect("frame").pixels;
    }
    (pixels, renderer)
}

#[test]
fn off_is_unchanged_and_on_is_another_frame_that_repeats() {
    let device = RenderDevice::headless().expect("device");
    let lab = scene(false);
    let (off, renderer) = last_frame(&device, &lab, desktop_high(false), 4);
    assert!(!renderer.blue_noise_active());
    assert_eq!(
        last_frame(&device, &lab, desktop_high(false), 4).0,
        off,
        "off repeats"
    );
    let (on, renderer) = last_frame(&device, &lab, desktop_high(true), 4);
    assert!(renderer.blue_noise_active());
    assert_ne!(on, off, "the knob reaches the frame");
    assert_eq!(
        last_frame(&device, &lab, desktop_high(true), 4).0,
        on,
        "on repeats"
    );
    // A renderer can switch: back off it draws the plain passes again and repeats their numbers.
    let frame_on = lab.as_frame_scene_with_quality(desktop_high(true));
    let frame_off = lab.as_frame_scene_with_quality(desktop_high(false));
    let mut switched = FrameRenderer::new();
    switched.render_offscreen(&device, &frame_on).expect("on");
    assert!(switched.blue_noise_active());
    switched.render_offscreen(&device, &frame_off).expect("off");
    assert!(!switched.blue_noise_active());
}

/// One frame's linear HDR, the input of the post stack.
fn hdr(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    lab: &LabScene,
    quality: FrameQuality,
) -> Vec<[f32; 3]> {
    let scene = lab.as_frame_scene_with_quality(quality);
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, &scene).expect("frame");
    renderer
        .take_hdr_capture(device)
        .expect("hdr readback")
        .expect("the post stack ran")
        .rgb
}

/// What the resolved frames show over `FRAMES` frames of a static view, over the flat pixels (the
/// camera jitter's aliasing on edges is the same with either noise, and larger): `grain` is the RMS
/// of one frame against the run's own mean, `flicker` the RMS between consecutive frames, both
/// relative to the image.
struct Stability {
    grain: f64,
    flicker: f64,
    pixels: usize,
}

const FRAMES: usize = 48;

fn stability(device: &RenderDevice, lab: &LabScene, blue_noise: bool) -> Stability {
    let quality = desktop_high(blue_noise);
    let mut renderer = FrameRenderer::new();
    let (w, h) = (W as usize, H as usize);
    for _ in 0..16 {
        hdr(device, &mut renderer, lab, quality);
    }
    let frames: Vec<Vec<[f32; 3]>> = (0..FRAMES)
        .map(|_| hdr(device, &mut renderer, lab, quality))
        .collect();
    let luma = |c: [f32; 3]| 0.2126 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.0722 * f64::from(c[2]);
    let mut mean = vec![0.0f64; w * h];
    for f in &frames {
        for (m, c) in mean.iter_mut().zip(f) {
            *m += luma(*c) / FRAMES as f64;
        }
    }
    // Flat: the mean moves less than 10 % across the pixel's 5 × 5 neighborhood.
    let flat = |x: usize, y: usize| {
        let (mut lo, mut hi) = (f64::MAX, 0.0f64);
        for yy in y - 2..=y + 2 {
            for xx in x - 2..=x + 2 {
                let v = mean[yy * w + xx];
                lo = lo.min(v);
                hi = hi.max(v);
            }
        }
        hi > 0.0 && hi - lo < 0.1 * hi
    };
    let (mut grain, mut flicker, mut level, mut pixels) = (0.0f64, 0.0f64, 0.0f64, 0usize);
    for y in 2..h - 2 {
        for x in 2..w - 2 {
            if !flat(x, y) {
                continue;
            }
            let i = y * w + x;
            pixels += 1;
            level += mean[i] * mean[i];
            for (t, f) in frames.iter().enumerate() {
                let v = luma(f[i]);
                grain += (v - mean[i]).powi(2) / FRAMES as f64;
                if t > 0 {
                    flicker += (v - luma(frames[t - 1][i])).powi(2) / (FRAMES - 1) as f64;
                }
            }
        }
    }
    Stability {
        grain: (grain / level.max(1e-9)).sqrt(),
        flicker: (flicker / level.max(1e-9)).sqrt(),
        pixels,
    }
}

/// The grain of a resolved frame drops, and it flickers no more than before. The same scene and the
/// same temporal resolve; only the numbers differ.
#[test]
fn resolved_frames_have_less_grain_with_blue_noise() {
    let device = RenderDevice::headless().expect("device");
    let lab = scene(true);
    let off = stability(&device, &lab, false);
    let on = stability(&device, &lab, true);
    eprintln!(
        "flat pixels {} / {}: grain off {:.5}, on {:.5}; flicker off {:.5}, on {:.5}",
        off.pixels, on.pixels, off.grain, on.grain, off.flicker, on.flicker
    );
    assert!(off.pixels > 1000 && on.pixels > 1000);
    assert!(on.grain < off.grain, "grain {} vs {}", on.grain, off.grain);
    assert!(
        on.flicker < off.flicker * 1.1,
        "flicker {} vs {}",
        on.flicker,
        off.flicker
    );
}

/// Each site by itself: with the other two passes off, the knob still changes the frame.
#[test]
fn every_site_takes_the_table_by_itself() {
    let device = RenderDevice::headless().expect("device");
    let lab = scene(false);
    type Site = (&'static str, fn(&mut FrameQuality), fn(&ScreenPassReport) -> bool);
    let sites: [Site; 3] = [
        (
            "ssao",
            |q| {
                q.ssr = false;
                q.screen_space_gi = false;
            },
            |r| r.ssao,
        ),
        (
            "ssgi",
            |q| {
                q.ssr = false;
                q.screen_space_gi = true;
                q.ssao = SsaoMode::Off;
            },
            |r| r.ssgi_gather,
        ),
        (
            "sssr",
            |q| {
                q.screen_space_gi = false;
                q.ssao = SsaoMode::Off;
            },
            |r| r.sssr,
        ),
    ];
    for (name, tune, ran) in sites {
        let quality = |on: bool| {
            let mut q = desktop_high(on);
            tune(&mut q);
            q
        };
        let (off, renderer) = last_frame(&device, &lab, quality(false), 4);
        assert!(
            ran(&renderer.screen_pass_report()),
            "{name} ran with the knob off"
        );
        let (on, renderer) = last_frame(&device, &lab, quality(true), 4);
        assert!(ran(&renderer.screen_pass_report()), "{name} ran with the knob on");
        let moved = off.iter().zip(&on).filter(|(a, b)| a != b).count();
        eprintln!("{name}: {moved} of {} bytes moved", off.len());
        assert!(moved > 0, "{name}: the knob reaches the frame");
    }
}
