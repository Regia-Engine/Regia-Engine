// SPDX-License-Identifier: MPL-2.0
//! Spec 32 § 9: the asset database budgets on a generated 50,000-asset, 400,000-edge registry
//! (ADR-2704). `#[ignore]`: the gate never runs these, and they assert only that the work happened,
//! never a time. They print the `docs/06-testing-and-ci.md` § 9 lines, and `cargo xtask perf-budgets`
//! reads them through `docs/perf/budgets.ron`:
//!
//! ```text
//! cargo test -p regia_asset_pipeline --release --test perf_micro_assetdb -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The graph: ten worlds of 5,000 assets. An asset references the asset before it and seven random
//! earlier ones of its world, so a world's last asset (a scene) reaches exactly its 5,000 assets:
//! the 5,000-asset closure the size map budget names. Everything comes from one xorshift seed.
//!
//! Its own test binary: it installs a counting global allocator. The shared counter (ADR-3291) costs a
//! few thread-local and relaxed atomic adds per allocation in the timed tests too, which is small
//! next to the allocation itself.

use std::collections::BTreeMap;
use std::hint::black_box;
use std::time::{Duration, Instant};

use regia_asset::AssetId;
use regia_asset::registry::{AssetRegistry, RegistryEntry};
use regia_asset_pipeline::asset_audit::{GraphQuery, SizeMapQuery, graph_query, size_map};
use regia_asset_pipeline::asset_graph::{AssetSet, DependencyGraph};
use regia_asset_pipeline::validators::{
    RunSettings, Scope, Trigger, ValidationConfig, ValidationReport, ValidatorRegistry,
};
use regia_core::id::Guid;

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): this thread's requests and bytes.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

const WORLDS: usize = 10;
const WORLD_ASSETS: usize = 5_000;
const ASSETS: usize = WORLDS * WORLD_ASSETS;
const EDGES: usize = 400_000;
/// References of an asset with enough earlier assets.
const REFERENCES: usize = 8;
/// The last assets of a world take one reference more; together they make the edge count exact,
/// since the first assets of a world have too few earlier ones (1 + 2 + ... + 8 = 36 short).
const TOPPED_UP: usize = 36;

fn report(name: &str, mut samples: Vec<Duration>) {
    samples.sort_unstable();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    println!(
        "perf-micro {name}: n={} p50={:?} p99={:?} max={:?}",
        samples.len(),
        at(0.5),
        at(0.99),
        samples[samples.len() - 1]
    );
}

/// Allocations and requested bytes of `ops` calls on this thread, after as many warm calls (lazy
/// tables and reused buffers settle there). Runs twice and asserts the same answer: a count is only
/// worth pinning when it repeats.
fn report_count(name: &str, ops: u64, mut op: impl FnMut()) {
    for _ in 0..ops {
        op();
    }
    let mut measure = || {
        let start = alloc_counter::thread_stats();
        for _ in 0..ops {
            op();
        }
        {
            let used = alloc_counter::thread_stats().since(start);
            (used.allocs, used.bytes)
        }
    };
    let (allocs, bytes) = measure();
    assert_eq!((allocs, bytes), measure(), "{name}: the count does not repeat");
    println!("perf-count {name}: ops={ops} allocs={allocs} bytes={bytes}");
}

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn id(index: usize) -> AssetId {
    AssetId(Guid::from_u128(index as u128 + 1))
}

fn path_of(index: usize) -> String {
    let world = index / WORLD_ASSETS;
    let offset = index % WORLD_ASSETS;
    if offset == WORLD_ASSETS - 1 {
        return format!("content/world{world}/level.scene");
    }
    let (folder, ext) = kind_of(offset);
    format!("content/world{world}/{folder}/asset{offset:04}.{ext}")
}

fn kind_of(offset: usize) -> (&'static str, &'static str) {
    match offset % 5 {
        0 => ("textures", "png"),
        1 => ("meshes", "mesh"),
        2 => ("materials", "material"),
        3 => ("audio", "ogg"),
        _ => ("data", "bin"),
    }
}

fn type_path(offset: usize) -> &'static str {
    if offset == WORLD_ASSETS - 1 {
        return "x::SceneAsset";
    }
    match offset % 5 {
        0 => "x::CookedTexture",
        1 => "x::CookedMesh",
        2 => "x::CookedMaterial",
        3 => "x::CookedAudio",
        _ => "x::CookedTable",
    }
}

/// The registry: see the file header.
fn registry() -> AssetRegistry {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut entries = Vec::with_capacity(ASSETS);
    for index in 0..ASSETS {
        let offset = index % WORLD_ASSETS;
        let base = index - offset;
        let wanted = if offset >= WORLD_ASSETS - TOPPED_UP {
            REFERENCES + 1
        } else {
            REFERENCES
        }
        .min(offset);
        let mut references = Vec::with_capacity(wanted);
        if offset > 0 {
            references.push(offset - 1);
        }
        while references.len() < wanted {
            let candidate = (xorshift(&mut state) % offset as u64) as usize;
            if !references.contains(&candidate) {
                references.push(candidate);
            }
        }
        let mut dependencies: Vec<AssetId> = references.iter().map(|r| id(base + r)).collect();
        dependencies.sort_unstable();
        entries.push(RegistryEntry {
            id: id(index),
            path: path_of(index),
            type_path: type_path(offset).to_owned(),
            dependencies,
            tags: BTreeMap::new(),
            size: xorshift(&mut state) % (1 << 20),
        });
    }
    AssetRegistry::from_entries(entries)
}

