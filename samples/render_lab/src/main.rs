// SPDX-License-Identifier: MIT-0
//! CLI for renderer lab scenes (`specs/13` § 13).
//!
//! Each scene renders headless and writes a PNG under `target/render-lab/` so
//! the images can be inspected alongside the golden tests. `sponza_1080p_fps`
//! measures the sustained frame rate of the Sponza view at 1920×1080.

mod planets;
mod post_volumes;
mod pt;
mod window;

use glam::{Mat4, Vec3};
use regia::prelude::*;
use regia_render::{
    BatchKey, DirectionalLightUniform, DrawItem, FrameQuality, FrameRenderer, GiMode, LabLocalLight,
    LabMaterial, LabMesh, LabScene, LabVertex, RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode,
    batch_instanced, corridor_point_lights, draw_call_count, render_lab_scene, sort_opaque,
};
use regia_test_utils::golden::{GoldenGuard, guard_golden, measure_golden};
use regia_test_utils::scene_loader::{LabView, ensure_model, load_gltf_model_with_view};
use regia_test_utils::scenes::{
    render_alpha_modes, render_clustered_lights, render_ibl_spheres, render_light_shadows,
    render_material_sphere_grid, render_post_stack, render_surface_maps,
};
use std::env;
use std::time::Instant;

fn out_dir() -> std::path::PathBuf {
    let dir = std::path::Path::new("target/render-lab");
    std::fs::create_dir_all(dir).expect("create target/render-lab");
    dir.to_path_buf()
}

fn save_png(name: &str, width: u32, height: u32, pixels: &[u8], guard: GoldenGuard) {
    let stats = measure_golden(pixels, width, height);
    println!(
        "{name}: mean lum {:+.1}, near-white {:.2}%, near-black {:.2}%, object {:.2}%",
        stats.mean_lum, stats.near_white_pct, stats.near_black_pct, stats.object_pct
    );
    let path = out_dir().join(format!("{name}.png"));
    image::save_buffer(&path, pixels, width, height, image::ColorType::Rgba8).expect("save png");
    println!("{name}: wrote {}", path.display());
    guard_golden(pixels, width, height, name, guard);
}

fn render_many_lights(
    device: &RenderDevice,
) -> Result<(Vec<u8>, regia_render::LightUploadReport), regia_render::RenderError> {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.local_shadow_atlas_size = 0;
    quality.local_shadow_max_lights = 0;
    quality.csm_cascade_count = 1;
    quality.csm_resolution = 64;
    quality.gpu_driven = false;
    quality.ssao = SsaoMode::Off;
    quality.ssr = false;
    quality.screen_space_gi = false;
    quality.gi = GiMode::Ambient;
    quality.reflection_probes = false;
    quality.paged_sun_shadows = false;
    quality.taau = false;
    quality.horizontal_plus = false;
    // The helper's 600 lm / radius-2 disks sit on the AgX toe in this view.
    // Raise them here only; the helper stays dim so the CPU cluster test
    // still spreads 200 lights without filling a froxel.
    let mut local_lights = corridor_point_lights(200, 6.0);
    for light in &mut local_lights {
        if let LabLocalLight::Point(point) = light {
            point.intensity = 12_000.0;
            point.radius = 10.0;
            point.position[1] = 2.4;
        }
    }
    let scene = LabScene {
        width: 160,
        height: 90,
        // Above light 20 (x = 120). A side view of the dim row stayed black.
        eye: [120.0, 4.2, 7.0],
        target: [120.0, 0.2, 0.0],
        fov_y: 55.0_f32.to_radians(),
        near: 0.15,
        far: 40.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 4.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -1.0, 0.1], [1.0, 1.0, 1.0], 0.0),
        local_lights,
        materials: vec![LabMaterial {
            base_color: [0.7, 0.7, 0.68, 1.0],
            roughness: 0.7,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: [
                [0.0, 0.0, -6.0],
                [400.0, 0.0, -6.0],
                [400.0, 0.0, 6.0],
                [0.0, 0.0, 6.0],
            ]
            .into_iter()
            .map(|position| LabVertex::from_pos_normal_uv(position, [0.0, 1.0, 0.0], [0.0, 0.0]))
            .collect(),
            indices: vec![0, 2, 1, 0, 3, 2],
            material: 0,
            ..LabMesh::unskinned()
        }],
    };
    let mut renderer = FrameRenderer::new();
    let pixels = renderer
        .render_offscreen(device, &scene.as_frame_scene_with_quality(quality))?
        .pixels;
    Ok((pixels, renderer.light_upload()))
}

fn lit_guard(min_object: f64) -> GoldenGuard {
    GoldenGuard {
        min_lum: 30.0,
        max_lum: 200.0,
        max_white_pct: 3.0,
        max_black_pct: 60.0,
        min_object_pct: min_object,
    }
}

