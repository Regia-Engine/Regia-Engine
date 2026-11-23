// SPDX-License-Identifier: MPL-2.0
//! Spec 121 § 11 "index build": the load of the cooked Switchbacks map (every mouth filled, the
//! full-pitch 4096² lattice) and nothing else, so a profiler sees only the load (ADR-8816). Run
//! pinned to one idle CPU, release (`cargo bench`). `INDEX_BUILD_LOADS` sets the loads per line
//! (default 15); `INDEX_BUILD_ONLY=<name>` runs one line alone (for a profiler).
//!
//! Prints (`perf-micro <name>: n= p50= p99= max=`):
//! - `index_build_section`: `WorldGeom::from_canonical_bytes` of the version 2 canonical section
//!   (the same call as `query_index_build_switchbacks` of the `mixed_calls` bench, whose row is the
//!   budget);
//! - `index_build_file`: `WorldGeom::from_cooked` of the whole file (what a level load calls);
//! - `index_build_section_deferred`: the same with `CountBodyCheck::Deferred` (the count-body hashes
//!   left to the caller), and `index_build_body_check`: `cooked::check_count_body`, the check it
//!   leaves out;
//! - `index_build_decode`: `cooked::decode_canonical` alone (the head, the count-body hashes and
//!   the copy of every count, borders kept);
//! - `index_build_body_hash`: BLAKE3 of the count body in `COUNT_BLOCK_BYTES` blocks alone;
//! - `index_build_alloc_tiles` and `index_build_alloc_one`: the device's floor for holding the
//!   counts, with no decode and no hash: allocating and writing the loaded map's count blocks (one
//!   per ground tile), and the same counts as one block;
//! - `index_build_v1_section`: `WorldGeom::from_canonical_bytes` of the same map's v0.6.0
//!   (version 1) section, which loads through the migration step;
//! - `INDEX_BUILD_BYTES section= head= body= v1_section= file=`.
#![allow(clippy::unwrap_used)]
#[path = "../tests/native_mouths/mod.rs"]
pub mod native_mouths;
use regia_world_geom::WorldGeom;
use regia_world_geom::cooked::{
    COUNT_BLOCK_BYTES, CountBodyCheck, check_count_body, decode_canonical, encode_canonical,
};
use std::time::{Duration, Instant};

fn fmt(d: Duration) -> String {
    format!("{:.3}µs", d.as_nanos() as f64 / 1000.0)
}

fn micro_line(name: &str, mut times: Vec<Duration>) {
    times.sort();
    let n = times.len();
    println!(
        "perf-micro {name}: n={n} p50={} p99={} max={}",
        fmt(times[n / 2]),
        fmt(times[(n - 1) * 99 / 100]),
        fmt(times[n - 1])
    );
}

/// Times `run` `n` times. Each result is dropped after its clock stops, so freeing a loaded map is
/// not part of its load.
#[expect(
    clippy::disallowed_methods,
    reason = "bench timing and its line filter: wall-clock cost of each load, never an input to a query (ADR-8367)"
)]
fn time<T>(name: &str, n: usize, mut run: impl FnMut() -> T) {
    if std::env::var("INDEX_BUILD_ONLY").is_ok_and(|only| only != name) {
        return;
    }
    let mut times = Vec::with_capacity(n);
    for _ in 0..n {
        let start = Instant::now();
        let out = run();
        times.push(start.elapsed());
        drop(std::hint::black_box(out));
    }
    micro_line(name, times);
}

#[expect(
    clippy::disallowed_methods,
    reason = "the bench's run count from the environment, never an input to a query (ADR-8367)"
)]
fn main() {
    let loads: usize = std::env::var("INDEX_BUILD_LOADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15);
    let (output, geom, _layout, _sampled) = native_mouths::world("switchbacks");
    drop(geom);
    let section = encode_canonical(&output.file.canonical).unwrap();
    let file = output.file.encode().unwrap();
    let v1_section = {
        let mut v1 = output.file.canonical.clone();
        v1.canonical_version = 1;
        postcard::to_allocvec(&v1).unwrap()
    };
    drop(output);
    let head_len = u32::from_le_bytes(section[1..5].try_into().unwrap()) as usize;
    let body = &section[5 + head_len..];
    println!(
        "INDEX_BUILD_BYTES section={} head={head_len} body={} v1_section={} file={}",
        section.len(),
        body.len(),
        v1_section.len(),
        file.len()
    );
    time("index_build_section", loads, || {
        WorldGeom::from_canonical_bytes(std::hint::black_box(&section)).unwrap()
    });
    time("index_build_file", loads, || {
        WorldGeom::from_cooked(std::hint::black_box(&file)).unwrap()
    });
    time("index_build_section_deferred", loads, || {
        WorldGeom::from_canonical_bytes_with(std::hint::black_box(&section), CountBodyCheck::Deferred)
            .unwrap()
    });
    time("index_build_body_check", loads, || {
        check_count_body(std::hint::black_box(&section)).unwrap();
    });
    time("index_build_decode", loads, || {
        decode_canonical(std::hint::black_box(&section)).unwrap()
    });
    time("index_build_body_hash", loads, || {
        for block in body.chunks(COUNT_BLOCK_BYTES) {
            std::hint::black_box(blake3::hash(std::hint::black_box(block)));
        }
    });
    // The device's floor for holding the counts: what its allocator and its page faults cost for
    // the loaded map's count blocks (one block per ground tile) and for the same counts in one
    // block, each written once. No decode, no hash.
    let shape: Vec<usize> = WorldGeom::from_canonical_bytes(&section)
        .unwrap()
        .cooked()
        .ground_counts
        .iter()
        .map(Vec::len)
        .collect();
    time("index_build_alloc_tiles", loads, || {
        shape
            .iter()
            .map(|&len| vec![std::hint::black_box(1u16); len])
            .collect::<Vec<_>>()
    });
    time("index_build_alloc_one", loads, || {
        vec![std::hint::black_box(1u16); shape.iter().sum()]
    });
    time("index_build_v1_section", loads.min(5), || {
        WorldGeom::from_canonical_bytes(std::hint::black_box(&v1_section)).unwrap()
    });
}
