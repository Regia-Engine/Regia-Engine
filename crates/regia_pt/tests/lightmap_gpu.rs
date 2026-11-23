// SPDX-License-Identifier: MPL-2.0
//! The GPU lightmap baker against the CPU baker (feature `lightmap-gpu`, ADR-2024). Both draw the
//! same random numbers in the same order, so texel by texel they agree up to float rounding: a
//! shader's `sin`, `cos` and `sqrt` round differently, and a ray that grazes an edge can go the other
//! way, so the tolerance is on the mean and a high percentile of the relative difference, on
//! every kind of light the baker has. No golden: the CPU baker is the reference. Runs on lavapipe.

#![cfg(feature = "lightmap-gpu")]

use regia_asset::lightmap::{Coverage, Lightmap};
use regia_pt::lightmap::{
    LIGHTMAP_GPU_PSNR_FLOOR_DB, LightmapBakeReport, LightmapBakeSettings, LightmapGpuControl,
    LightmapGpuOptions, LightmapGpuStats, LightmapGpuTile, LightmapScene, MIN_GPU_TILE_RAYS,
    MeshLightmapRequest, bake_lightmap, bake_lightmap_gpu, bake_lightmap_gpu_with, compare_lightmaps,
    lightmap_gpu_error_code, lightmap_gpu_schedule, lightmap_gpu_schedule_hash, mesh_layout, quad_box_scene,
};
use regia_pt::{PtLight, PtMaterial, PtScene};
use regia_render::RenderDevice;

/// The mean relative difference may reach this.
const MEAN_TOLERANCE: f64 = 0.002;
/// The 99th percentile may reach this: an edge that a rounded ray lands on the other side of.
const P99_TOLERANCE: f64 = 0.03;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn settings() -> LightmapBakeSettings {
    LightmapBakeSettings {
        samples: 32,
        bounces: 3,
        seed: 7,
        filter: false,
        threads: 2,
        ..LightmapBakeSettings::default()
    }
}

/// Bakes the floor of `pt` on both backends.
fn both(
    device: &RenderDevice,
    pt: &PtScene,
    instance: usize,
    uv: &[[f32; 2]],
    s: &LightmapBakeSettings,
) -> ((Lightmap, LightmapBakeReport), (Lightmap, LightmapBakeReport)) {
    let scene = LightmapScene::from_pt_scene(pt).expect("flatten");
    let request = MeshLightmapRequest {
        instance,
        name: "floor",
        uv,
        size: [36, 36],
        padding: 2,
    };
    let cpu = bake_lightmap(
        &scene,
        mesh_layout(&scene, &request).expect("layout"),
        &[instance],
        s,
        |_, _| {},
    )
    .expect("cpu bake");
    let gpu = bake_lightmap_gpu(
        device,
        &scene,
        mesh_layout(&scene, &request).expect("layout"),
        &[instance],
        s,
        |_, _| {},
    )
    .expect("gpu bake");
    (cpu, gpu)
}

/// `(mean, p99)` of the relative difference over the texels both baked.
fn difference(cpu: &Lightmap, gpu: &Lightmap) -> (f64, f64) {
    assert_eq!(
        cpu.coverage, gpu.coverage,
        "the same texels were baked and filled"
    );
    let mut rel: Vec<f64> = Vec::new();
    for y in 0..cpu.height {
        for x in 0..cpu.width {
            if cpu.coverage_at(x, y) != Coverage::BAKED {
                continue;
            }
            let (a, b) = (cpu.irradiance_at(x, y), gpu.irradiance_at(x, y));
            for k in 0..3 {
                let (a, b) = (f64::from(a[k]), f64::from(b[k]));
                let peak = a.abs().max(b.abs());
                // RGB9E5 cannot tell values below 1 / 65536 of the texel's brightest channel.
                rel.push(if peak < 1e-4 { 0.0 } else { (a - b).abs() / peak });
            }
        }
    }
    assert!(!rel.is_empty());
    rel.sort_by(f64::total_cmp);
    let mean = rel.iter().sum::<f64>() / rel.len() as f64;
    (mean, rel[rel.len() * 99 / 100])
}