/// Prints a luminance map so the structure of a render is visible without
/// opening the image (the worker rules require measuring what we bless).
fn ascii_map(pixels: &[u8], width: u32, height: u32, cols: usize, rows: usize) {
    let stats = measure_golden(pixels, width, height);
    println!(
        "stats: mean lum {:+.1}, near-white {:.2}%, near-black {:.2}%, object {:.2}%",
        stats.mean_lum, stats.near_white_pct, stats.near_black_pct, stats.object_pct
    );
    let chars: [char; 10] = [' ', '.', ':', '-', '=', '+', '*', '#', '%', '@'];
    // Cell luminance grid.
    let mut cells: Vec<Vec<f64>> = Vec::with_capacity(rows);
    for iy in 0..rows {
        let mut row = Vec::with_capacity(cols);
        for ix in 0..cols {
            let mut acc = 0.0f64;
            let mut n = 0u32;
            let x0 = ix * width as usize / cols;
            let x1 = (ix + 1) * width as usize / cols;
            let y0 = iy * height as usize / rows;
            let y1 = (iy + 1) * height as usize / rows;
            for y in y0..y1.max(y0 + 1) {
                for x in x0..x1.max(x0 + 1) {
                    let i = (y * width as usize + x) * 4;
                    if i + 2 < pixels.len() {
                        acc += 0.2126 * f64::from(pixels[i])
                            + 0.7152 * f64::from(pixels[i + 1])
                            + 0.0722 * f64::from(pixels[i + 2]);
                        n += 1;
                    }
                }
            }
            row.push(acc / f64::from(n.max(1)));
        }
        cells.push(row);
    }
    println!("{cols}x{rows} luminance map (dark → bright):");
    for row in &cells {
        let line: String = row
            .iter()
            .map(|v| chars[(((v / 255.0) * 10.0) as usize).min(9)])
            .collect();
        println!("{line}");
    }
    // Contrast-stretched (structure view): brightness relative to the frame's
    // maximum, so shapes are visible even in a dark scene.
    let max = cells.iter().flatten().copied().fold(0.0_f64, f64::max).max(1.0);
    println!("contrast-stretched map:");
    for row in &cells {
        let line: String = row
            .iter()
            .map(|v| chars[(((v / max) * 10.0) as usize).min(9)])
            .collect();
        println!("{line}");
    }
}

/// Renders `lab` repeatedly and reports the harness's sustained frame rate.
///
/// NOTE: `render_lab_scene` re-uploads every mesh/texture buffer and recompiles
/// every shader module and pipeline on each call — this measures the golden
/// harness's per-call throughput, not the engine's persistent frame loop.
fn fps_loop(label: &str, lab: &regia_render::LabScene) {
    let device = RenderDevice::headless().expect("GPU");
    let warm = render_lab_scene(&device, lab).expect("warm-up");
    println!("warm-up frame: {}x{}", warm.width, warm.height);
    let frames = 60u32;
    let start = Instant::now();
    for _ in 0..frames {
        render_lab_scene(&device, lab).expect("render");
    }
    let elapsed = start.elapsed();
    println!(
        "{label}: {frames} frames in {:.3} s → {:.1} fps ({:.2} ms/frame) [harness throughput, per-call uploads+compiles included]",
        elapsed.as_secs_f64(),
        frames as f64 / elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1000.0 / f64::from(frames),
    );
}

/// Möller–Trumbore ray–triangle intersection (public-domain algorithm; see
/// `docs/provenance.md`); returns the ray parameter `t` of the first hit.
fn ray_triangle(orig: Vec3, dir: Vec3, a: Vec3, b: Vec3, c: Vec3) -> Option<f32> {
    let e1 = b - a;
    let e2 = c - a;
    let pvec = dir.cross(e2);
    let det = e1.dot(pvec);
    if det.abs() < 1.0e-8 {
        return None;
    }
    let inv = 1.0 / det;
    let tvec = orig - a;
    let u = tvec.dot(pvec) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let qvec = tvec.cross(e1);
    let v = dir.dot(qvec) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = e2.dot(qvec) * inv;
    (t > 0.01).then_some(t)
}

