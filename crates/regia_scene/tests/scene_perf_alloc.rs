// SPDX-License-Identifier: MPL-2.0
//! Allocation bounds for the scene frame systems the perf audit measured (propagate's callers
//! are counted in `propagate.rs`; this binary owns the two checks that need a counting allocator).
//!
//! The counter is the same shape as `tests/perf_micro_level.rs` (ADR-2704): one add per
//! allocation, and the asserts are on byte counts, never on wall time.

#![allow(unsafe_code)] // A `GlobalAlloc` impl is unsafe by definition (ADR-2704).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_ecs::reflect::AppTypeRegistry;
use regia_scene::layers::apply_layer_switches;
use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};
use regia_scene::prefab::{PendingPrefab, PrefabLibrary, PrefabTemplate, resolve_pending_prefabs};
use regia_scene::prelude::*;

struct Counting;

thread_local! {
    static BYTES: Cell<u64> = const { Cell::new(0) };
}

fn note(bytes: usize) {
    let _ = BYTES.try_with(|cell| cell.set(cell.get() + bytes as u64));
}

// SAFETY: forwards to the system allocator and only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: same contract as the caller.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: same contract as the caller.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size.saturating_sub(layout.size()));
        // SAFETY: same contract as the caller.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn bytes() -> u64 {
    BYTES.get()
}

fn entity_count(world: &mut World) -> usize {
    let mut query = world.query::<Entity>();
    query.iter(world).count()
}

#[test]
fn resolve_does_not_copy_records_while_the_template_is_missing() {
    let mut world = World::new();
    world.init_resource::<PrefabLibrary>();
    let record = "x".repeat(4096);
    let asset = Guid::from_u128(0x000a_55e7);
    let mut ids = Vec::with_capacity(100);
    for index in 0u32..100 {
        ids.push(
            world
                .spawn((
                    PendingPrefab {
                        prefab: asset.as_u128(),
                        root_transform: None,
                        level: 0,
                        persistent: false,
                        record: record.clone(),
                    },
                    // Instantiation derives child guids from the root and refuses a root without one.
                    EntityGuid(Guid::from_u128(0xE000 + u128::from(index))),
                ))
                .id(),
        );
    }
    resolve_pending_prefabs(&mut world);
    let before = bytes();
    resolve_pending_prefabs(&mut world);
    let second = bytes() - before;
    assert!(
        second < record.len() as u64,
        "second resolve allocated {second} bytes; one record is {} bytes and 100 of them are still waiting",
        record.len()
    );
    for id in &ids {
        let pending = world.get::<PendingPrefab>(*id).expect("still pending");
        assert_eq!(pending.record, record);
    }

    // The template arrives: one resolve spawns, the next finds nothing pending.
    world.insert_resource(AppTypeRegistry::default());
    let root = Guid::from_u128(0x100);
    let file = SceneFile::from_ron_str(&format!(
        r#"(
            format_version: {version},
            entities: [ (guid: "{root}", name: "Root", components: {{}}) ],
        )"#,
        version = SCENE_FORMAT_VERSION,
    ))
    .expect("prefab");
    let registry = world.resource::<AppTypeRegistry>().0.clone();
    let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
    world.resource_mut::<PrefabLibrary>().insert(template);
    let follow = world
        .spawn((
            PendingPrefab {
                prefab: asset.as_u128(),
                root_transform: None,
                level: 0,
                persistent: false,
                record: String::new(),
            },
            EntityGuid(Guid::from_u128(0xF000)),
        ))
        .id();
    let spawned = entity_count(&mut world);
    resolve_pending_prefabs(&mut world);
    assert!(
        world.get::<PendingPrefab>(follow).is_none(),
        "the new instance resolved"
    );
    for id in &ids {
        assert!(
            world.get::<PendingPrefab>(*id).is_none(),
            "a waiting instance resolves once the template is in the library"
        );
    }
    let after = entity_count(&mut world);
    assert!(after > spawned, "the template spawned entities");
    resolve_pending_prefabs(&mut world);
    assert_eq!(entity_count(&mut world), after, "a second resolve spawns nothing");
}

#[test]
fn idle_layer_switch_does_not_allocate() {
    let mut app = App::new();
    app.add_plugins(ScenePlugin);
    let id = LevelId(Guid::from_u128(1));
    let guid = Guid::from_u128(2);
    let text = format!(
        r#"SceneFile(format_version: {version}, entities: [
            (guid: "{guid}", components: {{
                "regia_scene::transform::Transform": (translation: (1.0, 2.0, 3.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
            }}),
        ])"#,
        version = SCENE_FORMAT_VERSION,
    );
    let level = Level::from_ron_str(id, &text).expect("level");
    app.world_mut().resource_mut::<LevelLibrary>().insert(level);
    open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
    // The first apply builds the active-level key. The idle applies after it must not.
    apply_layer_switches(app.world_mut());
    let world = app.world_mut();
    let entity = world
        .query::<(Entity, &Transform)>()
        .iter(world)
        .find(|(_, transform)| transform.translation.x == 1.0)
        .map(|(entity, _)| entity)
        .expect("spawned");
    let translation = world.get::<Transform>(entity).expect("transform").translation;
    let before = bytes();
    apply_layer_switches(world);
    let second = bytes() - before;
    assert_eq!(second, 0, "an idle layer switch allocated {second} bytes");
    assert_eq!(
        app.world()
            .get::<Transform>(entity)
            .expect("transform")
            .translation,
        translation
    );
}