fn assert_close(what: &str, cpu: &Lightmap, gpu: &Lightmap) {
    let (mean, p99) = difference(cpu, gpu);
    let c = compare_lightmaps(cpu, gpu).expect("compares");
    eprintln!("{what}: mean {mean:.6}, p99 {p99:.6}, psnr {:?} dB", c.psnr_db);
    assert!(mean < MEAN_TOLERANCE, "{what}: mean difference {mean}");
    assert!(p99 < P99_TOLERANCE, "{what}: p99 difference {p99}");
    // The documented tolerance (ADR-2125): the PSNR floor over the texels both baked.
    assert!(c.within(LIGHTMAP_GPU_PSNR_FLOOR_DB), "{what}: {c:?}");
}

/// Sky, the sun's bounce and the direct sun on the baker's reference scene.
#[test]
fn the_gpu_bake_matches_the_cpu_on_the_reference_scene() {
    let device = device();
    let qb = quad_box_scene();
    let s = settings();
    let ((cpu, cpu_report), (gpu, gpu_report)) = both(&device, &qb.scene, qb.floor, &qb.floor_uv, &s);
    assert_eq!(cpu_report.texels_baked, gpu_report.texels_baked);
    assert_eq!(cpu_report.texels_inside, gpu_report.texels_inside);
    assert_eq!(cpu_report.contents, gpu_report.contents);
    assert_close("sky and bounce", &cpu, &gpu);
    let rays = cpu_report.rays.abs_diff(gpu_report.rays) as f64 / cpu_report.rays as f64;
    assert!(
        rays < 0.01,
        "rays: cpu {}, gpu {}",
        cpu_report.rays,
        gpu_report.rays
    );

    let sun = LightmapBakeSettings {
        direct_sun: true,
        ..settings()
    };
    let ((cpu, _), (gpu, report)) = both(&device, &qb.scene, qb.floor, &qb.floor_uv, &sun);
    assert!(report.contents.contains(&"sun_direct"));
    assert_close("direct sun", &cpu, &gpu);

    // Directions come out the same; they are stored as bytes.
    let dirs = |m: &Lightmap, x: u32, y: u32| m.direction_at(x, y).expect("directions");
    let (mut worst, mut compared) = (0.0f32, 0u32);
    for y in 0..cpu.height {
        for x in 0..cpu.width {
            if cpu.coverage_at(x, y) == Coverage::BAKED {
                let (a, b) = (dirs(&cpu, x, y), dirs(&gpu, x, y));
                compared += 1;
                worst = worst.max((0..4).map(|k| (a[k] - b[k]).abs()).fold(0.0, f32::max));
            }
        }
    }
    assert!(compared > 100 && worst < 0.1, "{compared} texels, worst {worst}");
}

/// The comparison can fail: the same bake with another seed (other paths, same scene) is measured
/// as different, and a GPU bake of a dark floor is not the CPU's bright one. Without this a shader
/// that returned the CPU's numbers by accident of the harness, or a metric that reads nothing, would
/// pass the tests above.
#[test]
fn the_comparison_sees_a_different_bake() {
    let device = device();
    let qb = quad_box_scene();
    let scene = LightmapScene::from_pt_scene(&qb.scene).expect("flatten");
    let request = MeshLightmapRequest {
        instance: qb.floor,
        name: "floor",
        uv: &qb.floor_uv,
        size: [36, 36],
        padding: 2,
    };
    let bake = |seed: u32, floor_scene: &LightmapScene| {
        let s = LightmapBakeSettings { seed, ..settings() };
        bake_lightmap_gpu(
            &device,
            floor_scene,
            mesh_layout(floor_scene, &request).expect("layout"),
            &[qb.floor],
            &s,
            |_, _| {},
        )
        .expect("bake")
        .0
    };
    let (a, b) = (bake(7, &scene), bake(8, &scene));
    let (mean, _) = difference(&a, &b);
    eprintln!("another seed: mean {mean:.6}");
    assert!(mean > 1e-4, "another seed must be measured as different: {mean}");

    // No sky: the shader's answer really depends on the scene it was given.
    let mut dark = quad_box_scene();
    dark.scene.sky = regia_pt::PtSky::None;
    let dark_scene = LightmapScene::from_pt_scene(&dark.scene).expect("flatten");
    let lit = bake(7, &scene);
    let unlit = bake(7, &dark_scene);
    let sum = |m: &Lightmap| {
        (0..m.height)
            .flat_map(|y| (0..m.width).map(move |x| (x, y)))
            .map(|(x, y)| f64::from(m.irradiance_at(x, y)[1]))
            .sum::<f64>()
    };
    assert!(
        sum(&lit) > 2.0 * sum(&unlit),
        "lit {}, unlit {}",
        sum(&lit),
        sum(&unlit)
    );
}

