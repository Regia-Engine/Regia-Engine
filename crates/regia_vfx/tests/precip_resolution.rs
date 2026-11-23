// SPDX-License-Identifier: MPL-2.0
//! The precipitation's resolution choice (spec 99 § 5.7, ADR-3249) on lavapipe: rain drawn at
//! full, half and quarter resolution through the real frame path, against the full-resolution
//! frame as the reference.
//!
//! - `Full` is the frame it always was: the setting at `Full` renders the same bytes as no setting.
//! - Streak survival: of the pixels a full-resolution storm changes, how many a reduced choice
//!   still changes, and how much of the full frame's streak contrast it keeps. Printed per choice
//!   with the fill (fragments, from the CPU mirror) and fetch counts, for the report.
//! - A reduced target tests the scene depth in its fragment stage: rain behind a near wall stays
//!   off the wall (bar the composite's edge band).
//!
//! `REGIA_PRECIP_SHEET_DIR=<dir>` writes the frames as PNGs.

use std::sync::{Arc, Mutex};

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    QualityTier, RenderDevice,
};
use regia_vfx::precip::{PrecipState, PrecipitationGpu, rain_drop};
use regia_vfx::runtime::translucent_layer_with_precipitation;
use regia_vfx::{
    PrecipBudget, PrecipIntegrator, PrecipResolution, PrecipitationHandle, PrecipitationSettings, VfxGpu,
    VfxProfile, WeatherConfig, WeatherState,
};

const W: u32 = 480;
const H: u32 = 270;
const DT: f32 = 1.0 / 60.0;
const EYE: [f32; 3] = [0.0, 1.6, 8.0];
const TARGET: [f32; 3] = [0.0, 1.4, 0.0];
const FOV: f32 = 1.0;
/// A pixel the full-resolution storm changes by at least this (largest channel) is a streak pixel.
const STREAK: u8 = 4;

fn device() -> RenderDevice {
    RenderDevice::headless().unwrap_or_else(|err| panic!("no headless GPU (lavapipe?): {err}"))
}

fn quad(corners: [[f32; 3]; 4], n: [f32; 3]) -> (Vec<LabVertex>, Vec<u32>) {
    (
        corners
            .into_iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(p, n, uv))
            .collect(),
        vec![0, 2, 1, 0, 3, 2],
    )
}

/// A dark floor far out, and a near wall over the left quarter of the frame, 1.2 m from the eye.
fn scene() -> LabScene {
    let (floor_v, floor_i) = quad(
        [
            [-80.0, 0.0, -80.0],
            [80.0, 0.0, -80.0],
            [80.0, 0.0, 80.0],
            [-80.0, 0.0, 80.0],
        ],
        [0.0, 1.0, 0.0],
    );
    let (wall_v, wall_i) = quad(
        [
            [-3.0, -1.0, 6.8],
            [-0.62, -1.0, 6.8],
            [-0.62, 5.0, 6.8],
            [-3.0, 5.0, 6.8],
        ],
        [0.0, 0.0, 1.0],
    );
    LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: FOV,
        near: 0.1,
        far: 160.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 6_000.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -1.0, -0.4], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.04, 0.04, 0.05, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }],
        meshes: vec![
            LabMesh {
                vertices: floor_v,
                indices: floor_i,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: wall_v,
                indices: wall_i,
                ..LabMesh::unskinned()
            },
        ],
    }
}

fn settings(resolution: Option<PrecipResolution>) -> PrecipitationSettings {
    let config = WeatherConfig::default();
    PrecipitationSettings {
        enabled: true,
        resolution,
        ..PrecipitationSettings::from_config(&config)
    }
}

fn storm(resolution: Option<PrecipResolution>) -> PrecipState {
    let config = WeatherConfig::default();
    let rates = WeatherState::STORM.rates(&config);
    let budget = PrecipBudget::for_quality(QualityTier::High);
    let mut integrator = PrecipIntegrator::default();
    for _ in 0..10 {
        integrator.step(&rates, &config, [0.0, 0.0], &budget, DT);
    }
    PrecipState {
        integrator,
        rates,
        config,
        settings: settings(resolution),
        quality: QualityTier::High,
        active: true,
        clock_s: 1.0,
        ..PrecipState::default()
    }
}

/// One frame with `state`'s precipitation, or none.
fn render(device: &RenderDevice, state: Option<PrecipState>) -> Vec<u8> {
    let mut renderer = FrameRenderer::new();
    let gpu = VfxGpu::new(device, VfxProfile::default()).unwrap_or_else(|err| panic!("vfx: {err:?}"));
    let handle = PrecipitationHandle::default();
    if let Some(state) = state {
        if let Ok(mut slot) = handle.state.lock() {
            *slot = state;
        }
        if let Ok(mut slot) = handle.gpu.lock() {
            *slot = PrecipitationGpu::new(device, &VfxProfile::default()).ok();
        }
    }
    let layer = translucent_layer_with_precipitation(Arc::new(Mutex::new(gpu)), handle, device.queue.clone());
    renderer.set_translucent_layers(vec![layer]);
    renderer
        .set_precipitation_occlusion_budget(PrecipBudget::for_quality(QualityTier::High).occlusion, 16_384);
    let frame = renderer
        .render_offscreen(device, &scene().as_frame_scene_with_quality(FrameQuality::LAB))
        .unwrap_or_else(|err| panic!("frame: {err}"));
    frame.pixels
}

