// SPDX-License-Identifier: MPL-2.0
//! Collection item templates (ADR-2489, `docs/specs/81-ui-designer.md` § 9): a `repeat` over 1,000
//! items with two item bindings per row. What a frame costs when nothing changed, when one item
//! changed (one pass over all items, one row written), and what instantiating the rows costs.
//! Headless, no GPU. Run with `cargo bench -p regia_ui --bench repeat_1000`.

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use regia_app::prelude::*;
use regia_ecs::prelude::AppTypeRegistry;
use regia_ui::authoring::{UiLibrary, spawn_ui};
use regia_ui::binding::update_bindings;
use regia_ui::{UiMigrators, parse_ui_ron};

const ITEMS: usize = 1000;

const LAYOUT: &str = r#"(version: 9, root: (children: [
    (name: "rows", repeat: (source: "ListVm.items", key: "id"), children: [
        (name: "row", children: [
            (name: "label", text: (spans: ["-"]), bindings: [(source: ".name")]),
            (name: "qty", text: (spans: ["0"]), bindings: [(source: ".count", format: "x{:03}")]),
        ]),
    ]),
]))"#;

#[derive(Reflect, Default, Clone)]
#[reflect(Default)]
struct Item {
    id: u32,
    name: String,
    count: u32,
}

#[derive(Resource, Reflect, Default)]
#[reflect(Resource, Default)]
struct ListVm {
    items: Vec<Item>,
}

fn items() -> Vec<Item> {
    (0..ITEMS as u32)
        .map(|id| Item {
            id,
            name: format!("item {id}"),
            count: id,
        })
        .collect()
}

/// A world with the layout spawned; the rows exist after the first `update_bindings`.
fn world() -> World {
    let mut world = World::new();
    let registry = AppTypeRegistry::default();
    registry.write().register::<ListVm>();
    world.insert_resource(registry);
    world.insert_resource(ListVm { items: items() });
    world.init_resource::<UiLibrary>();
    let file = parse_ui_ron(LAYOUT, "bench", &UiMigrators::default()).expect("layout");
    world
        .resource_mut::<UiLibrary>()
        .templates
        .insert("bench".to_owned(), file);
    spawn_ui(&mut world, "bench").expect("spawn");
    world
}

fn benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("repeat_1000");
    group.sample_size(30);
    group.bench_function("instantiate", |b| {
        b.iter_batched_ref(world, update_bindings, BatchSize::LargeInput);
    });
    let mut settled = world();
    update_bindings(&mut settled);
    group.bench_function("unchanged", |b| {
        b.iter(|| update_bindings(&mut settled));
    });
    let mut n = 0u32;
    group.bench_function("one_item_changed", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            settled.resource_mut::<ListVm>().items[500].count = n;
            update_bindings(&mut settled);
        });
    });
    group.bench_function("one_item_removed", |b| {
        b.iter_batched_ref(
            || {
                let mut world = world();
                update_bindings(&mut world);
                world
            },
            |world| {
                world.resource_mut::<ListVm>().items.remove(500);
                update_bindings(world);
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

criterion_group!(group, benches);
criterion_main!(group);