/// Point, spot, sphere and quad lights and an emissive surface.
#[test]
fn the_gpu_bake_matches_the_cpu_for_every_kind_of_light() {
    let device = device();
    let mut qb = quad_box_scene();
    qb.scene.lights.extend([
        PtLight::Point {
            position: [-1.0, 1.0, -1.0],
            intensity: [3.0, 2.0, 1.0],
        },
        PtLight::Spot {
            position: [1.5, 1.5, 1.5],
            direction: [0.0, -1.0, 0.0],
            intensity: [1.0, 2.0, 3.0],
            inner_cos: 0.9,
            outer_cos: 0.7,
        },
        PtLight::Sphere {
            position: [-1.5, 0.6, 1.0],
            radius: 0.2,
            radiance: [4.0, 4.0, 2.0],
        },
        PtLight::Quad {
            corner: [-1.5, 1.8, -1.5],
            edge_u: [1.0, 0.0, 0.0],
            edge_v: [0.0, 0.0, 1.0],
            radiance: [2.0, 3.0, 2.0],
            two_sided: false,
        },
    ]);
    // The box glows: an emissive material on a surface that also blocks light.
    let box_material = qb.scene.instances[qb.cube].material;
    qb.scene.materials[box_material] = PtMaterial {
        emissive: [0.0, 1.5, 0.5],
        ..qb.scene.materials[box_material].clone()
    };
    let ((cpu, cpu_report), (gpu, gpu_report)) =
        both(&device, &qb.scene, qb.floor, &qb.floor_uv, &settings());
    assert_eq!(cpu_report.emissive_triangles, gpu_report.emissive_triangles);
    assert!(cpu_report.emissive_triangles > 2, "the quad light and the box");
    assert_eq!(gpu_report.lights, 4);
    assert_close("every light", &cpu, &gpu);
}

/// The frame's extra directional lights (ADR-7254): the reference scene's sun moved into
/// `extra_suns`, with a second, coloured extra light, bakes on the GPU as on the CPU; and the
/// moved sun alone bakes as it did as the sun.
#[test]
fn extra_directional_lights_bake_on_the_gpu_like_the_cpu_and_like_the_sun() {
    let device = device();
    let qb = quad_box_scene();
    let sun = qb.scene.sun.expect("the reference scene has a sun");
    let s = LightmapBakeSettings {
        direct_sun: true,
        ..settings()
    };
    let mut moved = qb.scene.clone();
    moved.sun = None;
    moved.extra_suns = vec![sun];
    let ((_, _), (as_sun, _)) = both(&device, &qb.scene, qb.floor, &qb.floor_uv, &s);
    let ((cpu, cpu_report), (gpu, gpu_report)) = both(&device, &moved, qb.floor, &qb.floor_uv, &s);
    assert!(
        gpu_report.contents.contains(&"sun_direct"),
        "{:?}",
        gpu_report.contents
    );
    assert_eq!(cpu_report.contents, gpu_report.contents);
    assert_close("an extra light: cpu vs gpu", &cpu, &gpu);
    assert_close("the sun as an extra light vs as the sun (gpu)", &as_sun, &gpu);
    let mut two = moved.clone();
    two.extra_suns.push(regia_pt::PtSun {
        toward: [-0.5, 0.7, 0.4],
        color: [0.4, 0.6, 1.0],
        illuminance: 30_000.0,
        angular_radius: 0.0,
    });
    let ((cpu2, _), (gpu2, _)) = both(&device, &two, qb.floor, &qb.floor_uv, &s);
    assert_close("two extra lights", &cpu2, &gpu2);
    let lum = |m: &Lightmap| -> f64 {
        let mut sum = 0.0;
        for y in 0..m.height {
            for x in 0..m.width {
                if m.coverage_at(x, y) == Coverage::BAKED {
                    sum += f64::from(m.irradiance_at(x, y)[2]);
                }
            }
        }
        sum
    };
    assert!(
        lum(&gpu2) > lum(&gpu) * 1.05,
        "the second light adds light: {} vs {}",
        lum(&gpu2),
        lum(&gpu)
    );
}

