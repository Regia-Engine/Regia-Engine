// SPDX-License-Identifier: MPL-2.0
//! Native/compat column costs on the full T3 query set. Run pinned to one idle CPU.
#![allow(clippy::unwrap_used)]
#[path = "../tests/native_support/mod.rs"]
pub mod native_support;
use native_support::*;
use regia_world_geom::SpanBuf;
use regia_world_geom::compat::surface::column;
use std::hint::black_box;
use std::time::Instant;

#[expect(
    clippy::disallowed_methods,
    reason = "bench timing: wall-clock cost of each query call, never an input to one (ADR-8367)"
)]
fn measure(points: &[[f32; 3]], mut query: impl FnMut([f32; 3])) -> (f64, f64) {
    // One call per column and per at-row, in source order, in every measured sweep.
    // Each observation times exactly one call. Timer overhead is retained equally in all kernels.
    for &p in points {
        query(black_box(p));
    }
    let mut times = Vec::new();
    for _ in 0..31 {
        for &p in points {
            let p = black_box(p);
            let start = Instant::now();
            query(p);
            times.push(start.elapsed().as_nanos() as f64);
        }
    }
    times.sort_by(f64::total_cmp);
    (times[times.len() / 2], times[(times.len() - 1) * 99 / 100])
}
/// The whole sweep under one timer: nanoseconds per call without a clock read around each call
/// (the median of 31 sweeps). Diagnostic; the bar reads the per-call medians of `measure`.
#[expect(
    clippy::disallowed_methods,
    reason = "bench timing: wall-clock cost of a sweep of query calls, never an input to one (ADR-8367)"
)]
fn sweep(points: &[[f32; 3]], mut query: impl FnMut([f32; 3])) -> f64 {
    let mut times = Vec::new();
    for _ in 0..31 {
        let start = Instant::now();
        for &p in points {
            query(black_box(p));
        }
        times.push(start.elapsed().as_nanos() as f64 / points.len() as f64);
    }
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}
/// `COLUMN_PROFILE=<support|floors|ground|compat>[:<fixture>]` runs that kernel's sweep alone,
/// 300 times, for a sampling profiler (`perf record`, `simpleperf`), and prints nothing else.
#[expect(
    clippy::disallowed_methods,
    reason = "bench option: which kernel to loop for a profiler, read once before any query (ADR-8367)"
)]
fn profile_request() -> Option<(String, Option<String>)> {
    let value = std::env::var("COLUMN_PROFILE").ok()?;
    let mut parts = value.splitn(2, ':');
    let kernel = parts.next()?.to_owned();
    Some((kernel, parts.next().map(str::to_owned)))
}
fn main() {
    let profile = profile_request();
    for name in FIXTURES {
        if let Some((_, Some(only))) = &profile
            && only != name
        {
            continue;
        }
        let file = read(name);
        let geom = native(&file, name, 16.0);
        let state = geom.new_state();
        let mut ground_cooked = geom.cooked().clone();
        ground_cooked.tiles.clear();
        let ground_geom = regia_world_geom::WorldGeom::from_canonical_bytes(
            &regia_world_geom::cooked::encode_canonical(&ground_cooked).unwrap(),
        )
        .unwrap();
        let ground_state = ground_geom.new_state();
        let reference = mesh(&file, name);
        let terrain = terrain(name);
        let mut points = Vec::new();
        for row in file["columns"].as_array().unwrap() {
            let x = hex(&row["x"]);
            let z = hex(&row["z"]);
            points.push([x, 0.0, z]);
            for at in row["at"].as_array().unwrap() {
                points.push([x, hex(&at[0]), z]);
            }
        }
        if let Some((kernel, _)) = &profile {
            let mut spans = SpanBuf::default();
            for _ in 0..300 {
                for &[x, y, z] in &points {
                    match kernel.as_str() {
                        "support" => {
                            black_box(geom.support(&state, x, z, y));
                        }
                        "floors" => {
                            geom.floors_at(&state, x, z, &mut spans);
                            black_box(&spans);
                        }
                        "ground" => {
                            black_box(ground_geom.support(&ground_state, x, z, y));
                        }
                        _ => {
                            black_box(column(terrain.as_ref(), Some(&reference), x, z).span_at(y));
                        }
                    }
                }
            }
            continue;
        }
        let support = measure(&points, |[x, y, z]| {
            black_box(geom.support(&state, x, z, y));
        });
        let mut spans = SpanBuf::default();
        let floors = measure(&points, |[x, _, z]| {
            geom.floors_at(&state, x, z, &mut spans);
            black_box(&spans);
        });
        let ground = measure(&points, |[x, y, z]| {
            black_box(ground_geom.support(&ground_state, x, z, y));
        });
        eprintln!(
            "COLUMN_DETAIL {name} ground_only_median_ns={:.3} resident_bytes={}",
            ground.0,
            geom.resident_bytes()
        );
        let compat = measure(&points, |[x, y, z]| {
            black_box(column(terrain.as_ref(), Some(&reference), x, z).span_at(y));
        });
        // What one timed call costs with nothing in it (the meter's own floor), and each kernel
        // without the per-call clock reads.
        let floor = measure(&points, |p| {
            black_box(p);
        });
        let support_sweep = sweep(&points, |[x, y, z]| {
            black_box(geom.support(&state, x, z, y));
        });
        let ground_sweep = sweep(&points, |[x, y, z]| {
            black_box(ground_geom.support(&ground_state, x, z, y));
        });
        let compat_sweep = sweep(&points, |[x, y, z]| {
            black_box(column(terrain.as_ref(), Some(&reference), x, z).span_at(y));
        });
        eprintln!(
            "COLUMN_DETAIL {name} timer_floor_median_ns={:.3} sweep_ns_per_call support={support_sweep:.1} ground_only={ground_sweep:.1} compat={compat_sweep:.1} sweep_ratio={:.4}",
            floor.0,
            support_sweep / compat_sweep
        );
        // The same columns moved off the mesh's vertices and seams (the T3 probes sit on them): what
        // a body standing inside a face costs. Diagnostic; the bar reads the T3 points below.
        let inside: Vec<[f32; 3]> = points.iter().map(|&[x, y, z]| [x + 0.37, y, z + 0.21]).collect();
        let inside_support = measure(&inside, |[x, y, z]| {
            black_box(geom.support(&state, x, z, y));
        });
        let inside_compat = measure(&inside, |[x, y, z]| {
            black_box(column(terrain.as_ref(), Some(&reference), x, z).span_at(y));
        });
        eprintln!(
            "COLUMN_DETAIL {name} inside_faces support_median_ns={:.3} support_p99_ns={:.3} compat_median_ns={:.3} ratio={:.4}",
            inside_support.0,
            inside_support.1,
            inside_compat.0,
            inside_support.0 / inside_compat.0
        );
        println!(
            "COLUMN {name} calls={} support_median_ns={:.3} support_p99_ns={:.3} floors_median_ns={:.3} floors_p99_ns={:.3} compat_median_ns={:.3} compat_p99_ns={:.3} ratio={:.4} meets_bar={}",
            points.len(),
            support.0,
            support.1,
            floors.0,
            floors.1,
            compat.0,
            compat.1,
            support.0 / compat.0,
            support.0 <= 150.0 && support.0 / compat.0 <= 0.25
        );
    }
}
