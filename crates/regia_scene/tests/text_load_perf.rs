// SPDX-License-Identifier: MPL-2.0
//! Scaling of the authoring (text) scene load: parse + spawn must grow about linearly with the
//! entity count. `ron` 0.12.2 made a whole-document parse quadratic (see `format.rs`'s
//! `SceneFile::from_ron_str`); this test fails if that, or any other super-linear step, comes back.

use std::time::Instant;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_ecs::reflect::AppTypeRegistry;
use regia_scene::components::Name;
use regia_scene::prefab::PrefabLibrary;
use regia_scene::prelude::*;
use regia_scene::spawn::{SpawnOptions, spawn_scene};

/// Stand-in game component with one numeric field.
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

/// The same shape as the 10k budget test in `formats.rs`: a grid of props in groups of 20 under
/// a parent, each with a transform and a game component.
fn level_text(count: u32) -> String {
    let mut file = SceneFile::new();
    for index in 0..count {
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
            "text_load_perf::LampIntensity".to_owned(),
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
    file.to_ron_string().expect("write")
}

/// Best of `runs` text parse + spawn timings, in milliseconds, with the parse share.
fn text_load_ms(text: &str, count: u32, runs: usize) -> (f64, f64) {
    let mut best = (f64::MAX, 0.0);
    for _ in 0..runs {
        let mut world = test_world();
        let start = Instant::now();
        let parsed = SceneFile::from_ron_str(text).expect("parse text");
        let parse_ms = start.elapsed().as_secs_f64() * 1000.0;
        let spawned = spawn_scene(&mut world, &parsed, &SpawnOptions::default()).expect("spawn");
        let total_ms = start.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(spawned.entities, count as usize);
        assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings.first());
        if total_ms < best.0 {
            best = (total_ms, parse_ms);
        }
    }
    best
}

/// 10k vs 50k entities: linear growth is a 5× ratio, the old quadratic parse was ~25×. The bound
/// (10×) leaves room for allocator and cache effects of the larger world. Best of three runs per size
/// (was two): one descheduled run moved the short small-case time enough to swing the ratio
/// (audit-v035-flaky L2).
#[test]
fn text_load_scales_linearly_from_10k_to_50k_entities() {
    let small_text = level_text(10_000);
    let large_text = level_text(50_000);
    let (small_ms, small_parse) = text_load_ms(&small_text, 10_000, 3);
    let (large_ms, large_parse) = text_load_ms(&large_text, 50_000, 3);
    let ratio = large_ms / small_ms;
    println!(
        "text parse+spawn: 10k {small_ms:.1} ms (parse {small_parse:.1}), 50k {large_ms:.1} ms \
         (parse {large_parse:.1}); ratio {ratio:.2} (linear 5.0)"
    );
    assert!(
        ratio < 10.0,
        "text load grew {ratio:.1}× from 10k to 50k entities ({small_ms:.1} -> {large_ms:.1} ms); \
         expected ~5× (linear)"
    );
}