/// Enough lights that the bake splits into short dispatches: the split changes nothing.
#[test]
fn a_bake_split_by_its_light_count_matches_the_cpu() {
    let device = device();
    let mut qb = quad_box_scene();
    qb.scene.lights.extend((0..300).map(|k| {
        let a = k as f32 * 0.37;
        PtLight::Point {
            position: [2.0 * a.cos(), 1.0 + (k % 5) as f32 * 0.2, 2.0 * a.sin()],
            intensity: [0.02, 0.02, 0.02],
        }
    }));
    let s = LightmapBakeSettings {
        samples: 8,
        bounces: 2,
        ..settings()
    };
    let ((cpu, _), (gpu, report)) = both(&device, &qb.scene, qb.floor, &qb.floor_uv, &s);
    assert_eq!(report.lights, 300);
    assert_close("many lights", &cpu, &gpu);
}

/// The whole pipeline after the estimate is shared: filter, gutters and encoding give a file the
/// reader takes, and the same inputs give the same bytes on a second run.
#[test]
fn a_gpu_bake_is_deterministic_and_the_filter_applies() {
    let device = device();
    let qb = quad_box_scene();
    let s = LightmapBakeSettings {
        // More than one dispatch's worth of samples.
        samples: 40,
        filter: true,
        ..settings()
    };
    let scene = LightmapScene::from_pt_scene(&qb.scene).expect("flatten");
    let request = MeshLightmapRequest {
        instance: qb.floor,
        name: "floor",
        uv: &qb.floor_uv,
        size: [36, 36],
        padding: 2,
    };
    let run = || {
        bake_lightmap_gpu(
            &device,
            &scene,
            mesh_layout(&scene, &request).expect("layout"),
            &[qb.floor],
            &s,
            |_, _| {},
        )
        .expect("bake")
    };
    let (first, report) = run();
    let (second, _) = run();
    assert_eq!(
        first.to_bytes().expect("encodes"),
        second.to_bytes().expect("encodes"),
        "a re-bake is byte-identical"
    );
    let bytes = first.to_bytes().expect("encodes");
    assert_eq!(Lightmap::from_bytes(&bytes).expect("reads back"), first);
    assert!(report.texels_dilated > 0, "the gutters were filled");
    let noise = report.noise.expect("40 samples measure the noise");
    assert!(noise.mean > 0.0 && noise.mean_samples == 40.0, "{noise:?}");
}

