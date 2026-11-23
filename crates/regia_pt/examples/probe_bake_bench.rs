// SPDX-License-Identifier: MPL-2.0
//! `probe_bake_bench`: times the probe bake's speed-ups (ADR-862) and prints one JSON object per
//! line.
//!
//! ```text
//! cargo run --release -p regia_pt --example probe_bake_bench -- \
//!     [--scene cornell|furnace|rolling-hills|scattered-hills]... [--runs 3] [--rays 256] [--samples 16] \
//!     [--cpu-only] [--bvh-tris 1000000]
//! ```
//!
//! - `bvh`: one large mesh's BVH built on one thread and on every core (seconds, SAH cost; the
//!   two trees are the same, so the costs agree). No GPU.
//! - `bake`: every built-in scene plus `scattered-hills` (rolling hills with 300 boulders and
//!   four closed houses in detail bricks), or the `--scene` ones, baked in five modes, the best of
//!   `--runs`: `reference` (index-order rays, no path compaction, shaded relocation, every probe
//!   traced), `coherent` (the coherent ray order only), `compact` (path compaction only),
//!   `relocation` (geometry-only relocation and the inside skip only) and `fast` (the defaults:
//!   all of them), with the stage times and counters of `GiBakeReport`. A `speedup` line per
//!   scene divides the reference time by each mode's.
//!
//! The reference mode still has the speed-ups that change nothing a mode could switch off:
//! relocation retracing only moved probes, one GPU wait per batch and the multithreaded BVH. For
//! the time before all of ADR-862, time `regia bake-gi <scene>` built from the commit before
//! it against the same command built from this one.

use std::process::ExitCode;
use std::time::Instant;

use regia_core::glam::{Mat4, Vec3};
use regia_pt::bake_desc::{BUILTIN_SCENES, GiBakeJob, builtin};
use regia_pt::bvh::{Aabb, Bvh};
use regia_pt::probe::{GiBakeReport, GiBakeSettings, ProbeTraceSettings, bake_gi_volume, detail_brick};
use regia_pt::scenes::{merge, quad, uv_sphere};
use regia_pt::{PathTracer, PtMaterial, PtMesh};
use serde_json::json;

/// The bench's own scene, after the built-ins: `rolling-hills` with 300 boulders and four closed
/// houses, each house inside a 1 m detail brick (probes inside walls, buried probes, deep
/// bounces under the boulders).
const SCATTERED_HILLS: &str = "scattered-hills";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("probe_bake_bench: {e}");
            ExitCode::from(1)
        }
    }
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn parse<T: std::str::FromStr>(args: &[String], name: &str) -> Result<Option<T>, String> {
    flag(args, name)
        .map(|v| v.parse().map_err(|_| format!("{name}: bad value {v}")))
        .transpose()
}

fn run(args: &[String]) -> Result<(), String> {
    let runs = parse::<u32>(args, "--runs")?.unwrap_or(3).max(1);
    let bvh_tris: u32 = parse(args, "--bvh-tris")?.unwrap_or(1_000_000);
    bench_bvh(bvh_tris);
    if args.iter().any(|a| a == "--cpu-only") {
        return Ok(());
    }
    let rays: Option<u32> = parse(args, "--rays")?;
    let samples: Option<u32> = parse(args, "--samples")?;
    let named: Vec<&str> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| *a == "--scene")
        .filter_map(|(i, _)| args.get(i + 1).map(String::as_str))
        .collect();
    let scenes: Vec<&str> = if named.is_empty() {
        BUILTIN_SCENES.iter().copied().chain([SCATTERED_HILLS]).collect()
    } else {
        named
    };
    let device = regia_render::RenderDevice::headless().map_err(|e| e.to_string())?;
    let tracer = PathTracer::new(&device).map_err(|e| e.to_string())?;
    for name in scenes {
        let job = bench_job(name).ok_or_else(|| format!("no bench scene {name}"))?;
        let mut base = job.settings;
        if let Some(r) = rays {
            base.trace.rays_per_probe = r;
        }
        if let Some(s) = samples {
            base.trace.samples = s;
        }
        let reference = base.without_shortcuts();
        let modes = [
            ("reference", reference),
            (
                "coherent",
                GiBakeSettings {
                    trace: ProbeTraceSettings {
                        coherent_rays: true,
                        ..reference.trace
                    },
                    ..reference
                },
            ),
            (
                "compact",
                GiBakeSettings {
                    trace: ProbeTraceSettings {
                        compact_paths: true,
                        ..reference.trace
                    },
                    ..reference
                },
            ),
            (
                "relocation",
                GiBakeSettings {
                    trace: reference.trace,
                    ..base
                },
            ),
            ("fast", base),
        ];
        let mut times = Vec::new();
        for (mode, settings) in modes {
            let mut best: Option<GiBakeReport> = None;
            for _ in 0..runs {
                let bake = bake_gi_volume(&tracer, &job.scene, &job.layout, &settings, |_, _, _| true)
                    .map_err(|e| e.to_string())?;
                if best.as_ref().is_none_or(|b| bake.report.seconds < b.seconds) {
                    best = Some(bake.report);
                }
            }
            let report = best.ok_or("no run")?;
            times.push((mode, report.seconds));
            println!(
                "{}",
                json!({
                    "bench": "bake",
                    "scene": name,
                    "mode": mode,
                    "runs": runs,
                    "rays_per_probe": settings.trace.rays_per_probe,
                    "samples": settings.trace.samples,
                    "report": report,
                })
            );
        }
        let reference_seconds = times.first().map_or(0.0, |t| t.1);
        let speedup: serde_json::Map<String, serde_json::Value> = times
            .iter()
            .map(|(mode, s)| {
                let x = if *s > 0.0 { reference_seconds / s } else { 0.0 };
                ((*mode).to_owned(), json!(x))
            })
            .collect();
        println!(
            "{}",
            json!({ "bench": "speedup", "scene": name, "over_reference": speedup })
        );
    }
    Ok(())
}

