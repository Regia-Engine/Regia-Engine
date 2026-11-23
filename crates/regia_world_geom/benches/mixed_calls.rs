// SPDX-License-Identifier: MPL-2.0
//! Spec 121 § 11 (plan § 4.7, row W-31): the mixed sim-query tick on the cooked Switchbacks map (every mouth
//! filled, the full-pitch 4096² lattice), its allocations, the load-time index build and the
//! resident sim geometry. Run pinned to one idle CPU, release (`cargo bench`).
//!
//! Prints, for `cargo xtask perf-budgets` (`docs/perf/budgets.ron`):
//! - `perf-micro query_bench_1000_mixed_calls: n= p50= p99= max=`: wall time of one tick of
//!   `mixed_calls::TICK_CALLS` calls, over 1000 measured ticks;
//! - `perf-count query_bench_1000_mixed_calls_allocs: ops= allocs= bytes=`: heap use of those ticks;
//! - `perf-micro query_index_build_switchbacks: n= p50= p99= max=`: `WorldGeom::from_canonical_bytes`
//!   (the query index, height pyramid and ground coordinates built at load);
//! - `perf-micro ground_edit_crater_6m: n= p50= p99= max=`: one D1 crater of 6 m radius on the
//!   tube's ground (spec 121 § 11: at most 0.2 ms); `ground_edit_crater_3m` (a 6 m diameter) and
//!   `ground_edit_crater_6m_on_edited_state` are printed beside it without a budget row, as are
//!   `query_bench_mixed_calls_on_edited_state` (the tick after 30 craters) and, on a small `Nested`
//!   map, `ground_edit_crater_6m_nested_first_touch` (the edit that promotes a coarse tile) and
//!   `ground_edit_crater_6m_nested_promoted` (ADR-8818);
//! - `MIXED_KIND <name> calls= median_ns_per_call=` per call family and `MIXED_RESIDENT` bytes.
#![allow(clippy::unwrap_used)]
#[path = "../tests/mixed_calls/mod.rs"]
pub mod mixed_calls;
#[path = "../tests/native_mouths/mod.rs"]
pub mod native_mouths;
use mixed_calls::{Kind, MixedTick, TICK_CALLS};
use regia_world_geom::WorldGeom;
use regia_world_geom::allocation_counter as alloc_counter;
use std::time::{Duration, Instant};

#[global_allocator]
static GLOBAL: alloc_counter::CountingAlloc = alloc_counter::CountingAlloc::new();

/// Measured ticks (the `1000` of the entry name).
const TICKS: usize = 1000;

fn fmt(d: Duration) -> String {
    format!("{:.3}µs", d.as_nanos() as f64 / 1000.0)
}

fn micro_line(name: &str, mut times: Vec<Duration>) -> Duration {
    times.sort();
    let n = times.len();
    let p50 = times[n / 2];
    let p99 = times[(n - 1) * 99 / 100];
    let max = times[n - 1];
    println!(
        "perf-micro {name}: n={n} p50={} p99={} max={}",
        fmt(p50),
        fmt(p99),
        fmt(max)
    );
    p50
}

