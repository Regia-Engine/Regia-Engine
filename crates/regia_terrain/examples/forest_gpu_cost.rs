// SPDX-License-Identifier: MPL-2.0
//! What a fast flythrough of a dense forest costs on the GPU (ADR-1503). The synthetic forest
//! (two canopy classes and an understory, stand-in trees with three LODs and crown parts) on its
//! terrain, a canopy-level path carved collision-free first, flown at one speed with motion LOD
//! and wake sway. Every frame the scatter field streams and collects as a host's would, and the
//! frame renderer draws terrain plus scatter (GPU-driven instancing, per-instance frustum and Hi-Z
//! cull) into an offscreen target.
//!
//! ```text
//! REGIA_GPU_INDEX=3 cargo run --release -p regia_terrain --example forest_gpu_cost
//! FOREST_GPU_SIZE=3840x2160 FOREST_GPU_PROFILE=DesktopUltra FOREST_SPEED=100 FOREST_MOTION=1 ...
//! ```
//!
//! Knobs (environment): `FOREST_GPU_SIZE` (default 2560x1440), `FOREST_GPU_PROFILE` (a `RenderProfilePreset`
//! name, default `DesktopUltra`), `FOREST_SPEED` (m/s, default 100), `FOREST_HEIGHT` (path height above
//! ground, m, default 10), `FOREST_FRAMES` (default 600), `FOREST_MOTION` (1 = motion LOD, default 1),
//! `FOREST_SWAY` (1 = wake sway, default 1), `FOREST_PNG=<path>` (one frame from the middle of the flight).
//! Three frames are kept in flight; the statistics skip the first quarter (pipeline warm-up). Impostors
//! are off (they need the GPU impostor layer); the meshes shrink out over each class's fade band.

use std::sync::Arc;
use std::time::Instant;

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, FrameTarget, LabScene, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::scatter::forest::{
    CameraPath, CorridorSpec, ForestSpec, MotionLod, PathSampler, ScatterQuery, SwayField, SwaySettings,
    TrackWake, WakeModel, WakeSample, carve_corridor, synthetic_forest,
};
use regia_terrain::{CdlodConfig, LayerDesc, ScatterField, ScatterView, TerrainLayers, TerrainSharedDraw};

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    regia_core::brand::env_var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn layers() -> TerrainLayers {
    TerrainLayers {
        layers: vec![LayerDesc {
            name: "leaf litter".into(),
            albedo: [0.16, 0.11, 0.07],
            roughness: 0.9,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -1000.0,
            altitude_max: 5000.0,
            blend_sharpness: 0.15,
            triplanar: false,
            ..LayerDesc::ground()
        }],
        ..TerrainLayers::default()
    }
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("forest_gpu_cost: {msg}");
    std::process::exit(1);
}