fn sheet(name: &str, pixels: &[u8]) {
    let Some(dir) = std::env::var_os("REGIA_PRECIP_SHEET_DIR") else {
        return;
    };
    let Ok(file) = std::fs::File::create(std::path::Path::new(&dir).join(name)) else {
        return;
    };
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(pixels);
    }
}

/// Per pixel, the largest channel change against the plain frame.
fn delta(plain: &[u8], rain: &[u8]) -> Vec<u8> {
    plain
        .as_chunks::<4>()
        .0
        .iter()
        .zip(rain.as_chunks::<4>().0)
        .map(|(a, b)| (0..3).map(|c| a[c].abs_diff(b[c])).max().unwrap_or(0))
        .collect()
}

/// Columns left of the wall's right edge (the wall covers x < this), with an `edge` band cut.
fn wall_columns(edge: u32) -> u32 {
    // The wall's right edge at x = -0.62, 1.2 m in front of the eye: its column.
    let half_w = (FOV * 0.5).tan() * (W as f32 / H as f32);
    let ndc = -0.62 / 1.2 / half_w;
    (((ndc * 0.5 + 0.5) * W as f32) as u32).saturating_sub(edge)
}

/// The fragments the storm's streaks shade at `resolution` (the CPU mirror's ribbons, projected:
/// screen length times the width widened to one pixel), and the drops in view.
fn mirror_fill(state: &PrecipState, resolution: PrecipResolution) -> (f64, u32) {
    let mut state = state.clone();
    let mut settings = state.settings;
    settings.resolution = Some(resolution);
    state.settings = settings;
    let forward = [TARGET[0] - EYE[0], TARGET[1] - EYE[1], TARGET[2] - EYE[2]];
    let frame = state.frame_at(EYE.map(f64::from), forward);
    let (_, th) = resolution.target_size(W, H);
    let px_per_m = th as f32 * 0.5 / (FOV * 0.5).tan();
    let f = {
        let l = (forward[0] * forward[0] + forward[1] * forward[1] + forward[2] * forward[2]).sqrt();
        forward.map(|v| v / l)
    };
    let count = frame.counts.rain.ceil() as u32;
    let mut fill = 0.0f64;
    let mut seen = 0;
    for i in 0..count {
        let drop = rain_drop(i, &frame);
        if drop.alpha <= 0.0 {
            continue;
        }
        let p = drop.position_m;
        let depth = p[0] * f[0] + p[1] * f[1] + p[2] * f[2];
        if depth < 0.1 {
            continue;
        }
        let v = drop.velocity_m_per_s;
        let speed = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        let length_m = (speed * frame.streak_exposure_s).max(0.02);
        let width_px = (drop.diameter_mm * 0.001 * px_per_m / depth).max(1.0);
        let length_px = (length_m * px_per_m / depth).max(1.0);
        fill += f64::from(width_px * length_px);
        seen += 1;
    }
    (fill, seen)
}

