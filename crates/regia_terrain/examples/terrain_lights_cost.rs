// SPDX-License-Identifier: MPL-2.0
//! What the terrain's point and spot lights cost on the GPU (lane v020-terrain-local-lights,
//! review finding 2): the shared draw's forward time (`GpuTimings::shared_forward_ms`) with the
//! lights on and lamps over the ground, on with one lamp behind the camera, on with no lamp in
//! the frame, and off (the colour pass built without the light loop,
//! `TerrainSharedDraw::set_local_lights_enabled(false)`).
//!
//! A plain executable, so it runs on a desktop, a Steam Deck and from `adb shell` (no APK):
//!
//! ```text
//! cargo run --release -p regia_terrain --example terrain_lights_cost
//! cargo ndk -t arm64-v8a build --release -p regia_terrain --example terrain_lights_cost \
//!   && adb push target/aarch64-linux-android/release/examples/terrain_lights_cost /data/local/tmp/ \
//!   && adb shell /data/local/tmp/terrain_lights_cost
//! ```
//!
//! Knobs (environment): `TERRAIN_LIGHTS_COST_SIZE=1280x800`, `TERRAIN_LIGHTS_COST_PROFILE`
//! (a `RenderProfilePreset` name; default the platform's), `TERRAIN_LIGHTS_COST_FRAMES=480`,
//! `TERRAIN_LIGHTS_COST_ROUNDS=2`. Frames render back to back with three in flight and no
//! readback, so the GPU stays at its working clocks; the medians cover the last three quarters
//! of each burst. Without in-pass timestamps (`TIMESTAMP_QUERY_INSIDE_PASSES`, most mobile
//! GPUs) the terrain column reads 0 and the wall time per frame is the measure.

use std::time::Instant;

use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer,
    FrameTarget, LabLocalLight, LabPointLight, LabScene, OutdoorParams, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, MaterialParams, Rgba8Image, TerrainLayers, TerrainMap,
    TerrainMaterial, TerrainSharedDraw,
};

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    regia_core::brand::env_var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Rolling 0.18-grey ground, 256 m across.
fn ground(device: &RenderDevice) -> TerrainSharedDraw {
    let n = 129usize;
    let world = 256.0;
    let heights: Vec<f32> = (0..n * n)
        .map(|i| {
            let (x, z) = ((i % n) as f32, (i / n) as f32);
            0.5 + 0.25 * ((x * 0.21).sin() * (z * 0.17).cos())
        })
        .collect();
    let map = TerrainMap::from_heights(n as u32, n as u32, heights, world, 8.0, -4.0).expect("map");
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [1.0, 1.0, 1.0],
            roughness: 0.8,
            tiling: 2.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            blend_sharpness: 0.15,
            triplanar: false,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let v = regia_terrain::linear_to_srgb8(0.18);
    let mut draw = TerrainSharedDraw::new(device, map, &stack, CdlodConfig::default(), 3).expect("terrain");
    draw.set_material(
        &TerrainMaterial {
            layers: vec![Some(LayerTextures {
                albedo: Rgba8Image::solid(256, 256, [v, v, v, 255]),
                normal: Rgba8Image::neutral_normal(256),
                height: Rgba8Image::neutral_height(256),
                orm: Rgba8Image::solid(256, 256, [255, 200, 0, 255]),
                use_orm: false,
            })],
            macro_set: None,
            params: MaterialParams::default(),
        },
        &[],
    )
    .expect("material");
    draw
}

/// Sixteen lamps 2–3 m over the ground in front of the camera, every fourth one casting
/// shadows (muzzle flashes, base lights, a hull's lamps).
fn lamps() -> Vec<LabLocalLight> {
    (0..16)
        .map(|i| {
            let (col, row) = ((i % 4) as f32, (i / 4) as f32);
            LabLocalLight::Point(LabPointLight {
                position: [-12.0 + col * 8.0, 2.5 + 0.5 * (i % 2) as f32, -4.0 + row * 8.0],
                color: [1.0, 0.85, 0.7],
                intensity: 60_000.0,
                radius: 12.0,
                cast_shadows: i % 4 == 0,
                source_radius: None,
            })
        })
        .collect()
}

/// One lamp behind the camera: the frame has a light, no visible cluster holds it.
fn lamp_behind() -> Vec<LabLocalLight> {
    vec![LabLocalLight::Point(LabPointLight {
        position: [0.0, 5.0, -80.0],
        color: [1.0, 1.0, 1.0],
        intensity: 60_000.0,
        radius: 12.0,
        cast_shadows: false,
        source_radius: None,
    })]
}