fn main() {
    let size = regia_core::brand::env_var("FOREST_GPU_SIZE").unwrap_or_else(|_| "2560x1440".into());
    let (width, height): (u32, u32) = size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((2560, 1440));
    let preset = regia_core::brand::env_var("FOREST_GPU_PROFILE")
        .ok()
        .and_then(|name| RenderProfilePreset::from_name(&name))
        .unwrap_or(RenderProfilePreset::DesktopUltra);
    let speed: f32 = env_or("FOREST_SPEED", 100.0);
    let agl: f32 = env_or("FOREST_HEIGHT", 10.0);
    let frames: usize = env_or("FOREST_FRAMES", 600usize).max(8);
    let motion = env_or("FOREST_MOTION", 1u32) == 1;
    let sway_on = env_or("FOREST_SWAY", 1u32) == 1;
    let fps = 60.0f32;
    let png = regia_core::brand::env_var("FOREST_PNG").ok();

    let device = RenderDevice::headless().unwrap_or_else(|e| fail(format!("GPU device: {e}")));
    let forest = synthetic_forest(&ForestSpec::default()).unwrap_or_else(|e| fail(e));
    let map = Arc::clone(&forest.source.map);
    let length = speed * frames as f32 / fps + 100.0;
    let x0 = -length * 0.5;
    let n = (length / 10.0).ceil() as usize;
    let points: Vec<[f32; 3]> = (0..=n)
        .map(|i| {
            let x = x0 + length * i as f32 / n as f32;
            let z = 25.0 * (x * 0.01).sin();
            [x, map.sample_bilinear(x, z) + agl, z]
        })
        .collect();
    let path = CameraPath { points, times: None };
    let query =
        ScatterQuery::new(&forest.set, forest.source.clone(), &forest.library).unwrap_or_else(|e| fail(e));
    let carve = carve_corridor(
        &query,
        &path,
        &CorridorSpec {
            clearance_m: 2.5,
            ..CorridorSpec::default()
        },
    );
    let mut set = forest.set.clone();
    carve.apply(&mut set);
    let mut field =
        ScatterField::new(&set, forest.source.clone(), &forest.library, 0x1503).unwrap_or_else(|e| fail(e));
    if motion {
        field.set_motion_lod(Some(MotionLod::for_flight(speed, fps)));
    }
    let sampler = PathSampler::new(&path, speed);
    let start = sampler.point_at(0.0);
    field.settle_streaming(start);

    let draw = TerrainSharedDraw::new(&device, (*map).clone(), &layers(), CdlodConfig::default(), 5)
        .unwrap_or_else(|e| fail(format!("terrain: {e}")));
    let post = PostSettings {
        exposure_ev: 14.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width,
        height,
        eye: start,
        target: [start[0] + 1.0, start[1], start[2]],
        fov_y: 50f32.to_radians(),
        near: 0.1,
        far: 6000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 9_000.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.6, -0.36, -0.71], [1.0, 0.92, 0.82], 50_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
    quality.render_scale_permille = 1000;
    let mut renderer = FrameRenderer::new();
    renderer.set_gpu_timing(&device, true);
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    if let Err(e) = renderer.prewarm(&device, &field.prewarm_meshes(), 65_536) {
        fail(format!("prewarm: {e}"));
    }
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("forest cost target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let target = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let info = device.adapter.get_info();
    println!(
        "forest gpu cost: {} ({:?}), {width}x{height}, profile {preset:?}, {speed} m/s at {agl} m AGL, {frames} frames, motion LOD {motion}, sway {sway_on}, carve removed {} trees",
        info.name,
        info.backend,
        carve.removed()
    );
    let mut wake = TrackWake::new(WakeModel::default());
    let mut sway = SwayField::new(SwaySettings::default());
    let mut in_flight = std::collections::VecDeque::new();
    let mut rows: Vec<[f32; 7]> = Vec::new();
    let mut dir = [1.0f32, 0.0, 0.0];
    for k in 0..frames {
        let s = sampler.along_at(k as f32 / fps);
        let eye = sampler.point_at(s);
        let ahead = sampler.point_at(s + 20.0);
        let d = [ahead[0] - eye[0], ahead[1] - eye[1], ahead[2] - eye[2]];
        let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        if len > 1e-3 {
            dir = [d[0] / len, d[1] / len, d[2] / len];
        }
        let look = [eye[0] + dir[0], eye[1] + dir[1], eye[2] + dir[2]];
        let cpu = Instant::now();
        field.update(eye);
        let view = ScatterView::look_at(
            eye,
            look,
            scene.fov_y,
            width as f32 / height as f32,
            scene.near,
            scene.far,
        )
        .with_sun([-0.6, 0.36, 0.71]);
        let mut draws = Vec::new();
        let stats = field.collect(&view, &mut draws);
        if sway_on {
            wake.push(WakeSample {
                time_s: k as f64 / f64::from(fps),
                position: eye,
            });
            sway.step(&wake, &draws, 1.0 / fps);
            sway.apply_frame(&mut draws);
        }
        let cpu_ms = cpu.elapsed().as_secs_f32() * 1000.0;
        let mut frame = scene.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        frame.eye = eye;
        frame.target = look;
        let draw_count = draws.len();
        frame.meshes = draws;
        if let Err(e) =
            renderer.render_into(&device, &frame, FrameTarget::full(&target, format, width, height))
        {
            fail(format!("frame {k}: {e}"));
        }
        if let Some(submission) = renderer.take_submission() {
            in_flight.push_back(submission);
        }
        if in_flight.len() >= 3
            && let Some(oldest) = in_flight.pop_front()
            && let Err(e) = device.device.poll(wgpu::PollType::Wait {
                submission_index: Some(oldest),
                timeout: None,
            })
        {
            fail(format!("poll: {e}"));
        }
        if k == frames / 2
            && let Some(path) = &png
        {
            match renderer.render_offscreen(&device, &frame) {
                Ok(out) => write_png(path, out.width, out.height, &out.pixels),
                Err(err) => eprintln!("png frame failed: {err}"),
            }
        }
        if k >= frames / 4 {
            let t = renderer.gpu_timings();
            rows.push([
                t.frame_ms,
                t.forward_ms,
                t.shadows_ms,
                t.prepass_ms,
                cpu_ms,
                stats.emitted as f32,
                draw_count as f32,
            ]);
        }
    }
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    let pct = |i: usize, q: f32| {
        let mut v: Vec<f32> = rows.iter().map(|r| r[i]).collect();
        v.sort_by(f32::total_cmp);
        v.get(((v.len().max(1) - 1) as f32 * q).round() as usize)
            .copied()
            .unwrap_or(0.0)
    };
    println!("metric,p50,p99,max");
    for (i, name) in [
        "gpu_frame_ms",
        "gpu_forward_ms",
        "gpu_shadows_ms",
        "gpu_prepass_ms",
        "cpu_scatter_ms",
    ]
    .iter()
    .enumerate()
    {
        println!("{name},{:.3},{:.3},{:.3}", pct(i, 0.5), pct(i, 0.99), pct(i, 1.0));
    }
    println!(
        "emitted_instances,{:.0},{:.0},{:.0}",
        pct(5, 0.5),
        pct(5, 0.99),
        pct(5, 1.0)
    );
    println!("draws,{:.0},{:.0},{:.0}", pct(6, 0.5), pct(6, 0.99), pct(6, 1.0));
}

fn write_png(path: &str, width: u32, height: u32, pixels: &[u8]) {
    let Ok(file) = std::fs::File::create(path) else {
        eprintln!("cannot create {path}");
        return;
    };
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(pixels);
    }
}