#[test]
fn reduced_resolutions_keep_the_streaks_they_can() {
    let device = device();
    let plain = render(&device, None);
    let default_full = render(&device, Some(storm(None)));
    let full = render(&device, Some(storm(Some(PrecipResolution::Full))));
    assert!(
        full == default_full,
        "Full at run time differs from the tier's Full"
    );
    sheet("res_full.png", &full);
    let reference = delta(&plain, &full);
    let open_x = wall_columns(0) + 8;
    let streak: Vec<usize> = (0..(W * H) as usize)
        .filter(|&i| (i as u32 % W) >= open_x && reference[i] >= STREAK)
        .collect();
    assert!(
        streak.len() > 500,
        "the storm drew {} streak pixels",
        streak.len()
    );
    let reference_contrast: f64 = streak.iter().map(|&i| f64::from(reference[i])).sum();
    let wall = wall_columns(6);
    let over_wall = |d: &[u8]| {
        (0..(W * H) as usize)
            .filter(|&i| (i as u32 % W) < wall && d[i] > 1)
            .count()
    };
    let full_wall = over_wall(&reference);
    let mut survival = Vec::new();
    for resolution in PrecipResolution::ALL {
        let frame = if resolution == PrecipResolution::Full {
            full.clone()
        } else {
            render(&device, Some(storm(Some(resolution))))
        };
        sheet(&format!("res_{}.png", resolution.name()), &frame);
        let d = delta(&plain, &frame);
        let kept = streak.iter().filter(|&&i| d[i] >= STREAK / 2).count() as f64 / streak.len() as f64;
        let contrast = streak.iter().map(|&i| f64::from(d[i])).sum::<f64>() / reference_contrast;
        let changed = d.iter().filter(|v| **v > 1).count();
        let wall_hits = over_wall(&d);
        let (fill, drops) = mirror_fill(&storm(None), resolution);
        let (tw, th) = resolution.target_size(W, H);
        let composite = if resolution == PrecipResolution::Full {
            0
        } else {
            u64::from(W * H)
        };
        println!(
            "{}: target {tw}x{th}; streak pixels kept {:.3}, streak contrast {:.3}, pixels changed {changed}, \
             over the wall {wall_hits} (full {full_wall}); fill ~{fill:.0} fragments over {drops} drops in view, \
             fetches: {fill:.0} scene-depth loads + {} occlusion loads (6 per streak) + {composite} composite samples",
            resolution.name(),
            kept,
            contrast,
            drops * 6,
        );
        if resolution != PrecipResolution::Full {
            assert!(
                changed > 500,
                "{}: the reduced pass drew nothing ({changed} pixels)",
                resolution.name()
            );
            // The fragment-stage depth test keeps the rain behind the wall off it (the composite's
            // bilinear band at the wall's edge is cut from the count).
            assert!(
                wall_hits <= full_wall + (H as usize) * 2,
                "{}: {wall_hits} wall pixels rained on (full: {full_wall})",
                resolution.name()
            );
        }
        survival.push((resolution, kept, contrast, fill));
    }
    let [
        (_, full_kept, _, full_fill),
        (_, half_kept, _, half_fill),
        (_, quarter_kept, _, quarter_fill),
    ] = survival[..]
    else {
        panic!("three choices");
    };
    assert!((full_kept - 1.0).abs() < 1e-9);
    assert!(
        half_kept > quarter_kept,
        "half {half_kept} kept fewer streak pixels than quarter {quarter_kept}"
    );
    assert!(
        half_fill < full_fill && quarter_fill < half_fill,
        "fill does not fall with the resolution"
    );
}

/// Frame time per choice at the MobileHigh budget (6,000 streaks, the 256² view), for the device
/// run that picks the Mobile tiers' default (ADR-3249). Wall clock over `REGIA_PRECIP_BENCH_FRAMES`
/// frames (default 60) at `REGIA_PRECIP_BENCH_SIZE` (default `1280x720`), readback included, so
/// compare the choices with each other and with `off`. On a phone (the lead's command, ADR-3249):
/// `./precip_resolution-<hash> bench --ignored --nocapture --test-threads=1`.
#[test]
#[ignore = "prints numbers; the device run"]
fn bench_precipitation_resolution() {
    let device = device();
    let frames: u32 = std::env::var("REGIA_PRECIP_BENCH_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let (width, height) = std::env::var("REGIA_PRECIP_BENCH_SIZE")
        .ok()
        .and_then(|v| {
            let (w, h) = v.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((1280u32, 720u32));
    let tier = QualityTier::MobileHigh;
    let mut lab = scene();
    lab.width = width;
    lab.height = height;
    let state = |resolution: PrecipResolution| {
        let mut state = storm(Some(resolution));
        state.quality = tier;
        state
    };
    let run = |label: &str, state: Option<PrecipState>| {
        let mut renderer = FrameRenderer::new();
        let gpu = VfxGpu::new(&device, VfxProfile::default()).unwrap_or_else(|err| panic!("vfx: {err:?}"));
        let handle = PrecipitationHandle::default();
        if let Some(state) = state {
            if let Ok(mut slot) = handle.state.lock() {
                *slot = state;
            }
            if let Ok(mut slot) = handle.gpu.lock() {
                *slot = PrecipitationGpu::new(&device, &VfxProfile::default()).ok();
            }
        }
        let layer =
            translucent_layer_with_precipitation(Arc::new(Mutex::new(gpu)), handle, device.queue.clone());
        renderer.set_translucent_layers(vec![layer]);
        renderer.set_precipitation_occlusion_budget(PrecipBudget::for_quality(tier).occlusion, 16_384);
        let scene = lab.as_frame_scene_with_quality(FrameQuality::LAB);
        for _ in 0..10 {
            let _ = renderer.render_offscreen(&device, &scene);
        }
        let started = std::time::Instant::now();
        for _ in 0..frames {
            let _ = renderer.render_offscreen(&device, &scene);
        }
        println!(
            "{label}: {:.3} ms a frame ({width}x{height}, {frames} frames, MobileHigh budget)",
            started.elapsed().as_secs_f64() * 1000.0 / f64::from(frames.max(1))
        );
    };
    run("off", None);
    for resolution in PrecipResolution::ALL {
        run(resolution.name(), Some(state(resolution)));
    }
}
