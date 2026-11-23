// SPDX-License-Identifier: MPL-2.0
//! What the grass field costs on the GPU (ADR-1400): a 4 km terrain tile (4097² samples,
//! eroded synthetic hills) with the dead-winter-grass set at its full density, rendered back to
//! back with grass off and on per tier, at a chosen output size and profile.
//!
//! ```text
//! cargo run --release -p regia_terrain --example grass_cost
//! GRASS_COST_SIZE=3840x2160 GRASS_COST_PROFILE=DesktopUltra cargo run --release -p regia_terrain --example grass_cost
//! ```
//!
//! Knobs (environment): `GRASS_COST_SIZE=2560x1440`, `GRASS_COST_PROFILE` (a
//! `RenderProfilePreset` name; default `DesktopUltra`), `GRASS_COST_TIERS=off,low,high,ultra`,
//! `GRASS_COST_FRAMES=240`, `GRASS_COST_EYE=1.6` (camera height over the ground, m),
//! `GRASS_COST_SAMPLES` (the heightfield side, default 4097). Frames render with three in
//! flight and no readback; the medians cover the last three quarters of each burst. The eye
//! creeps forward so no held-view reuse applies (the grass regenerates every frame).
//! `GRASS_COST_PNG=<dir/stem>` also writes one frame per tier, `<dir/stem>_<tier>.png` (read back
//! after each burst).