/// Adaptive sampling stays in its bounds and spends about what the CPU does: the same streams meet
/// the same stopping rule.
#[test]
fn adaptive_sampling_stays_in_bounds_and_tracks_the_cpu() {
    let device = device();
    let qb = quad_box_scene();
    let s = LightmapBakeSettings {
        samples: 16,
        max_samples: 128,
        noise_target: 0.02,
        ..settings()
    };
    let ((_, cpu), (_, gpu)) = both(&device, &qb.scene, qb.floor, &qb.floor_uv, &s);
    let (cpu, gpu) = (cpu.noise.expect("cpu noise"), gpu.noise.expect("gpu noise"));
    assert!((16.0..=128.0).contains(&gpu.mean_samples), "{gpu:?}");
    assert!(
        gpu.mean_samples > 16.0,
        "a 2 % target needs more somewhere: {gpu:?}"
    );
    let off = (cpu.mean_samples - gpu.mean_samples).abs() / cpu.mean_samples;
    assert!(off < 0.05, "cpu {cpu:?}, gpu {gpu:?}");
}

/// The same checks refuse the same bad inputs before anything reaches the GPU.
#[test]
fn bad_inputs_are_refused_like_the_cpu_baker_refuses_them() {
    let device = device();
    let qb = quad_box_scene();
    let scene = LightmapScene::from_pt_scene(&qb.scene).expect("flatten");
    let request = MeshLightmapRequest {
        instance: qb.floor,
        name: "floor",
        uv: &qb.floor_uv,
        size: [36, 36],
        padding: 2,
    };
    let layout = mesh_layout(&scene, &request).expect("layout");
    let bad = LightmapBakeSettings {
        samples: 0,
        ..settings()
    };
    assert!(bake_lightmap_gpu(&device, &scene, layout.clone(), &[qb.floor], &bad, |_, _| {}).is_err());
    assert!(bake_lightmap_gpu(&device, &scene, layout, &[99], &settings(), |_, _| {}).is_err());
}

/// The floor of the reference scene at 36x36 with `s`, on the GPU with `options`; the tiles the
/// control saw, in order, and the texel count the bake planned for.
fn gpu_run(
    device: &RenderDevice,
    s: &LightmapBakeSettings,
    options: &LightmapGpuOptions,
) -> (Lightmap, LightmapGpuStats, Vec<LightmapGpuTile>, usize) {
    let qb = quad_box_scene();
    let scene = LightmapScene::from_pt_scene(&qb.scene).expect("flatten");
    let request = MeshLightmapRequest {
        instance: qb.floor,
        name: "floor",
        uv: &qb.floor_uv,
        size: [36, 36],
        padding: 2,
    };
    let mut tiles = Vec::new();
    let mut texels = 0usize;
    let (map, _, stats) = bake_lightmap_gpu_with(
        device,
        &scene,
        mesh_layout(&scene, &request).expect("layout"),
        &[qb.floor],
        s,
        options,
        |p| {
            tiles.push(p.tile);
            texels = (p.texel_samples_budget / u64::from(s.sample_cap())) as usize;
            LightmapGpuControl::Continue
        },
    )
    .expect("bake");
    (map, stats, tiles, texels)
}

