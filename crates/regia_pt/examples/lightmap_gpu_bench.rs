// SPDX-License-Identifier: MPL-2.0
//! `lightmap_gpu_bench`: times the GPU lightmap baker against the CPU baker on the same bake and
//! prints one JSON object per line (ADR-2125). Hand-run, never part of the gate.
//!
//! ```text
//! cargo run --release -p regia_pt --features lightmap-gpu --example lightmap_gpu_bench -- \
//!     [--size 256] [--samples 64] [--bounces 3] [--spheres 64] [--threads 16] [--runs 2] [--tile-rays N] \
//!     [--gpu-only]
//! ```
//!
//! The scene is the baker's reference quad and box with `--spheres` spheres of 960 triangles each
//! on the floor and four point lights. The floor bakes at `--size`² texels. Each backend is timed
//! as the best of `--runs` (the GPU's first run, which builds the pipeline, is reported apart). The
//! CPU uses `--threads` threads. The last line compares the two maps (`compare_lightmaps`) against
//! the documented floor; `--gpu-only` skips the CPU and the comparison. Pick the GPU with
//! `REGIA_GPU_UUID` (and `REGIA_GPU_STRICT=1`).

use std::process::ExitCode;
use std::time::Instant;

use regia_core::glam::{Mat4, Vec3};
use regia_pt::PtLight;
use regia_pt::lightmap::{
    LIGHTMAP_GPU_PSNR_FLOOR_DB, LightmapBakeSettings, LightmapGpuControl, LightmapGpuOptions, LightmapScene,
    MeshLightmapRequest, bake_lightmap, bake_lightmap_gpu_with, compare_lightmaps, mesh_layout,
    quad_box_scene,
};
use regia_pt::scenes::uv_sphere;
use regia_render::RenderDevice;
use serde_json::json;

fn arg(args: &[String], name: &str, default: u64) -> u64 {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let size = arg(&args, "--size", 256) as u32;
    let spheres = arg(&args, "--spheres", 64) as u32;
    let runs = arg(&args, "--runs", 2).max(1);
    let settings = LightmapBakeSettings {
        samples: arg(&args, "--samples", 64) as u32,
        bounces: arg(&args, "--bounces", 3) as u32,
        threads: arg(&args, "--threads", 16) as usize,
        ..LightmapBakeSettings::default()
    };
    let options = LightmapGpuOptions {
        tile_rays: arg(&args, "--tile-rays", regia_pt::lightmap::DEFAULT_GPU_TILE_RAYS),
        ..LightmapGpuOptions::default()
    };

    let mut qb = quad_box_scene();
    let ball = qb.scene.add_mesh(uv_sphere(0.12, 32, 16, false));
    let white = qb.scene.add_material(regia_pt::PtMaterial::lambert([0.7; 3]));
    let side = (f64::from(spheres).sqrt().ceil() as u32).max(1);
    for k in 0..spheres {
        let (i, j) = (k % side, k / side);
        let at = |n: u32| -1.8 + 3.6 * (n as f32 + 0.5) / side as f32;
        qb.scene
            .add_instance(ball, white, Mat4::from_translation(Vec3::new(at(i), 0.12, at(j))));
    }
    for (x, z) in [(-1.5, -1.5), (1.5, -1.5), (-1.5, 1.5), (1.5, 1.5)] {
        qb.scene.lights.push(PtLight::Point {
            position: [x, 1.2, z],
            intensity: [2.0, 1.8, 1.5],
        });
    }
    let scene = match LightmapScene::from_pt_scene(&qb.scene) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("scene: {e}");
            return ExitCode::FAILURE;
        }
    };
    let request = MeshLightmapRequest {
        instance: qb.floor,
        name: "floor",
        uv: &qb.floor_uv,
        size: [size, size],
        padding: 2,
    };
    let layout = match mesh_layout(&scene, &request) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("layout: {e}");
            return ExitCode::FAILURE;
        }
    };
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("no GPU: {e}");
            return ExitCode::FAILURE;
        }
    };
    let adapter = device.adapter.get_info().name;

    let mut gpu_best = f64::MAX;
    let mut gpu_map = None;
    for run in 0..=runs {
        let started = Instant::now();
        let result = bake_lightmap_gpu_with(
            &device,
            &scene,
            layout.clone(),
            &[qb.floor],
            &settings,
            &options,
            |_| LightmapGpuControl::Continue,
        );
        let seconds = started.elapsed().as_secs_f64();
        match result {
            Ok((map, report, stats)) => {
                println!(
                    "{}",
                    json!({"backend": "gpu", "run": run, "warm": run > 0, "seconds": seconds, "adapter": adapter,
                        "rays": report.rays, "mrays_per_s": report.rays as f64 / seconds / 1e6,
                        "tiles": stats.tiles, "tile_texels": stats.tile_texels, "tile_samples": stats.tile_samples, "peak_bytes": stats.peak_bytes})
                );
                if run > 0 {
                    gpu_best = gpu_best.min(seconds);
                }
                gpu_map = Some(map);
            }
            Err(e) => {
                eprintln!("gpu bake: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    if args.iter().any(|a| a == "--gpu-only") {
        return ExitCode::SUCCESS;
    }
    let mut cpu_best = f64::MAX;
    let mut cpu_map = None;
    for run in 0..runs {
        let started = Instant::now();
        match bake_lightmap(&scene, layout.clone(), &[qb.floor], &settings, |_, _| {}) {
            Ok((map, report)) => {
                let seconds = started.elapsed().as_secs_f64();
                println!(
                    "{}",
                    json!({"backend": "cpu", "run": run, "seconds": seconds, "threads": settings.threads,
                        "rays": report.rays, "mrays_per_s": report.rays as f64 / seconds / 1e6})
                );
                cpu_best = cpu_best.min(seconds);
                cpu_map = Some(map);
            }
            Err(e) => {
                eprintln!("cpu bake: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    let (Some(cpu), Some(gpu)) = (cpu_map, gpu_map) else {
        return ExitCode::FAILURE;
    };
    match compare_lightmaps(&cpu, &gpu) {
        Ok(c) => {
            println!(
                "{}",
                json!({"summary": true, "size": size, "samples": settings.samples, "bounces": settings.bounces,
                    "triangles": scene.triangle_count(), "cpu_threads": settings.threads,
                    "cpu_seconds": cpu_best, "gpu_seconds": gpu_best, "speedup": cpu_best / gpu_best,
                    "psnr_db": c.psnr_db, "mean_rel": c.mean_rel, "p99_rel": c.p99_rel,
                    "within_floor": c.within(LIGHTMAP_GPU_PSNR_FLOOR_DB)})
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("compare: {e}");
            ExitCode::FAILURE
        }
    }
}
