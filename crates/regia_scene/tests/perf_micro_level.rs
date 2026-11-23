// SPDX-License-Identifier: MPL-2.0
//! Save and load of a 10,000-entity level (spec 14 A2, ADR-2704): the budgeted cooked load, the text
//! load `tests/formats.rs` bounds, and the save a saving editor pays, none of which had a number per
//! operation. `#[ignore]`: the gate never runs these, and they assert only that the work happened,
//! never a time (`tests/formats.rs` keeps the budget's own test). They print the
//! `docs/06-testing-and-ci.md` § 9 lines, and `cargo xtask perf-budgets` reads them through
//! `docs/perf/budgets.ron`:
//!
//! ```text
//! cargo test -p regia_scene --release --test perf_micro_level -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Its own test binary: it installs a counting global allocator. The shared counter (ADR-3291) costs a
//! few thread-local and relaxed atomic adds per allocation in the timed tests too, which is small
//! next to the allocation itself.

use std::hint::black_box;
use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_ecs::reflect::AppTypeRegistry;
use regia_scene::components::Name;
use regia_scene::cooked::CookedScene;
use regia_scene::prefab::PrefabLibrary;
use regia_scene::prelude::*;
use regia_scene::spawn::{SnapshotFilter, SpawnOptions, snapshot_scene, spawn_scene};

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): this thread's requests and bytes.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

const COUNT: u32 = 10_000;
/// Samples of each timed operation; the whole operation is many milliseconds.
const SAMPLES: usize = 15;

/// Stand-in game component with one numeric field, as in `tests/formats.rs`.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct LampIntensity {
    /// Candela.
    pub intensity: f32,
}

fn test_world() -> World {
    let mut world = World::new();
    world.insert_resource(AppTypeRegistry::default());
    {
        let handle = world.resource::<AppTypeRegistry>().0.clone();
        let mut registry = handle.write();
        regia_scene::components::register_primitives(&mut registry);
        registry.register::<Transform>();
        registry.register::<Name>();
        registry.register::<LampIntensity>();
    }
    world.init_resource::<PrefabLibrary>();
    world
}

/// The level shape of the budget's test in `tests/formats.rs`: a grid of props in groups of 20 under
/// a parent, each with a transform and a game component.
fn level() -> SceneFile {
    let mut file = SceneFile::new();
    for index in 0..COUNT {
        let mut components = std::collections::BTreeMap::new();
        components.insert(
            "regia_scene::transform::Transform".to_owned(),
            ron::value::RawValue::from_ron(&format!(
                "(translation: ({:.1}, 0.0, {:.1}))",
                (index % 100) as f32,
                (index / 100) as f32
            ))
            .expect("ron")
            .to_owned(),
        );
        components.insert(
            "perf_micro_level::LampIntensity".to_owned(),
            ron::value::RawValue::from_ron("(intensity: 50.0)")
                .expect("ron")
                .to_owned(),
        );
        file.entities.push(regia_scene::format::SceneEntity {
            guid: Guid::from_u128(u128::from(index) + 1),
            name: Some(format!("Prop {index}")),
            parent: (index % 20 != 0).then(|| Guid::from_u128(u128::from(index - index % 20) + 1)),
            components,
            prefab_instance: None,
        });
    }
    file.validate().expect("generated level is valid");
    file
}

/// A world with the level in it, ready to save.
fn spawned(file: &SceneFile) -> World {
    let mut world = test_world();
    let spawned = spawn_scene(&mut world, file, &SpawnOptions::default()).expect("spawn");
    assert_eq!(spawned.entities, COUNT as usize);
    assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings.first());
    world
}

fn cooked_bytes(file: &SceneFile) -> Vec<u8> {
    let world = test_world();
    let registry = world.resource::<AppTypeRegistry>().0.clone();
    let (cooked, warnings) = CookedScene::cook(file, &registry.read()).expect("cook");
    assert!(warnings.is_empty(), "{warnings:?}");
    cooked.to_bytes().expect("bytes")
}

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