/// The tile schedule is a pure function of the bake: the tiles run are the planned ones, in
/// order, on a second run too; small tiles, memory chunks and a yield between tiles each change
/// the schedule and leave the bytes of the file exactly as they were.
#[test]
fn the_tile_schedule_is_deterministic_and_never_changes_the_bytes() {
    let device = device();
    let qb = quad_box_scene();
    let scene = LightmapScene::from_pt_scene(&qb.scene).expect("flatten");
    let s = LightmapBakeSettings {
        samples: 40,
        ..settings()
    };
    let default = LightmapGpuOptions::default();
    let (reference, stats, tiles, texels) = gpu_run(&device, &s, &default);
    let bytes = reference.to_bytes().expect("encodes");
    let planned = lightmap_gpu_schedule(&scene, texels, &s, &default, stats.chunk_texels);
    assert_eq!(tiles, planned, "the tiles run are the planned ones");
    assert_eq!(stats.schedule_hash, lightmap_gpu_schedule_hash(&planned));
    assert_eq!(stats.tiles, planned.len() as u64);
    assert_eq!((stats.chunks, stats.chunk_texels as usize), (1, texels));
    assert!(
        stats.peak_bytes > stats.scene_bytes && stats.scene_bytes > 0,
        "{stats:?}"
    );

    let (again, again_stats, again_tiles, _) = gpu_run(&device, &s, &default);
    assert_eq!(
        (again_stats, &again_tiles),
        (stats, &tiles),
        "a second run, the same schedule"
    );
    assert_eq!(again.to_bytes().expect("encodes"), bytes);

    let small = LightmapGpuOptions {
        tile_rays: MIN_GPU_TILE_RAYS,
        ..default
    };
    // About four chunks.
    let budget = LightmapGpuOptions {
        memory_budget: stats.scene_bytes + (stats.peak_bytes - stats.scene_bytes) / 4,
        ..default
    };
    let paced = LightmapGpuOptions {
        tile_rays: MIN_GPU_TILE_RAYS * 4,
        memory_budget: budget.memory_budget,
        yield_between_tiles: std::time::Duration::from_millis(1),
    };
    for (what, options) in [
        ("small tiles", small),
        ("memory chunks", budget),
        ("paced", paced),
    ] {
        let (map, run, tiles, _) = gpu_run(&device, &s, &options);
        let planned = lightmap_gpu_schedule(&scene, texels, &s, &options, run.chunk_texels);
        eprintln!(
            "{what}: {} tiles, {} chunks of {} texels",
            run.tiles, run.chunks, run.chunk_texels
        );
        assert_eq!(tiles, planned, "{what}: the planned tiles");
        assert_eq!(run.schedule_hash, lightmap_gpu_schedule_hash(&planned), "{what}");
        assert!(run.tiles > stats.tiles, "{what}: {run:?}");
        assert_ne!(run.schedule_hash, stats.schedule_hash, "{what}");
        assert_eq!(map.to_bytes().expect("encodes"), bytes, "{what}: the same bytes");
    }
    let (_, chunked, _, _) = gpu_run(&device, &s, &budget);
    assert!(chunked.chunks >= 4, "{chunked:?}");
    assert!(chunked.peak_bytes <= budget.memory_budget, "{chunked:?}");
}

/// Adaptive sampling reads the noise between batches, and still runs the same tiles every time;
/// chunked by memory it gives the same bytes.
#[test]
fn adaptive_sampling_keeps_a_deterministic_schedule_across_chunks() {
    let device = device();
    let s = LightmapBakeSettings {
        samples: 16,
        max_samples: 96,
        noise_target: 0.03,
        ..settings()
    };
    let default = LightmapGpuOptions::default();
    let (a, a_stats, a_tiles, _) = gpu_run(&device, &s, &default);
    let (b, b_stats, b_tiles, _) = gpu_run(&device, &s, &default);
    assert_eq!((a_stats, &a_tiles), (b_stats, &b_tiles));
    assert!(a_stats.readbacks > 1, "noise read between batches: {a_stats:?}");
    let bytes = a.to_bytes().expect("encodes");
    assert_eq!(b.to_bytes().expect("encodes"), bytes);
    let budget = LightmapGpuOptions {
        memory_budget: a_stats.scene_bytes + (a_stats.peak_bytes - a_stats.scene_bytes) / 3,
        ..default
    };
    let (c, c_stats, _, _) = gpu_run(&device, &s, &budget);
    assert!(c_stats.chunks >= 3, "{c_stats:?}");
    assert_eq!(c.to_bytes().expect("encodes"), bytes, "chunks change nothing");
}