struct Fixture {
    registry: AssetRegistry,
    set: AssetSet,
    graph: DependencyGraph,
    validators: ValidatorRegistry,
}

fn fixture() -> Fixture {
    let registry = registry();
    let set = AssetSet::from_registry(&registry);
    assert_eq!(set.assets.len(), ASSETS, "the set has the budget's assets");
    let edges: usize = set.assets.iter().map(|a| a.references.len()).sum();
    assert_eq!(edges, EDGES, "the set has the budget's edges");
    assert_eq!(set.effective_roots().len(), WORLDS);
    let graph = DependencyGraph::build(&set);
    Fixture {
        registry,
        set,
        graph,
        validators: ValidatorRegistry::with_builtins(&ValidationConfig::default()),
    }
}

/// The root of a graph query: a pseudo-random asset in the middle of a world, so both sides have
/// nodes within two hops.
fn query_for(sample: u64) -> GraphQuery {
    let world = (sample as usize) % WORLDS;
    let offset = WORLD_ASSETS / 4 + (sample as usize * 17) % (WORLD_ASSETS / 2);
    GraphQuery {
        roots: vec![path_of(world * WORLD_ASSETS + offset)],
        referrer_depth: 2,
        dependency_depth: 2,
        max_per_column: 50,
        ..GraphQuery::default()
    }
}

fn size_query(sample: usize) -> SizeMapQuery {
    let world = sample % WORLDS;
    SizeMapQuery {
        roots: vec![path_of(world * WORLD_ASSETS + WORLD_ASSETS - 1)],
        ..SizeMapQuery::default()
    }
}

/// A save-time run: ten assets of one world by path (`<= 10 assets`).
fn save_settings() -> RunSettings {
    RunSettings {
        scope: Scope::Assets((0..10).map(|i| path_of(WORLD_ASSETS * 3 + 1_000 + i)).collect()),
        trigger: Trigger::Save,
        ..RunSettings::default()
    }
}

fn expect_rows(report: &ValidationReport, assets: usize) {
    let validators = ValidatorRegistry::with_builtins(&ValidationConfig::default())
        .names()
        .len();
    assert_eq!(
        report.results.len(),
        assets * validators,
        "every asset met every validator"
    );
}

/// Project-wide validation from the registry, bridge included, then each part.
#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_project_validation() {
    let f = fixture();
    let (mut whole, mut bridge, mut validate) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..7 {
        let start = Instant::now();
        let set = AssetSet::from_registry(black_box(&f.registry));
        let bridged = start.elapsed();
        let result = f.validators.run(&set);
        let total = start.elapsed();
        expect_rows(&result, ASSETS);
        whole.push(total);
        bridge.push(bridged);
        validate.push(total - bridged);
    }
    report("assetdb_project_validation_50k_400k", whole);
    report("assetdb_registry_bridge_50k_400k", bridge);
    report("assetdb_validators_50k_400k", validate);
}

#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_graph_build() {
    let f = fixture();
    let mut samples = Vec::new();
    for _ in 0..7 {
        let start = Instant::now();
        let graph = DependencyGraph::build(black_box(&f.set));
        samples.push(start.elapsed());
        black_box(&graph);
    }
    report("assetdb_graph_build_50k_400k", samples);
}

#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_graph_query() {
    let f = fixture();
    let mut listed = 0;
    let mut samples = Vec::new();
    for sample in 0..200 {
        let query = query_for(sample);
        let start = Instant::now();
        let result = graph_query(&f.set, &f.graph, &query).expect("query");
        samples.push(start.elapsed());
        listed += result.columns.iter().map(|c| c.nodes.len()).sum::<usize>();
    }
    assert!(listed > 200, "the queries found neighbors: {listed}");
    report("assetdb_graph_query_2_2_width_50", samples);
}

#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_size_map() {
    let f = fixture();
    let mut samples = Vec::new();
    for sample in 0..30 {
        let query = size_query(sample);
        let start = Instant::now();
        let map = size_map(&f.set, &f.graph, &query).expect("size map");
        samples.push(start.elapsed());
        assert_eq!(map.closure_assets, WORLD_ASSETS as u64, "a 5,000-asset closure");
    }
    report("assetdb_size_map_5k_closure", samples);
}

#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_save_time_run() {
    let f = fixture();
    let settings = save_settings();
    let mut samples = Vec::new();
    for _ in 0..30 {
        let start = Instant::now();
        let result = f.validators.run_with(&f.set, &settings);
        samples.push(start.elapsed());
        expect_rows(&result, 10);
    }
    report("assetdb_save_time_run_10_assets", samples);
}

/// Allocation counts: the same on every machine, so `perf-budgets --counts-only` can gate on them.
#[test]
#[ignore = "hand-run allocation counts (docs/06 § 9)"]
fn perf_count_assetdb() {
    let f = fixture();
    report_count("assetdb_project_validation_allocs", 1, || {
        let set = AssetSet::from_registry(&f.registry);
        expect_rows(&f.validators.run(&set), ASSETS);
    });
    let query = query_for(7);
    report_count("assetdb_graph_query_allocs", 16, || {
        black_box(graph_query(&f.set, &f.graph, &query).expect("query"));
    });
    let sizes = size_query(3);
    report_count("assetdb_size_map_allocs", 2, || {
        black_box(size_map(&f.set, &f.graph, &sizes).expect("size map"));
    });
    let settings = save_settings();
    report_count("assetdb_save_time_run_allocs", 4, || {
        expect_rows(&f.validators.run_with(&f.set, &settings), 10);
    });
}