use std::time::Instant;

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, FrameTarget, LabScene, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, GrassLayer, GrassMask, GrassMaskInputs, GrassQuality, GrassRequest, GrassSet, LayerDesc,
    TerrainLayers, TerrainMap, TerrainSharedDraw,
};

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    regia_core::brand::env_var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn layers() -> TerrainLayers {
    TerrainLayers {
        layers: vec![LayerDesc {
            name: "dead grass".into(),
            albedo: [0.24, 0.19, 0.12],
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

/// A 4 km tile of rolling hills (valleys and ridges, no flat plain).
fn map(samples: u32) -> TerrainMap {
    let world = 4096.0f32;
    let mut heights = Vec::with_capacity((samples * samples) as usize);
    for row in 0..samples {
        for col in 0..samples {
            let x = col as f32 / (samples - 1) as f32 * world - world * 0.5;
            let z = row as f32 / (samples - 1) as f32 * world - world * 0.5;
            let h = 18.0 * (x * 0.0021).sin() * (z * 0.0017).cos()
                + 6.0 * (x * 0.0071 + z * 0.0043).sin()
                + 1.2 * (x * 0.031).sin() * (z * 0.027).sin();
            heights.push(h);
        }
    }
    TerrainMap::from_heights(samples, samples, heights, world, 1.0, 0.0).expect("map")
}

struct Sample {
    shared_ms: f32,
    shadows_ms: f32,
    prepass_ms: f32,
    frame_ms: f32,
    wall_ms: f32,
    candidates: u64,
    patches: u32,
}

/// The bench's knobs (environment, see the module docs).
#[derive(Clone, Copy)]
struct Knobs {
    width: u32,
    height: u32,
    preset: RenderProfilePreset,
    frames: usize,
    eye_height: f32,
}

fn burst(
    device: &RenderDevice,
    map: &TerrainMap,
    mask: &GrassMask,
    tier: GrassQuality,
    knobs: &Knobs,
    png: Option<&str>,
) -> Sample {
    let Knobs {
        width,
        height,
        preset,
        frames,
        eye_height,
    } = *knobs;
    let layers = layers();
    let mut draw =
        TerrainSharedDraw::new(device, map.clone(), &layers, CdlodConfig::default(), 5).expect("terrain");
    let set = GrassSet::winter();
    let layer = GrassLayer::new(&device.device, &device.queue, set, mask.clone()).expect("grass");
    let handle = layer.handle();
    handle.set_request(GrassRequest {
        enabled: tier != GrassQuality::Off,
        quality: Some(tier),
        ..GrassRequest::default()
    });
    draw.set_grass(Some(layer));
    let post = PostSettings {
        exposure_ev: 14.0,
        ..PostSettings::golden_deterministic()
    };
    let ground = map.sample_bilinear(0.0, 300.0);
    let scene = LabScene {
        width,
        height,
        eye: [0.0, ground + eye_height, 300.0],
        target: [0.0, map.sample_bilinear(0.0, 260.0) + eye_height * 0.6, 260.0],
        fov_y: 0.9,
        near: 0.05,
        far: 6000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 9_000.0,
        double_sided: false,
        post: Some(post.clone()),
        // About 11° of sun elevation, the showcase morning.
        directional: DirectionalLightUniform::new([0.6, -0.19, -0.78], [1.0, 0.92, 0.82], 50_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
    quality.taau = false;
    quality.render_scale_permille = 1000;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_gpu_timing(device, true);
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("grass cost target"),
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
    let eye = frame.eye;
    let look = frame.target;
    let mut in_flight = std::collections::VecDeque::new();
    let mut samples = Vec::new();
    let mut last = Instant::now();
    for k in 0..frames {
        // The eye creeps 2 cm a frame toward −Z.
        let dz = -0.02 * k as f32;
        frame.eye = [eye[0], eye[1], eye[2] + dz];
        frame.target = [look[0], look[1], look[2] + dz];
        renderer
            .render_into(device, &frame, FrameTarget::full(&target, format, width, height))
            .expect("frame");
        if let Some(submission) = renderer.take_submission() {
            in_flight.push_back(submission);
        }
        if in_flight.len() >= 3
            && let Some(oldest) = in_flight.pop_front()
        {
            device
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(oldest),
                    timeout: None,
                })
                .expect("poll");
        }
        let now = Instant::now();
        if k >= frames / 4 {
            let t = renderer.gpu_timings();
            samples.push([
                t.shared_forward_ms,
                t.shadows_ms,
                t.prepass_ms,
                t.frame_ms,
                (now - last).as_secs_f32() * 1000.0,
            ]);
        }
        last = now;
    }
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    if let Some(path) = png {
        match renderer.render_offscreen(device, &frame) {
            Ok(out) => {
                if let Ok(file) = std::fs::File::create(path) {
                    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), out.width, out.height);
                    encoder.set_color(png::ColorType::Rgba);
                    encoder.set_depth(png::BitDepth::Eight);
                    if let Ok(mut writer) = encoder.write_header() {
                        let _ = writer.write_image_data(&out.pixels);
                    }
                }
            }
            Err(err) => eprintln!("png frame failed: {err}"),
        }
    }
    let median = |i: usize| {
        let mut v: Vec<f32> = samples.iter().map(|s| s[i]).collect();
        v.sort_by(f32::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    let report = handle.report();
    Sample {
        shared_ms: median(0),
        shadows_ms: median(1),
        prepass_ms: median(2),
        frame_ms: median(3),
        wall_ms: median(4),
        candidates: report.candidates,
        patches: report.patches,
    }
}

fn main() {
    let device = RenderDevice::headless().expect("GPU device");
    let size = regia_core::brand::env_var("GRASS_COST_SIZE").unwrap_or_else(|_| "2560x1440".into());
    let (width, height) = size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((2560, 1440));
    let preset = regia_core::brand::env_var("GRASS_COST_PROFILE")
        .ok()
        .and_then(|name| RenderProfilePreset::from_name(&name))
        .unwrap_or(RenderProfilePreset::DesktopUltra);
    let tiers: Vec<GrassQuality> = regia_core::brand::env_var("GRASS_COST_TIERS")
        .unwrap_or_else(|_| "off,low,high,ultra".into())
        .split(',')
        .filter_map(|t| GrassQuality::from_name(t.trim()))
        .collect();
    let frames = env_or("GRASS_COST_FRAMES", 240usize).max(8);
    let eye_height = env_or("GRASS_COST_EYE", 1.6f32);
    let samples = env_or("GRASS_COST_SAMPLES", 4097u32).clamp(65, 4097);
    let png = regia_core::brand::env_var("GRASS_COST_PNG").ok();
    let knobs = Knobs {
        width,
        height,
        preset,
        frames,
        eye_height,
    };
    let info = device.adapter.get_info();
    println!(
        "grass cost: {} ({:?}), {width}x{height}, profile {}, {frames} frames, eye {eye_height} m, map {samples}² over 4096 m",
        info.name,
        info.backend,
        preset.name()
    );
    let started = Instant::now();
    let map = map(samples);
    let set = GrassSet::winter();
    let mask = GrassMask::build(
        &set,
        &GrassMaskInputs {
            layers: Some(&layers()),
            texel_m: 2.0,
            ..GrassMaskInputs::new(&map)
        },
    )
    .expect("mask");
    println!(
        "  map + mask ({}² texels) built in {:.2} s",
        mask.side(),
        started.elapsed().as_secs_f32()
    );
    for &tier in &tiers {
        let path = png.as_ref().map(|stem| format!("{stem}_{}.png", tier.name()));
        let s = burst(&device, &map, &mask, tier, &knobs, path.as_deref());
        println!(
            "  grass {:<8} shared-forward {:>7.3} ms  prepass {:>7.3} ms  shadows {:>7.3} ms  frame {:>7.3} ms  wall {:>7.3} ms/frame  patches {:>5}  candidates {:>9}",
            tier.name(),
            s.shared_ms,
            s.prepass_ms,
            s.shadows_ms,
            s.frame_ms,
            s.wall_ms,
            s.patches,
            s.candidates
        );
    }
}