#[expect(
    clippy::disallowed_methods,
    reason = "bench timing: wall-clock cost of each tick and each load, never an input to a query (ADR-8367)"
)]
fn main() {
    let (output, geom, _layout, sampled) = native_mouths::world("switchbacks");
    let state = geom.new_state();
    let tick = MixedTick::new(&sampled, native_mouths::acceptance_ground_m("switchbacks"), 640.0);
    // Warm: caches, branch predictors.
    for _ in 0..50 {
        tick.run(&geom, &state);
    }
    let mut times = Vec::with_capacity(TICKS);
    let mut allocs = alloc_counter::AllocStats::default();
    for _ in 0..TICKS {
        let start = Instant::now();
        let ((), stats) = alloc_counter::measure(|| tick.run(&geom, &state));
        times.push(start.elapsed());
        allocs.allocs += stats.allocs;
        allocs.bytes += stats.bytes;
    }
    let p50 = micro_line("query_bench_1000_mixed_calls", times);
    println!(
        "perf-count query_bench_1000_mixed_calls_allocs: ops={TICKS} allocs={} bytes={}",
        allocs.allocs, allocs.bytes
    );
    println!(
        "MIXED_TICK calls={TICK_CALLS} p50_ns_per_call={:.1} budget_ms buildhost=0.25 deck=0.5 android=0.8",
        p50.as_nanos() as f64 / TICK_CALLS as f64
    );
    for kind in Kind::ALL {
        let mut per = Vec::with_capacity(200);
        for _ in 0..200 {
            let start = Instant::now();
            tick.run_kind(kind, &geom, &state);
            per.push(start.elapsed());
        }
        per.sort();
        println!(
            "MIXED_KIND {} calls={} median_ns_per_call={:.1}",
            kind.name(),
            kind.calls(),
            per[per.len() / 2].as_nanos() as f64 / kind.calls() as f64
        );
    }
    // Where a sight line's time goes: the lattice ground alone, the layer triangles alone.
    for (name, mask) in [
        ("ground", regia_world_geom::FaceMask::GROUND),
        (
            "triangles",
            regia_world_geom::FaceMask(
                regia_world_geom::FaceMask::SOLID.0 & !regia_world_geom::FaceMask::GROUND.0,
            ),
        ),
    ] {
        let mut per = Vec::with_capacity(100);
        for _ in 0..100 {
            let start = Instant::now();
            for &(a, b) in &tick.los {
                std::hint::black_box(geom.segment(&state, std::hint::black_box(a), b, mask));
            }
            per.push(start.elapsed());
        }
        per.sort();
        println!(
            "MIXED_LOS_PART {name} median_ns_per_call={:.1}",
            per[per.len() / 2].as_nanos() as f64 / tick.los.len() as f64
        );
    }
    // Shell segments: the tube half and the outdoor half.
    let half = tick.segments.len() / 2;
    for (name, list) in [
        ("tube", &tick.segments[..half]),
        ("outdoor", &tick.segments[half..]),
    ] {
        let mut per = Vec::with_capacity(100);
        for _ in 0..100 {
            let start = Instant::now();
            for &(a, b) in list {
                std::hint::black_box(geom.segment(
                    &state,
                    std::hint::black_box(a),
                    b,
                    regia_world_geom::FaceMask::SOLID,
                ));
            }
            per.push(start.elapsed());
        }
        per.sort();
        println!(
            "MIXED_SEGMENT_PART {name} median_ns_per_call={:.1}",
            per[per.len() / 2].as_nanos() as f64 / list.len() as f64
        );
    }
    // The load-time index build: decode the canonical bytes and build the query index, the
    // height pyramid and the ground coordinates.
    let bytes = regia_world_geom::cooked::encode_canonical(&output.file.canonical).unwrap();
    let mut builds = Vec::with_capacity(9);
    for _ in 0..9 {
        let start = Instant::now();
        let loaded = WorldGeom::from_canonical_bytes(&bytes).unwrap();
        builds.push(start.elapsed());
        std::hint::black_box(&loaded);
    }
    micro_line("query_index_build_switchbacks", builds);
    // Two reference parts (format v2, ADR-8816): the tools' decode (`decode_canonical`: the head,
    // the count-body hashes and a copy of every count, borders kept) and `content_hash` (the
    // hashed prefix only). The `index_build` bench splits the load itself.
    let mut decodes = Vec::with_capacity(9);
    let mut hashes = Vec::with_capacity(9);
    for _ in 0..9 {
        let start = Instant::now();
        let cooked = regia_world_geom::cooked::decode_canonical(&bytes).unwrap();
        cooked.check().unwrap();
        decodes.push(start.elapsed());
        std::hint::black_box(&cooked);
        let start = Instant::now();
        std::hint::black_box(regia_world_geom::cooked::content_hash_of_bytes(&bytes));
        hashes.push(start.elapsed());
    }
    micro_line("query_load_decode_switchbacks", decodes);
    micro_line("query_load_hash_switchbacks", hashes);
    // D1: a crater at tube points (outside the timer: a fresh state each time). The budget row is
    // the 6 m radius; the 3 m radius (a 6 m diameter) is printed beside it for the owner's ruling
    // on which of the two spec 121 § 11's "6 m crater" means (ADR-8662 D6, ADR-8818).
    let crater_at = |i: usize, radius_mm: i32| {
        let [x, _, z] = tick.floors[(i * 13) % tick.floors.len()];
        regia_world_geom::GroundEdit::Crater {
            x_mm: regia_world_geom::m_to_mm(x),
            z_mm: regia_world_geom::m_to_mm(z),
            radius_mm,
            depth_mm: 450,
        }
    };
    for (name, radius_mm) in [("ground_edit_crater_6m", 6_000), ("ground_edit_crater_3m", 3_000)] {
        let mut craters = Vec::with_capacity(31);
        let mut vertices = 0;
        for i in 0..31 {
            let edit = crater_at(i, radius_mm);
            let mut fresh = geom.new_state();
            let start = Instant::now();
            let applied = fresh.apply_ground_edit(&geom, &edit);
            craters.push(start.elapsed());
            vertices += fresh.ground_edits().override_count();
            std::hint::black_box((applied.is_ok(), &fresh));
        }
        micro_line(name, craters);
        println!(
            "CRATER {name} radius_mm={radius_mm} mean_vertices_written={}",
            vertices / 31
        );
    }
    // The same tick on a state with one D1 crater (plan row W-11, ADR-8815; the edited ranges of
    // ADR-8818 bound its pyramid walk): what one crater costs every later segment and sight line.
    // No budget row: a number for the owner.
    {
        let [x, _, z] = tick.floors[0];
        let mut edited = geom.new_state();
        let applied = edited.apply_ground_edit(
            &geom,
            &regia_world_geom::GroundEdit::Crater {
                x_mm: regia_world_geom::m_to_mm(x),
                z_mm: regia_world_geom::m_to_mm(z),
                radius_mm: 6_000,
                depth_mm: 450,
            },
        );
        let mut per = Vec::with_capacity(21);
        for _ in 0..21 {
            let start = Instant::now();
            tick.run(&geom, &edited);
            per.push(start.elapsed());
        }
        per.sort();
        println!(
            "MIXED_TICK_EDITED one_crater applied={} p50_us={:.1}",
            applied.is_ok(),
            per[per.len() / 2].as_nanos() as f64 / 1000.0
        );
    }
    // What a host does next: the edit's rectangle as `TerrainHeightEdits` heights for the renderer.
    let mut rects = Vec::with_capacity(31);
    for i in 0..31 {
        let mut state = geom.new_state();
        let dirty = state.apply_ground_edit(&geom, &crater_at(i, 6_000)).unwrap();
        let mut out = Vec::new();
        let start = Instant::now();
        state.height_rects(&geom, dirty, &mut out);
        rects.push(start.elapsed());
        std::hint::black_box(&out);
    }
    micro_line("ground_edit_height_rects_6m", rects);
    // The same 6 m crater on a state that already holds 30 others (shared override blocks, the
    // way a sim that snapshots its state every tick sees it): printed, no budget row.
    let mut edited = geom.new_state();
    for i in 0..30 {
        let applied = edited.apply_ground_edit(&geom, &crater_at(i * 7 + 3, 6_000));
        std::hint::black_box(applied.is_ok());
    }
    let mut craters = Vec::with_capacity(31);
    for i in 0..31 {
        let edit = crater_at(i, 6_000);
        let mut state = edited.clone();
        let start = Instant::now();
        let applied = state.apply_ground_edit(&geom, &edit);
        craters.push(start.elapsed());
        std::hint::black_box((applied.is_ok(), &state));
    }
    micro_line("ground_edit_crater_6m_on_edited_state", craters);
    // The mixed tick on that edited state (30 craters along the tube): what a match pays per tick
    // once the ground has been edited. Printed, no budget row.
    for _ in 0..20 {
        tick.run(&geom, &edited);
    }
    let mut times = Vec::with_capacity(200);
    for _ in 0..200 {
        let start = Instant::now();
        tick.run(&geom, &edited);
        times.push(start.elapsed());
    }
    micro_line("query_bench_mixed_calls_on_edited_state", times);
    for kind in Kind::ALL {
        let mut per = Vec::with_capacity(50);
        for _ in 0..50 {
            let start = Instant::now();
            tick.run_kind(kind, &geom, &edited);
            per.push(start.elapsed());
        }
        per.sort();
        println!(
            "MIXED_KIND_EDITED {} calls={} median_ns_per_call={:.1}",
            kind.name(),
            kind.calls(),
            per[per.len() / 2].as_nanos() as f64 / kind.calls() as f64
        );
    }
    nested_promotion();
    let parts = geom.resident_parts();
    println!(
        "MIXED_RESIDENT switchbacks resident_bytes={} lattice_counts={} cooked_triangles={} query_index={} height_pyramid={} triangles={} canonical_bytes={}",
        geom.resident_bytes(),
        parts[0],
        parts[1],
        parts[3],
        parts[4],
        geom.triangles().len(),
        bytes.len()
    );
}