/// One cube, 500 materials, 100,000 instances, desktop-high path.
///
/// Not part of the default test suite: it is a floor-tier stress measurement
/// (`specs/26` § 6). Colour, shadows and the depth prepass are instanced on the
/// GPU-driven path.
fn stress_100k() {
    use regia_render::{
        DirectionalLightUniform, FrameMesh, FrameRenderer, FrameScene, FrameTarget, LabMaterial,
        RenderProfilePreset, RenderSettings,
    };
    const INSTANCES: usize = 100_000;
    const MATERIALS: usize = 500;
    let settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    let post = settings.post.clone();
    let device = RenderDevice::headless().expect("GPU");
    let (width, height) = (1920u32, 1080u32);
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let target = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("stress target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let (vertices, indices) = unit_cube();
    let materials: Vec<LabMaterial> = (0..MATERIALS)
        .map(|i| {
            let t = i as f32 / MATERIALS as f32;
            LabMaterial {
                base_color: [0.2 + 0.8 * t, 0.35, 1.0 - 0.7 * t, 1.0],
                roughness: 0.35,
                ..LabMaterial::default()
            }
        })
        .collect();
    let models: Vec<[[f32; 4]; 4]> = (0..INSTANCES)
        .map(|i| {
            let x = (i % 50) as f32;
            let y = ((i / 50) % 40) as f32;
            let z = (i / 2_000) as f32;
            Mat4::from_translation(Vec3::new(x * 0.45, y * 0.45, z * 0.45)).to_cols_array_2d()
        })
        .collect();
    let meshes: Vec<FrameMesh<'_>> = (0..INSTANCES)
        .map(|i| FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: (i % MATERIALS) as u64 + 1,
            object_key: i as u64 + 1,
            vertices: &vertices,
            indices: &indices,
            material: &materials[i % MATERIALS],
            model: models[i],
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        })
        .collect();
    let scene = FrameScene {
        width,
        height,
        eye: [10.0, 28.0, 36.0],
        target: [11.0, 8.0, 11.0],
        fov_y: 1.0,
        near: 0.1,
        far: 80.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 15.0,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: Some(&post),
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new([-0.4, -0.8, -0.3], [1.0, 0.98, 0.95], 100_000.0),
        local_lights: &[],
        quality: settings.quality(),
        meshes,
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_gpu_timing(&device, true);
    let frame_target = FrameTarget::full(&view, format, width, height);
    for _ in 0..3 {
        renderer
            .render_into(&device, &scene, frame_target)
            .expect("warmup");
        device
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("warmup poll");
    }
    let frames = 20u32;
    let mut times = Vec::with_capacity(frames as usize);
    for _ in 0..frames {
        let start = Instant::now();
        renderer
            .render_into(&device, &scene, frame_target)
            .expect("render");
        device
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let p = |q: f64| times[((q * frames as f64) as usize).min(times.len() - 1)];
    println!(
        "stress 100k instances / {MATERIALS} materials, 1920x1080 desktop-high: {frames} frames, p50 {:.2} ms ({:.0} fps), p95 {:.2} ms, mean {:.2} ms",
        p(0.50),
        1000.0 / p(0.50),
        p(0.95),
        times.iter().sum::<f64>() / f64::from(frames),
    );
    println!("GPU: {}", device.capabilities.adapter_name);
    let gpu = renderer.gpu_timings();
    println!(
        "gpu passes (last frame): {}, gi cpu {:.2} ms",
        gpu.report(),
        renderer.gi_cpu_ms()
    );
    let (mesh_n, mat_n, obj_n) = renderer.cache_sizes();
    println!("gpu cache: {mesh_n} meshes, {mat_n} materials, {obj_n} objects");
}

/// Deck preset at 1280×800, `render_scale` 0.7, TAAU on.
///
/// This machine is not a Steam Deck. The number is a desktop-GPU frame time for
/// that preset, not a Deck measurement.
fn deck_800p() {
    use regia::prelude::*;
    use regia_render::{
        ExtractedFrame, FrameRenderer, FrameTarget, RenderProfilePreset, RenderSettings, spawn_lab_scene,
    };
    let path = ensure_model("Sponza").expect("Sponza in cache");
    let lab = load_gltf_model_with_view(&path, LabView::SponzaAtrium, None).expect("load");
    let (width, height) = (
        RenderProfilePreset::DECK_OUTPUT_WIDTH,
        RenderProfilePreset::DECK_OUTPUT_HEIGHT,
    );
    let device = RenderDevice::headless().expect("GPU");
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let target = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("deck target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.add_systems(Render, regia_render::extract_frame);
    spawn_lab_scene(app.world_mut(), &lab);
    // spawn_lab_scene pins the golden baseline. Put the Deck preset back.
    app.world_mut()
        .insert_resource(RenderSettings::from_profile(RenderProfilePreset::Deck));
    let mut renderer = FrameRenderer::new();
    renderer.set_gpu_timing(&device, true);
    let render = |renderer: &mut FrameRenderer, app: &mut App| {
        let frame = app
            .world()
            .resource::<ExtractedFrame>()
            .frame_scene(0, width, height);
        renderer.render_into(&device, &frame, FrameTarget::full(&view, format, width, height))
    };
    for _ in 0..20 {
        app.update();
        render(&mut renderer, &mut app).expect("warm-up render");
        device
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("warm-up poll");
    }
    let frames = 120u32;
    let mut times = Vec::with_capacity(frames as usize);
    for _ in 0..frames {
        let start = Instant::now();
        app.update();
        render(&mut renderer, &mut app).expect("render");
        device
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).expect("finite times"));
    let p = |q: f64| times[((q * frames as f64) as usize).min(times.len() - 1)];
    println!("not a Steam Deck — desktop GPU, Deck preset only");
    println!(
        "deck preset 1280x800 render_scale 0.7 TAAU: {} frames, p50 {:.2} ms ({:.0} fps), p95 {:.2} ms, mean {:.2} ms",
        frames,
        p(0.50),
        1000.0 / p(0.50),
        p(0.95),
        times.iter().sum::<f64>() / f64::from(frames),
    );
    println!("GPU: {}", device.capabilities.adapter_name);
    let gpu = renderer.gpu_timings();
    println!(
        "gpu passes (last frame): {}, gi cpu {:.2} ms",
        gpu.report(),
        renderer.gi_cpu_ms()
    );
}

/// M15c planet presets on a heightfield (`planets.rs`). `--out DIR` saves there instead of
/// `target/render-lab`.
fn planet_frames(args: &[String]) {
    let device = RenderDevice::headless().expect("headless device");
    let dir = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1))
        .map_or_else(out_dir, std::path::PathBuf::from);
    std::fs::create_dir_all(&dir).expect("create output dir");
    let (width, height) = (960, 540);
    for (name, pixels, report) in planets::render_shots(&device, width, height) {
        let path = dir.join(format!("{name}.png"));
        if pixels.len() != (width * height * 4) as usize {
            eprintln!("{name}: no image");
            continue;
        }
        image::save_buffer(&path, &pixels, width, height, image::ColorType::Rgba8).expect("save png");
        println!("{name}: wrote {} — gpu {report}", path.display());
    }
}