/// A 1 m crate near each lamp, so the shadow-casting ones fill their atlas tiles.
fn crates() -> Vec<regia_render::LabMesh> {
    (0..4)
        .map(|i| {
            let (verts, indices) = regia_render::box_mesh([0.5, 0.5, 0.5]);
            regia_render::LabMesh {
                vertices: verts,
                indices,
                material: 0,
                model: regia_core::glam::Mat4::from_translation([-10.0 + i as f32 * 8.0, 1.2, -2.0].into())
                    .to_cols_array_2d(),
                ..regia_render::LabMesh::unskinned()
            }
        })
        .collect()
}

struct Sample {
    terrain_ms: f32,
    frame_ms: f32,
    wall_ms: f32,
}

/// Renders `frames` back to back and returns the medians of the last three quarters.
fn burst(
    device: &RenderDevice,
    draw: TerrainSharedDraw,
    lights: Vec<LabLocalLight>,
    (width, height): (u32, u32),
    preset: RenderProfilePreset,
    frames: usize,
) -> Sample {
    let post = PostSettings {
        exposure_ev: 6.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width,
        height,
        eye: [0.0, 9.0, -22.0],
        target: [0.0, 0.0, 6.0],
        fov_y: 0.9,
        near: 0.1,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 6.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([-0.5, -0.6, 0.3], [1.0, 1.0, 1.0], 2000.0),
        local_lights: lights,
        materials: vec![regia_render::LabMaterial {
            base_color: [0.18, 0.18, 0.18, 1.0],
            roughness: 0.8,
            ..regia_render::LabMaterial::default()
        }],
        meshes: crates(),
    };
    let mut quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
    quality.taau = false;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_gpu_timing(device, true);
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("local lights cost target"),
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
    let mut in_flight = std::collections::VecDeque::new();
    let mut samples = Vec::new();
    let mut last = Instant::now();
    for k in 0..frames {
        // The eye creeps so no held-view reuse applies.
        frame.eye = [eye[0] + k as f32 * 0.002, eye[1], eye[2] + k as f32 * 0.001];
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
                t.frame_ms,
                (now - last).as_secs_f32() * 1000.0,
            ]);
        }
        last = now;
    }
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    let median = |i: usize| {
        let mut v: Vec<f32> = samples.iter().map(|s| s[i]).collect();
        v.sort_by(f32::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    Sample {
        terrain_ms: median(0),
        frame_ms: median(1),
        wall_ms: median(2),
    }
}

fn main() {
    let device = RenderDevice::headless().expect("GPU device");
    let size = regia_core::brand::env_var("TERRAIN_LIGHTS_COST_SIZE").unwrap_or_else(|_| "1280x800".into());
    let (width, height) = size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((1280, 800));
    let preset = regia_core::brand::env_var("TERRAIN_LIGHTS_COST_PROFILE")
        .ok()
        .and_then(|name| RenderProfilePreset::from_name(&name))
        .unwrap_or_else(RenderProfilePreset::platform_default);
    let frames = env_or("TERRAIN_LIGHTS_COST_FRAMES", 480usize).max(8);
    let rounds = env_or("TERRAIN_LIGHTS_COST_ROUNDS", 2usize).max(1);
    let info = device.adapter.get_info();
    println!(
        "terrain local lights cost: {} ({:?}), {width}x{height}, profile {}, {frames} frames x {rounds} rounds",
        info.name,
        info.backend,
        preset.name()
    );
    {
        let probe = ground(&device);
        println!(
            "  local lights supported on this device: {}",
            probe.gpu().local_lights_supported()
        );
    }
    for round in 1..=rounds {
        // "on, no lamp" is also what the batch v020 lane's first off switch cost: the loop compiled in,
        // skipped on the frame uniform. "on, 1 lamp behind" pays the froxel lookup on every
        // terrain pixel with no light in any visible cluster.
        for (name, enabled, lights) in [
            ("on, 16 lamps", true, lamps()),
            ("on, 1 lamp behind", true, lamp_behind()),
            ("on, no lamp", true, Vec::new()),
            ("off (no loop), 16 lamps", false, lamps()),
        ] {
            let mut draw = ground(&device);
            draw.set_local_lights_enabled(enabled);
            let s = burst(&device, draw, lights, (width, height), preset, frames);
            println!(
                "  round {round} {name:<24} terrain-forward {:.3} ms  frame {:.3} ms  wall {:.3} ms/frame",
                s.terrain_ms, s.frame_ms, s.wall_ms
            );
        }
    }
}