/// The runtime path of the budget: parse the cooked bytes and spawn, in a fresh world.
#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_cooked_load() {
    let bytes = cooked_bytes(&level());
    let (mut parse, mut whole) = (Vec::new(), Vec::new());
    for _ in 0..SAMPLES {
        let mut world = test_world();
        let start = Instant::now();
        let scene = CookedScene::from_bytes(black_box(&bytes)).expect("parse");
        let parsed = start.elapsed();
        let spawned = scene.spawn(&mut world, &SpawnOptions::default()).expect("spawn");
        whole.push(start.elapsed());
        parse.push(parsed);
        assert_eq!(spawned.entities, COUNT as usize);
    }
    report("level_10k_cooked_load", whole);
    report("level_10k_cooked_parse", parse);
}

/// The authoring path: parse the text and spawn, in a fresh world.
#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_text_load() {
    let text = level().to_ron_string().expect("write");
    let (mut parse, mut whole) = (Vec::new(), Vec::new());
    for _ in 0..SAMPLES {
        let mut world = test_world();
        let start = Instant::now();
        let file = SceneFile::from_ron_str(black_box(&text)).expect("parse");
        let parsed = start.elapsed();
        let spawned = spawn_scene(&mut world, &file, &SpawnOptions::default()).expect("spawn");
        whole.push(start.elapsed());
        parse.push(parsed);
        assert_eq!(spawned.entities, COUNT as usize);
    }
    report("level_10k_text_load", whole);
    report("level_10k_text_parse", parse);
}

/// The save: snapshot the world, then write the text and the cooked form.
#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_save() {
    let file = level();
    let mut world = spawned(&file);
    let registry = world.resource::<AppTypeRegistry>().0.clone();
    let (mut snapshot, mut text, mut cook) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..SAMPLES {
        let start = Instant::now();
        let saved = snapshot_scene(&mut world, &SnapshotFilter::default()).expect("snapshot");
        snapshot.push(start.elapsed());
        assert_eq!(saved.entities.len(), COUNT as usize);

        let start = Instant::now();
        black_box(saved.to_ron_string().expect("write"));
        text.push(start.elapsed());

        let start = Instant::now();
        let (cooked, _) = CookedScene::cook(&saved, &registry.read()).expect("cook");
        black_box(cooked.to_bytes().expect("bytes"));
        cook.push(start.elapsed());
    }
    report("level_10k_snapshot", snapshot);
    report("level_10k_save_text", text);
    report("level_10k_save_cooked", cook);
}

/// Allocation counts: the same on every machine, so `perf-budgets --counts-only` can gate on them.
#[test]
#[ignore = "hand-run allocation counts (docs/06 § 9)"]
fn perf_count_level() {
    let file = level();
    let bytes = cooked_bytes(&file);
    let text = file.to_ron_string().expect("write");
    let mut world = spawned(&file);
    report_count("level_10k_cooked_load_allocs", 1, || {
        let mut fresh = test_world();
        let scene = CookedScene::from_bytes(&bytes).expect("parse");
        let spawned = scene.spawn(&mut fresh, &SpawnOptions::default()).expect("spawn");
        assert_eq!(spawned.entities, COUNT as usize);
    });
    report_count("level_10k_text_load_allocs", 1, || {
        let mut fresh = test_world();
        let parsed = SceneFile::from_ron_str(&text).expect("parse");
        let spawned = spawn_scene(&mut fresh, &parsed, &SpawnOptions::default()).expect("spawn");
        assert_eq!(spawned.entities, COUNT as usize);
    });
    report_count("level_10k_save_text_allocs", 1, || {
        let saved = snapshot_scene(&mut world, &SnapshotFilter::default()).expect("snapshot");
        black_box(saved.to_ron_string().expect("write"));
    });
}