fn outdoor_frame() {
    use regia_render::{
        CloudSettings, FogVolume, FrameQuality, FrameRenderer, LabScene, OutdoorParams, PostSettings,
        RenderProfile, RenderProfilePreset, SsaoMode, TimeOfDay, VolumetricFogSettings,
    };
    let device = RenderDevice::headless().expect("headless device");
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.gpu_driven = false;
    quality.ssao = SsaoMode::Off;
    let scene = LabScene {
        width: 320,
        height: 180,
        eye: [0.0, 2.2, 14.0],
        target: [1.5, 1.5, 2.0],
        fov_y: 50.0_f32.to_radians(),
        near: 0.3,
        far: 80.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 16.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            exposure_ev: 16.0,
            fxaa: false,
            taa: false,
            film_grain: 0.0,
            vignette: 0.0,
            ..PostSettings::default()
        }),
        directional: regia_render::DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 100_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_gpu_timing(&device, true);
    renderer.set_outdoor(OutdoorParams {
        time: Some(TimeOfDay::noon()),
        fog: VolumetricFogSettings {
            albedo: [0.9, 0.92, 0.95],
            anisotropy: 0.4,
            ..VolumetricFogSettings::default()
        },
        volumes: vec![FogVolume::Box {
            center: [1.2, 2.0, 6.0],
            extents: [8.0, 3.0, 10.0],
            density: 0.15,
        }],
        lights: Vec::new(),
        clouds: CloudSettings::coverage_layer(0.45),
        ..OutdoorParams::default()
    });
    let frame = scene.as_frame_scene_with_quality(quality);
    // Several frames: the first one pays for pipeline creation, and timestamps land two
    // frames late (ADR-027).
    let mut image = renderer.render_offscreen(&device, &frame).expect("outdoor frame");
    for _ in 0..4 {
        image = renderer.render_offscreen(&device, &frame).expect("outdoor frame");
    }
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let gpu = renderer.gpu_timings();
    println!(
        "outdoor desktop-high {}x{}: {}, gi cpu {:.2} ms",
        image.width,
        image.height,
        gpu.report(),
        renderer.gi_cpu_ms()
    );
    save_png(
        "outdoor",
        image.width,
        image.height,
        &image.pixels,
        GoldenGuard {
            min_lum: 5.0,
            max_lum: 220.0,
            max_white_pct: 40.0,
            max_black_pct: 80.0,
            min_object_pct: 0.0,
        },
    );
}

fn unit_cube() -> (Vec<regia_render::LabVertex>, Vec<u32>) {
    use regia_render::LabVertex;
    let face = |normal: [f32; 3], verts: [[f32; 3]; 4]| -> Vec<LabVertex> {
        verts
            .into_iter()
            .map(|position| LabVertex {
                position,
                normal,
                uv: [0.0, 0.0],
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            })
            .collect()
    };
    let s = 0.2;
    let mut vertices = Vec::new();
    vertices.extend(face(
        [0.0, 0.0, 1.0],
        [[-s, -s, s], [s, -s, s], [s, s, s], [-s, s, s]],
    ));
    vertices.extend(face(
        [0.0, 0.0, -1.0],
        [[s, -s, -s], [-s, -s, -s], [-s, s, -s], [s, s, -s]],
    ));
    vertices.extend(face(
        [1.0, 0.0, 0.0],
        [[s, -s, s], [s, -s, -s], [s, s, -s], [s, s, s]],
    ));
    vertices.extend(face(
        [-1.0, 0.0, 0.0],
        [[-s, -s, -s], [-s, -s, s], [-s, s, s], [-s, s, -s]],
    ));
    vertices.extend(face(
        [0.0, 1.0, 0.0],
        [[-s, s, s], [s, s, s], [s, s, -s], [-s, s, -s]],
    ));
    vertices.extend(face(
        [0.0, -1.0, 0.0],
        [[-s, -s, -s], [s, -s, -s], [s, -s, s], [-s, -s, s]],
    ));
    let mut indices = Vec::with_capacity(36);
    for face_i in 0..6u32 {
        let b = face_i * 4;
        indices.extend([b, b + 1, b + 2, b, b + 2, b + 3]);
    }
    (vertices, indices)
}

/// Project directory of this sample, resolved at runtime (ADR-10370).
///
/// `cargo run` exports `CARGO_MANIFEST_DIR`. A binary under `target/<profile>`
/// walks up to `samples/render_lab`. A packaged game finds neither and returns
/// `None`. The compile-time `env!("CARGO_MANIFEST_DIR")` is not remapped by rustc.
fn sample_project_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("CARGO_MANIFEST_DIR") {
        return Some(dir.into());
    }
    let root = std::env::current_exe()
        .ok()?
        .parent()?
        .parent()?
        .parent()?
        .to_path_buf();
    let project = root.join("samples").join("render_lab");
    project.is_dir().then_some(project)
}