/// D1 on a `Nested` map (spec 121 § 3 seam rule 6): the first edit in a coarse ground tile promotes
/// it to full pitch before the profile runs. A 1025-sample lattice at Switchbacks' pitch (0.3125 m),
/// ground tiles of 64 cells, a plane (so every tile cooks at the coarsest level, 3). Prints the
/// first 6 m crater in a tile (one promotion) and a second one on the promoted tile, no budget row.
#[expect(
    clippy::disallowed_methods,
    reason = "bench timing: wall-clock cost of each edit, never an input to a query (ADR-8367)"
)]
fn nested_promotion() {
    use regia_world_geom::cook::{CookInputs, GroundInput, cook};
    let n = 1025u32;
    let ground = GroundInput {
        lattice_n: n,
        width_m: 320.0,
        offset_m: 0.0,
        step_m: 0.001,
        counts: (0..n * n)
            .map(|v| (20_000 + 8 * (v % n) + 16 * (v / n)) as u16)
            .collect(),
    };
    let cooked = cook(&CookInputs {
        document: b"(version:1, ground_tile_cells:64, ground: Nested(max_level: 3), layers:[])",
        ground: &ground,
        terrain: None,
        read_file: &|_| Err("no files".into()),
    })
    .unwrap();
    let geom = WorldGeom::from_canonical_bytes(
        &regia_world_geom::cooked::encode_canonical(&cooked.file.canonical).unwrap(),
    )
    .unwrap();
    let l = geom.cooked().lattice;
    let (mut first, mut second) = (Vec::with_capacity(31), Vec::with_capacity(31));
    let (mut promoted, mut fills) = (0, 0);
    for i in 0..31u32 {
        // The middle of ground tile (2 + i % 6, 2 + i / 6): the 6 m disc stays inside it.
        let centre = |t: u32| l.vertex_mm(t * 64 + 32) as i32;
        let edit = |depth_mm: i32| regia_world_geom::GroundEdit::Crater {
            x_mm: centre(2 + i % 6),
            z_mm: centre(2 + i / 6),
            radius_mm: 6_000,
            depth_mm,
        };
        let mut state = geom.new_state();
        let mut stats = regia_world_geom::EditStats::default();
        let start = Instant::now();
        let applied = state.apply_ground_edit_with_stats(&geom, &edit(450), &mut stats);
        first.push(start.elapsed());
        promoted += stats.tiles_promoted;
        fills += state.ground_edits().override_count();
        let start = Instant::now();
        let again = state.apply_ground_edit(&geom, &edit(300));
        second.push(start.elapsed());
        std::hint::black_box((applied.is_ok(), again.is_ok(), &state));
    }
    micro_line("ground_edit_crater_6m_nested_first_touch", first);
    micro_line("ground_edit_crater_6m_nested_promoted", second);
    println!(
        "CRATER nested mean_tiles_promoted={:.2} mean_overrides_after_first={}",
        f64::from(promoted) / 31.0,
        fills / 31
    );
}