/// A built-in scene, or `SCATTERED_HILLS`.
fn bench_job(name: &str) -> Option<GiBakeJob> {
    if name != SCATTERED_HILLS {
        return builtin(name);
    }
    let mut job = builtin("rolling-hills")?;
    let terrain = job.scene.terrain.clone()?;
    let ground = |x: f32, z: f32| terrain.height_at(x, z).unwrap_or(0.0);
    let rock = job.scene.add_mesh(uv_sphere(1.0, 24, 12, false));
    let stone = job.scene.add_material(PtMaterial::lambert([0.45, 0.42, 0.4]));
    // A fixed linear congruential sequence: the same scene on every run.
    let mut state = 0x9e37_79b9_u32;
    let mut next = move || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 8) as f32 / (1u32 << 24) as f32
    };
    for _ in 0..300 {
        let (x, z) = (next() * 240.0 - 120.0, next() * 240.0 - 120.0);
        let r = 1.5 + next() * 2.5;
        let at = Vec3::new(x, ground(x, z) + 0.3 * r, z);
        job.scene.add_instance(
            rock,
            stone,
            Mat4::from_translation(at) * Mat4::from_scale(Vec3::splat(r)),
        );
    }
    let house = job.scene.add_mesh(closed_box([4.0, 3.0, 4.0]));
    let plaster = job.scene.add_material(PtMaterial::lambert([0.7, 0.68, 0.62]));
    for (x, z) in [(-60.0f32, -40.0f32), (35.0, -70.0), (-20.0, 55.0), (70.0, 30.0)] {
        let at = Vec3::new(x, ground(x, z) + 2.5, z);
        job.scene.add_instance(house, plaster, Mat4::from_translation(at));
        let (lo, hi) = (at - Vec3::new(6.0, 4.0, 6.0), at + Vec3::new(6.0, 5.0, 6.0));
        job.layout
            .bricks
            .push(detail_brick(lo.to_array(), hi.to_array(), 1.0));
    }
    job.layout.assign_brick_offsets();
    Some(job)
}

/// A closed box of half extents `half` around the origin, faces wound outward.
fn closed_box(half: [f32; 3]) -> PtMesh {
    let [x, y, z] = half;
    let faces = [
        [[x, -y, -z], [x, y, -z], [x, y, z], [x, -y, z]],
        [[-x, -y, -z], [-x, -y, z], [-x, y, z], [-x, y, -z]],
        [[-x, y, -z], [-x, y, z], [x, y, z], [x, y, -z]],
        [[-x, -y, -z], [x, -y, -z], [x, -y, z], [-x, -y, z]],
        [[-x, -y, z], [x, -y, z], [x, y, z], [-x, y, z]],
        [[-x, -y, -z], [-x, y, -z], [x, y, -z], [x, -y, -z]],
    ];
    let mut mesh = PtMesh::default();
    for f in faces {
        merge(&mut mesh, &quad(f));
    }
    mesh
}

/// One `tris`-triangle sphere's BVH, single-threaded and on every core.
fn bench_bvh(tris: u32) {
    // A uv sphere has 2·segments·rings triangles; rings = segments / 2.
    let segments = (f64::from(tris).sqrt() as u32).max(8);
    let mesh = uv_sphere(10.0, segments, segments / 2, false);
    let bounds: Vec<Aabb> = mesh
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|t| {
            Aabb::from_points(&[
                mesh.positions[t[0] as usize],
                mesh.positions[t[1] as usize],
                mesh.positions[t[2] as usize],
            ])
        })
        .collect();
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let start = Instant::now();
    let one = Bvh::build_with_threads(&bounds, 1);
    let single = start.elapsed().as_secs_f64();
    let start = Instant::now();
    let many = Bvh::build_with_threads(&bounds, threads);
    let parallel = start.elapsed().as_secs_f64();
    let speedup = if parallel > 0.0 { single / parallel } else { 0.0 };
    let same_tree = one.nodes == many.nodes && one.order == many.order;
    println!(
        "{}",
        json!({
            "bench": "bvh",
            "triangles": bounds.len(),
            "threads": threads,
            "single_seconds": single,
            "parallel_seconds": parallel,
            "speedup": speedup,
            "sah_cost": many.sah_cost(),
            "same_tree": same_tree,
        })
    );
}