fn main() {
    // `REGIA_SPAN_REPORT=N`: `sponza_1080p_runtime_fps` also prints its N heaviest spans and
    // systems (`docs/perf-log.md`); that needs the engine's logging with a frame profiler.
    let mut spans = match regia::core::profiling::RunTotals::install_from_env() {
        Ok(Some(spans)) => Some(spans),
        Ok(None) => {
            tracing_subscriber::fmt::init();
            None
        }
        Err(err) => {
            eprintln!("render_lab: span report logging: {err}");
            tracing_subscriber::fmt::init();
            None
        }
    };
    let args: Vec<String> = env::args().collect();
    let scene = args.get(1).map(String::as_str).unwrap_or("help");

    match scene {
        "--window" => {
            // `render_lab --window sponza [fps] [screenshot-path]`: the runtime
            // ECS path in a real window (orbit/fly camera, agent screenshots).
            let model = match args.get(2).map(String::as_str) {
                Some("sponza") => window::WindowModel::Sponza,
                other => {
                    eprintln!(
                        "Usage: render_lab --window <sponza> [--exit-after-frames N] [--screenshot PATH]"
                    );
                    if let Some(name) = other {
                        eprintln!("unknown window model {name:?}");
                    }
                    std::process::exit(2);
                }
            };
            let flag = |name: &str| -> Option<u32> {
                args.iter()
                    .position(|a| a == name)
                    .and_then(|i| args.get(i + 1))
                    .and_then(|v| v.parse().ok())
            };
            let exit_after = flag("--exit-after-frames").unwrap_or(0);
            let screenshot = args
                .iter()
                .position(|a| a == "--screenshot")
                .and_then(|i| args.get(i + 1))
                .map(std::path::PathBuf::from);
            let exit = regia::run_with(
                window::WindowedLab {
                    model,
                    exit_after_frames: exit_after,
                    screenshot,
                },
                |app| {
                    app.insert_resource(WindowSettings {
                        window: Window {
                            // The "regia." prefix lands the window on the
                            // silent dev workspace (see the worker rules).
                            title: "regia.render_lab".into(),
                            ..Default::default()
                        },
                    });
                    // Advertise the agent endpoint so `regia agent call
                    // --project samples/render_lab seeing.screenshot` works.
                    app.insert_resource(regia::agent::AgentSettings {
                        project_dir: sample_project_dir(),
                    });
                },
            );
            std::process::exit(match exit {
                AppExit::Success => 0,
                AppExit::Error(code) => i32::from(code.get()),
            });
        }
        "analyze_sponza" => {
            // Camera-position + structure probe: prints the model bounds, the
            // planned camera and an ASCII luminance map of the rendered view.
            let path = ensure_model("Sponza").expect("Sponza in cache");
            let lab = load_gltf_model_with_view(&path, LabView::SponzaAtrium, None).expect("load");
            println!(
                "scene: {} meshes, {} materials",
                lab.meshes.len(),
                lab.materials.len()
            );
            println!(
                "camera: eye {:?} target {:?} fov {:.1}° near {:.3} far {:.1}",
                lab.eye,
                lab.target,
                lab.fov_y.to_degrees(),
                lab.near,
                lab.far
            );
            let mut lo = [f32::MAX; 3];
            let mut hi = [f32::MIN; 3];
            let mut tris = 0usize;
            let mut verts = 0usize;
            for mesh in &lab.meshes {
                tris += mesh.indices.len() / 3;
                verts += mesh.vertices.len();
                let model = Mat4::from_cols_array_2d(&mesh.model);
                for v in &mesh.vertices {
                    let wp = model.transform_point3(Vec3::from_array(v.position));
                    for c in 0..3 {
                        lo[c] = lo[c].min(wp[c]);
                        hi[c] = hi[c].max(wp[c]);
                    }
                }
            }
            println!("world bounds: lo {lo:?} hi {hi:?}");
            println!("triangles: {tris}, vertices: {verts}");
            // Line-of-sight probe: cast rays from the camera and report the
            // first triangle hit distance, proving the camera sits in the open
            // hall with a clear view down the nave (not inside a wall).
            let dirs = [
                ("center (down the nave)", Vec3::new(1.0, 0.0, 0.0)),
                ("up 20°", Vec3::new(1.0, 0.36, 0.0)),
                ("down 20°", Vec3::new(1.0, -0.36, 0.0)),
                ("left 30°", Vec3::new(0.87, 0.0, 0.5)),
                ("right 30°", Vec3::new(0.87, 0.0, -0.5)),
            ];
            let eye = Vec3::from_array(lab.eye);
            for (name, dir) in dirs {
                let dir = dir.normalize();
                let mut best = f32::INFINITY;
                for mesh in &lab.meshes {
                    let model = Mat4::from_cols_array_2d(&mesh.model);
                    for tri in mesh.indices.as_chunks::<3>().0 {
                        let a =
                            model.transform_point3(Vec3::from_array(mesh.vertices[tri[0] as usize].position));
                        let b =
                            model.transform_point3(Vec3::from_array(mesh.vertices[tri[1] as usize].position));
                        let c =
                            model.transform_point3(Vec3::from_array(mesh.vertices[tri[2] as usize].position));
                        if let Some(t) = ray_triangle(eye, dir, a, b, c) {
                            best = best.min(t);
                        }
                    }
                }
                println!("ray {name}: first hit at {best:.2} m");
            }
            let device = RenderDevice::headless().expect("GPU");
            let result = render_lab_scene(&device, &lab).expect("render");
            ascii_map(&result.pixels, result.width, result.height, 64, 28);
            let out = out_dir().join("sponza_analysis.png");
            image::save_buffer(
                &out,
                &result.pixels,
                result.width,
                result.height,
                image::ColorType::Rgba8,
            )
            .expect("save png");
            println!("wrote {}", out.display());
        }
        "material_grid" => {
            let device = RenderDevice::headless().expect("GPU");
            let pixels = render_material_sphere_grid(&device).expect("render");
            save_png("material_grid", 512, 512, &pixels, lit_guard(50.0));
        }
        "many_lights" => {
            let device = RenderDevice::headless().expect("GPU");
            let (pixels, report) = render_many_lights(&device).expect("render");
            println!(
                "many_lights: uploaded {} of {} authored (cap {}, dropped {}, cluster overflow {})",
                report.uploaded, report.authored, report.cap, report.dropped, report.cluster_overflow
            );
            save_png(
                "many_lights",
                160,
                90,
                &pixels,
                // Mean luma 4 with 85% near-black used to pass a visually black
                // corridor. The floor under light 20 is a lit surface: luma ~1
                // fails, and most of the frame cannot be black.
                GoldenGuard {
                    min_lum: 18.0,
                    max_lum: 220.0,
                    max_white_pct: 15.0,
                    max_black_pct: 45.0,
                    min_object_pct: 40.0,
                },
            );
        }
        "clustered_lights" => {
            let device = RenderDevice::headless().expect("GPU");
            let pixels = render_clustered_lights(&device).expect("render");
            save_png("clustered_lights", 512, 512, &pixels, lit_guard(5.0));
        }
        "light_shadows" => {
            let device = RenderDevice::headless().expect("GPU");
            let pixels = render_light_shadows(&device).expect("render");
            save_png("light_shadows", 512, 512, &pixels, lit_guard(50.0));
        }
        "surface_maps" => {
            let device = RenderDevice::headless().expect("GPU");
            let pixels = render_surface_maps(&device).expect("render");
            save_png("surface_maps", 512, 512, &pixels, lit_guard(90.0));
        }
        "alpha_modes" => {
            let device = RenderDevice::headless().expect("GPU");
            let pixels = render_alpha_modes(&device).expect("render");
            save_png("alpha_modes", 512, 512, &pixels, lit_guard(40.0));
        }
        "post_stack" => {
            let device = RenderDevice::headless().expect("GPU");
            let pixels = render_post_stack(&device).expect("render");
            save_png("post_stack", 512, 512, &pixels, lit_guard(10.0));
        }
        "ibl_spheres" => {
            let device = RenderDevice::headless().expect("GPU");
            let pixels = render_ibl_spheres(&device).expect("render");
            save_png("ibl_spheres", 512, 512, &pixels, lit_guard(15.0));
        }
        "sponza" | "skinned_fox" => {
            let (model, view, clip) = if scene == "sponza" {
                ("Sponza", LabView::SponzaAtrium, None)
            } else {
                ("Fox", LabView::ModelPortrait, Some(("Walk", 0.5)))
            };
            let path = ensure_model(model).expect("model in cache");
            let mut lab = load_gltf_model_with_view(&path, view, clip).expect("load");
            lab.width = 512;
            lab.height = 512;
            let device = RenderDevice::headless().expect("GPU");
            let result = render_lab_scene(&device, &lab).expect("render");
            save_png(
                scene,
                result.width,
                result.height,
                &result.pixels,
                lit_guard(15.0),
            );
        }
        "sponza_1080p_runtime_fps" => {
            // The runtime path at 1920×1080: a real ECS world (extraction runs
            // through the same system the window path uses), a persistent
            // FrameRenderer and a persistent target — 600 frames, p50/p95,
            // GPU pass timings where the device supports them.
            use regia::prelude::*;
            use regia_render::{
                ExtractedFrame, FrameRenderer, FrameTarget, RenderProfilePreset, RenderSettings,
                spawn_lab_scene,
            };
            let path = ensure_model("Sponza").expect("Sponza in cache");
            let lab = load_gltf_model_with_view(&path, LabView::SponzaAtrium, None).expect("load");
            let (width, height) = (1920u32, 1080u32);
            let device = RenderDevice::headless().expect("GPU");
            let format = wgpu::TextureFormat::Rgba8UnormSrgb;
            let target = device.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("bench target"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = target.create_view(&wgpu::TextureViewDescriptor::default());

            let mut app = App::new();
            app.init_resource::<ExtractedFrame>();
            app.add_systems(Render, regia_render::extract_frame);
            spawn_lab_scene(app.world_mut(), &lab);
            // The interactive lab stays on the golden baseline. This bench measures
            // the desktop-high path (GPU cull, SSAO, the live profile fields).
            app.world_mut()
                .insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));

            let mut renderer = FrameRenderer::new();
            renderer.set_gpu_timing(&device, true);

            let render = |renderer: &mut FrameRenderer, app: &mut App| {
                let frame = app
                    .world()
                    .resource::<ExtractedFrame>()
                    .frame_scene(0, width, height);
                renderer.render_into(&device, &frame, FrameTarget::full(&view, format, width, height))
            };
            // Warm-up: pipeline compiles, mesh/texture uploads, post chain setup.
            for _ in 0..30 {
                app.update();
                render(&mut renderer, &mut app).expect("warm-up render");
                device
                    .device
                    .poll(wgpu::PollType::wait_indefinitely())
                    .expect("warm-up poll");
                if let Some((totals, _)) = spans.as_mut() {
                    totals.skip_frame();
                }
            }
            let frames = 600u32;
            let mut times = Vec::with_capacity(frames as usize);
            let mut encodes = Vec::with_capacity(frames as usize);
            let mut submit_ms = Vec::with_capacity(frames as usize);
            let mut submit_counts = Vec::with_capacity(frames as usize);
            let mut scenes = Vec::with_capacity(frames as usize);
            let mut polls = Vec::with_capacity(frames as usize);
            let mut pass_shadows = Vec::with_capacity(frames as usize);
            let mut pass_clusters = Vec::with_capacity(frames as usize);
            let mut pass_prepass = Vec::with_capacity(frames as usize);
            let mut pass_cull = Vec::with_capacity(frames as usize);
            let mut pass_ssao = Vec::with_capacity(frames as usize);
            let mut pass_forward = Vec::with_capacity(frames as usize);
            let mut pass_ssgi = Vec::with_capacity(frames as usize);
            let mut pass_ssr = Vec::with_capacity(frames as usize);
            let mut pass_taau = Vec::with_capacity(frames as usize);
            // Two frames stay queued. The third waits on the oldest submission
            // instead of draining the whole queue after every `render_into`.
            let mut in_flight: std::collections::VecDeque<wgpu::SubmissionIndex> =
                std::collections::VecDeque::new();
            for index in 0..frames {
                let start = Instant::now();
                let _extract = tracing::info_span!("extract").entered();
                app.update();
                drop(_extract);
                render(&mut renderer, &mut app).expect("render");
                if let Some(submission) = renderer.take_submission() {
                    in_flight.push_back(submission);
                }
                let mut poll_this = 0.0f64;
                let mut waited = false;
                if in_flight.len() >= 3
                    && let Some(oldest) = in_flight.pop_front()
                {
                    let poll_start = Instant::now();
                    device
                        .device
                        .poll(wgpu::PollType::Wait {
                            submission_index: Some(oldest),
                            timeout: None,
                        })
                        .expect("poll");
                    poll_this = poll_start.elapsed().as_secs_f64() * 1000.0;
                    waited = true;
                }
                let wall = start.elapsed().as_secs_f64() * 1000.0;
                let cpu = renderer.cpu_timings();
                let gpu = renderer.gpu_timings();
                println!(
                    "frame {index}: wall {wall:.2} ms  cpu encode {:.2} submit {:.2} ({} calls) readback {:.2} present {:.2} total {:.2}  gpu scene {:.2} shadows {:.2}  poll {poll_this:.2} (gpu timestamps lag 2 frames)",
                    cpu.encode_ms,
                    cpu.submit_ms,
                    cpu.submits,
                    cpu.readback_ms,
                    cpu.present_ms,
                    cpu.total_ms,
                    gpu.scene_ms,
                    gpu.shadows_ms,
                );
                times.push(wall);
                encodes.push(f64::from(cpu.encode_ms));
                submit_ms.push(f64::from(cpu.submit_ms));
                submit_counts.push(cpu.submits);
                scenes.push(f64::from(gpu.scene_ms));
                pass_shadows.push(f64::from(gpu.shadows_ms));
                pass_clusters.push(f64::from(gpu.clusters_ms));
                pass_prepass.push(f64::from(gpu.prepass_ms));
                pass_cull.push(f64::from(gpu.cull_ms));
                pass_ssao.push(f64::from(gpu.ssao_ms));
                pass_forward.push(f64::from(gpu.forward_ms));
                pass_ssgi.push(f64::from(gpu.ssgi_ms));
                pass_ssr.push(f64::from(gpu.ssr_ms));
                pass_taau.push(f64::from(gpu.taau_ms));
                if waited {
                    polls.push(poll_this);
                }
                if let Some((totals, _)) = spans.as_mut() {
                    totals.end_frame();
                }
            }
            while let Some(oldest) = in_flight.pop_front() {
                device
                    .device
                    .poll(wgpu::PollType::Wait {
                        submission_index: Some(oldest),
                        timeout: None,
                    })
                    .expect("drain");
            }
            times.sort_by(|a, b| a.partial_cmp(b).expect("finite times"));
            let p = |q: f64| times[((q * frames as f64) as usize).min(times.len() - 1)];
            println!(
                "sponza 1920x1080 runtime path: {} frames, p50 {:.2} ms ({:.0} fps), p95 {:.2} ms ({:.0} fps), p99 {:.2} ms ({:.0} fps), mean {:.2} ms",
                frames,
                p(0.50),
                1000.0 / p(0.50),
                p(0.95),
                1000.0 / p(0.95),
                p(0.99),
                1000.0 / p(0.99),
                times.iter().sum::<f64>() / f64::from(frames),
            );
            let pct = |samples: &[f64], q: f64| -> f64 {
                if samples.is_empty() {
                    return 0.0;
                }
                let mut sorted = samples.to_vec();
                sorted.sort_by(|a, b| a.partial_cmp(b).expect("finite times"));
                sorted[((q * sorted.len() as f64) as usize).min(sorted.len() - 1)]
            };
            let submit_count_f: Vec<f64> = submit_counts.iter().map(|count| f64::from(*count)).collect();
            println!(
                "cpu encode p50 {:.2} ms p99 {:.2} ms; cpu submit p50 {:.2} ms p99 {:.2} ms; submit count p50 {:.0}; gpu scene p50 {:.2} ms p99 {:.2} ms; poll p50 {:.2} ms p99 {:.2} ms",
                pct(&encodes, 0.50),
                pct(&encodes, 0.99),
                pct(&submit_ms, 0.50),
                pct(&submit_ms, 0.99),
                pct(&submit_count_f, 0.50),
                pct(&scenes, 0.50),
                pct(&scenes, 0.99),
                pct(&polls, 0.50),
                pct(&polls, 0.99),
            );
            println!(
                "gpu pass p50 ms: shadows {:.2} clusters {:.2} prepass {:.2} cull {:.2} ssao {:.2} forward {:.2} ssgi {:.2} ssr {:.2} taau {:.2}",
                pct(&pass_shadows, 0.50),
                pct(&pass_clusters, 0.50),
                pct(&pass_prepass, 0.50),
                pct(&pass_cull, 0.50),
                pct(&pass_ssao, 0.50),
                pct(&pass_forward, 0.50),
                pct(&pass_ssgi, 0.50),
                pct(&pass_ssr, 0.50),
                pct(&pass_taau, 0.50),
            );
            println!(
                "shadow cache cascades_skipped: {}",
                renderer.shadow_cache_report().cascades_skipped
            );
            println!("GPU: {}", device.capabilities.adapter_name);
            println!(
                "gpu timestamps available: {}",
                device.capabilities.timestamp_query
            );
            let gpu = renderer.gpu_timings();
            println!(
                "gpu passes (last frame): {}, gi cpu {:.2} ms",
                gpu.report(),
                renderer.gi_cpu_ms()
            );
            let (meshes, materials, objects) = renderer.cache_sizes();
            println!("gpu cache: {meshes} meshes, {materials} materials, {objects} objects");
            let draw_calls = app.world().resource::<ExtractedFrame>().meshes.len();
            println!("drawn meshes per frame: {draw_calls}");
            if let Some((totals, rows)) = &spans {
                print!("{}", totals.report(*rows));
            }
        }
        "sponza_1080p_fps" => {
            let path = ensure_model("Sponza").expect("Sponza in cache");
            let mut lab = load_gltf_model_with_view(&path, LabView::SponzaAtrium, None).expect("load");
            lab.width = 1920;
            lab.height = 1080;
            fps_loop("sponza 1080p", &lab);
        }
        "fox_1080p_fps" => {
            let path = ensure_model("Fox").expect("Fox in cache");
            let mut lab =
                load_gltf_model_with_view(&path, LabView::ModelPortrait, Some(("Walk", 0.5))).expect("load");
            lab.width = 1920;
            lab.height = 1080;
            fps_loop("fox 1080p", &lab);
        }
        "stress_100k" => stress_100k(),
        "pt" => {
            if let Err(err) = pt::run(&args[2..]) {
                eprintln!("render_lab pt: {err}");
                std::process::exit(1);
            }
        }
        "outdoor" => outdoor_frame(),
        "planets" => planet_frames(&args),
        "post_volumes" => {
            if let Err(err) = post_volumes::run(&args[2..]) {
                eprintln!("render_lab post_volumes: {err}");
                std::process::exit(1);
            }
        }
        "deck_800p" => deck_800p(),
        "10k_cubes" => {
            let mut draws = Vec::with_capacity(10_000);
            for i in 0..10_000u32 {
                draws.push(DrawItem {
                    key: BatchKey {
                        mesh: 0,
                        material: i % 50,
                    },
                    object_index: i,
                    depth: i as f32 * 0.001,
                });
            }
            sort_opaque(&mut draws, true);
            let batches = batch_instanced(&draws);
            let calls = draw_call_count(&batches);
            println!("10k_cubes instanced draw calls (excl. shadows): {calls}");
            if calls > 60 {
                eprintln!("FAIL: expected <= 60 draw calls, got {calls}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!(
                "Usage: render_lab <material_grid|clustered_lights|many_lights|light_shadows|surface_maps|alpha_modes|ibl_spheres|post_stack|sponza|skinned_fox|analyze_sponza|sponza_1080p_runtime_fps|sponza_1080p_fps|fox_1080p_fps|stress_100k|deck_800p|10k_cubes|outdoor|planets [--out DIR]>\n       render_lab post_volumes [--out DIR]\n       render_lab pt <scene> [--spp N] [--bounces N] [--width W] [--height H] [--out DIR]\n       render_lab --window <sponza> [--exit-after-frames N] [--screenshot PATH]"
            );
        }
    }
}