/// The control can stop a bake between tiles: no map, `lightmap_gpu_cancelled`, and no tile after
/// the one it stopped at. A budget the scene alone passes is `lightmap_gpu_limit`, and options out
/// of range are `lightmap_gpu_options`, before any tile runs.
#[test]
fn a_bake_can_be_cancelled_and_a_budget_or_option_refuses_it() {
    let device = device();
    let qb = quad_box_scene();
    let scene = LightmapScene::from_pt_scene(&qb.scene).expect("flatten");
    let request = MeshLightmapRequest {
        instance: qb.floor,
        name: "floor",
        uv: &qb.floor_uv,
        size: [36, 36],
        padding: 2,
    };
    let s = settings();
    let small = LightmapGpuOptions {
        tile_rays: MIN_GPU_TILE_RAYS,
        ..LightmapGpuOptions::default()
    };
    let bake = |options: &LightmapGpuOptions, stop_after: u64| {
        let mut seen = 0u64;
        let result = bake_lightmap_gpu_with(
            &device,
            &scene,
            mesh_layout(&scene, &request).expect("layout"),
            &[qb.floor],
            &s,
            options,
            |p| {
                seen = p.tiles_done;
                if p.tiles_done >= stop_after {
                    LightmapGpuControl::Cancel
                } else {
                    LightmapGpuControl::Continue
                }
            },
        );
        (result.map(|(_, _, stats)| stats), seen)
    };
    let (result, seen) = bake(&small, 3);
    let e = result.expect_err("cancelled");
    assert_eq!(lightmap_gpu_error_code(&e), "lightmap_gpu_cancelled", "{e}");
    assert_eq!(seen, 3, "no tile ran after the cancel");

    let tight = LightmapGpuOptions {
        memory_budget: 64,
        ..LightmapGpuOptions::default()
    };
    let (result, seen) = bake(&tight, u64::MAX);
    let e = result.expect_err("over budget");
    assert_eq!(lightmap_gpu_error_code(&e), "lightmap_gpu_limit", "{e}");
    assert_eq!(seen, 0);

    let wrong = LightmapGpuOptions {
        tile_rays: 1,
        ..LightmapGpuOptions::default()
    };
    let (result, _) = bake(&wrong, u64::MAX);
    assert_eq!(
        lightmap_gpu_error_code(&result.expect_err("bad option")),
        "lightmap_gpu_options"
    );
}

/// Progress counts what the CPU baker's counts (texels on a surface), never goes back, and ends
/// at the total; the control sees the same total.
#[test]
fn progress_counts_the_texels_the_cpu_baker_counts() {
    let device = device();
    let qb = quad_box_scene();
    let scene = LightmapScene::from_pt_scene(&qb.scene).expect("flatten");
    let request = MeshLightmapRequest {
        instance: qb.floor,
        name: "floor",
        uv: &qb.floor_uv,
        size: [36, 36],
        padding: 2,
    };
    let s = settings();
    let seen = |gpu: bool| {
        let calls = std::sync::Mutex::new(Vec::new());
        let record = |done: usize, total: usize| {
            if let Ok(mut c) = calls.lock() {
                c.push((done, total));
            }
        };
        let layout = mesh_layout(&scene, &request).expect("layout");
        if gpu {
            bake_lightmap_gpu(&device, &scene, layout, &[qb.floor], &s, record).expect("gpu");
        } else {
            bake_lightmap(&scene, layout, &[qb.floor], &s, record).expect("cpu");
        }
        calls.into_inner().expect("calls")
    };
    let (cpu, gpu) = (seen(false), seen(true));
    let total = cpu.last().expect("cpu progress").1;
    assert!(
        total > 0 && total < 36 * 36,
        "{total}: surface texels, not the atlas"
    );
    assert!(gpu.len() > 1, "{gpu:?}");
    assert!(gpu.iter().all(|&(_, t)| t == total), "{gpu:?}");
    assert!(gpu.windows(2).all(|w| w[0].0 <= w[1].0), "{gpu:?}");
    assert_eq!(gpu.last(), Some(&(total, total)));
    let small = LightmapGpuOptions {
        tile_rays: MIN_GPU_TILE_RAYS,
        ..LightmapGpuOptions::default()
    };
    let mut last = None;
    bake_lightmap_gpu_with(
        &device,
        &scene,
        mesh_layout(&scene, &request).expect("layout"),
        &[qb.floor],
        &s,
        &small,
        |p| {
            assert_eq!(p.texels, total);
            last = Some(p.texels_done());
            LightmapGpuControl::Continue
        },
    )
    .expect("bake");
    assert_eq!(last, Some(total));
}
