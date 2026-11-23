// SPDX-License-Identifier: MPL-2.0
//! A component inside a `repeat` costs what the inline form costs (spec 111 § 3, ADR-3147):
//! spawning a 1,000-item repeat whose row is a component instance allocates no more than the same
//! row written inline. Counted with a counting global allocator around the deterministic,
//! single-threaded spawn and binding pass (no timers). One test per binary, so no other test's
//! allocations are counted.

// The counting global allocator is the only unsafe code (ADR-3147; the pattern of the render
// crate's counter tests).
#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use regia_app::prelude::*;
use regia_ui::authoring::{UiLibrary, spawn_ui};
use regia_ui::node::NodeName;
use regia_ui::ui_component::text_loader;
use regia_ui::{UiMigrators, ViewModelAppExt, parse_ui_ron};

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

#[derive(Reflect, Debug, Default, Clone, PartialEq)]
#[reflect(Default)]
struct Item {
    id: u32,
    name: String,
    count: u32,
}

#[derive(Resource, Reflect, Debug, Default, Clone, PartialEq)]
#[reflect(Resource, Default)]
struct InvVm {
    items: Vec<Item>,
}

const ROW: &str = r#"(version: 9, root: (name: "row", classes: ["row"], children: [
    (name: "label", text: (spans: ["-"]), bindings: [(source: "$.name")]),
    (name: "qty", text: (spans: ["0"]), bindings: [(source: "$.count", format: "x{:03}")]),
]))"#;

const WITH_COMPONENT: &str = r#"(version: 9, root: (children: [
    (name: "rows", repeat: (source: "InvVm.items", key: "id"), children: [
        (use: "ui/row.ui.ron", bind_root: "."),
    ]),
]))"#;

const INLINE: &str = r#"(version: 9, root: (children: [
    (name: "rows", repeat: (source: "InvVm.items", key: "id"), children: [
        (name: "row", classes: ["row"], children: [
            (name: "label", text: (spans: ["-"]), bindings: [(source: ".name")]),
            (name: "qty", text: (spans: ["0"]), bindings: [(source: ".count", format: "x{:03}")]),
        ]),
    ]),
]))"#;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(regia_ui::UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.init_resource::<regia_ecs::prelude::AppTypeRegistry>();
    app.register_view_model::<InvVm>();
    app.update();
    app.world_mut().resource_mut::<InvVm>().items = (0..1000)
        .map(|i| Item {
            id: i,
            name: format!("item {i}"),
            count: i,
        })
        .collect();
    app
}

/// Allocations of spawning `layout` and running the binding pass that builds the 1,000 rows.
fn rows_cost(layout: &str) -> (u64, usize) {
    let mut app = app();
    let migrators = UiMigrators::default();
    let file = parse_ui_ron(layout, "ui/list.ui.ron", &migrators).expect("layout");
    let mut load = text_loader(
        |p: &str| {
            if p == "ui/row.ui.ron" {
                Ok(ROW.to_owned())
            } else {
                Err(format!("{p}: not found"))
            }
        },
        &migrators,
    );
    let diags = app
        .world_mut()
        .resource_mut::<UiLibrary>()
        .insert_with("list", "ui/list.ui.ron", file, &mut load)
        .to_vec();
    assert!(diags.is_empty(), "{diags:?}");
    let world = app.world_mut();
    let before = ALLOCS.load(Ordering::Relaxed);
    spawn_ui(world, "list").expect("spawn");
    regia_ui::binding::update_bindings(world);
    let cost = ALLOCS.load(Ordering::Relaxed) - before;
    let mut q = world.query::<&NodeName>();
    let labels = q.iter(world).filter(|n| n.0 == "label").count();
    (cost, labels)
}

#[test]
fn a_component_row_in_a_thousand_item_repeat_costs_no_more_than_inline() {
    // Warm both paths once (type registrations, query caches), then measure.
    rows_cost(INLINE);
    rows_cost(WITH_COMPONENT);
    let (inline, inline_rows) = rows_cost(INLINE);
    let (component, component_rows) = rows_cost(WITH_COMPONENT);
    assert_eq!(inline_rows, 1000);
    assert_eq!(component_rows, 1000);
    assert!(
        component <= inline,
        "component rows allocated {component}, inline rows {inline}"
    );
}
