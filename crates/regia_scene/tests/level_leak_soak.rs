// SPDX-License-Identifier: MPL-2.0
//! Leak soak (ADR-539): 1000 level load/unload cycles keep the heap bounded.
//!
//! Its own test binary with one test, because it installs `regia_core`'s tagging allocator:
//! every heap byte the process holds is then in the `regia_core::memory` ledger, and no other
//! test allocates beside the soak. Each cycle parses a 64-entity level from RON, opens it through
//! the `LevelManager`, runs a frame, unloads it, drops its data from the `LevelLibrary` and runs
//! another frame, like a game moving between maps. The heap and the scene subsystem's share of it
//! are sampled after every unload and judged with `LeakCheck`: after 100 warm-up cycles, the peak
//! of the last 100 cycles may exceed the peak of the first 100 measured ones by at most
//! `BOUND_BYTES`. A control run then leaks 64 bytes per cycle on purpose and must fail the same
//! check, so a pass means the harness can see a leak of that size.
//!
//! Deterministic: fixed level text and ids, a fixed number of cycles, no clocks.

use std::fmt::Write as _;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::memory::{self, LeakCheck, LeakVerdict, Subsystem, TaggingAllocator};
use regia_scene::level::{self, Level, LevelId, LevelLibrary, LevelManager, LoadMode};

#[global_allocator]
static ALLOC: TaggingAllocator = TaggingAllocator::system();

/// Load/unload cycles of the soak.
const CYCLES: u64 = 1000;
/// Cycles ignored while caches, pools and container capacities settle.
const WARMUP: u64 = 100;
/// Cycles per peak window.
const WINDOW: u64 = 100;
/// Growth allowed between the first and the last window, bytes. One 16-byte allocation kept per
/// cycle is 12.8 KB over the 800 cycles between the windows, so it fails.
const BOUND_BYTES: u64 = 8 * 1024;
/// Child entities of the level's root.
const PROPS: u32 = 63;
/// Cycles of the control run (it leaks on purpose).
const CONTROL_CYCLES: u64 = 300;
/// Bytes the control run leaks per cycle: 200 cycles between its windows × 64 B = 12.8 KB.
const CONTROL_LEAK: usize = 64;

/// The soak level: a root and `PROPS` children with transforms and names.
fn level_text() -> String {
    let root = Guid::from_u128(0xF00D_0000);
    let mut text = String::from("(\n    format_version: 1,\n    entities: [\n");
    let _ = writeln!(
        text,
        "        (guid: \"{root}\", name: \"Root\", components: {{\n            \
         \"regia_scene::transform::Transform\": (),\n        }}),"
    );
    for i in 0..PROPS {
        let guid = Guid::from_u128(0xF00D_1000 + u128::from(i));
        let _ = writeln!(
            text,
            "        (guid: \"{guid}\", name: \"Prop {i}\", parent: Some(\"{root}\"), components: {{\n            \
             \"regia_scene::transform::Transform\": (translation: ({i}.0, 0.0, 2.0)),\n        }}),"
        );
    }
    text.push_str("    ],\n)\n");
    text
}

/// One load/unload cycle; returns the entity count while the level was open.
/// Spawned entities that are not resources: resources are entities in bevy_ecs 0.20, and one
/// inserted on first use is not a level leak.
fn non_resource_entities(world: &World) -> u32 {
    let resources = world.resource_entities().iter().count() as u32;
    world.entities().count_spawned() - resources
}

fn cycle(app: &mut App, text: &str, n: u64) -> u32 {
    let id = LevelId(Guid::from_u128(0x5EED_0000_0000 + u128::from(n)));
    let parsed = match Level::from_ron_str(id, text) {
        Ok(parsed) => parsed,
        Err(err) => panic!("cycle {n}: the soak level does not parse: {err}"),
    };
    app.world_mut().resource_mut::<LevelLibrary>().insert(parsed);
    if let Err(err) = level::open_level(app.world_mut(), id, LoadMode::Replace) {
        panic!("cycle {n}: open_level: {err}");
    }
    app.update();
    let open = non_resource_entities(app.world());
    if let Err(err) = level::unload_level(app.world_mut(), id) {
        panic!("cycle {n}: unload_level: {err}");
    }
    let removed = app.world_mut().resource_mut::<LevelLibrary>().remove(id);
    assert!(
        removed.is_some(),
        "cycle {n}: the level's data was in the library"
    );
    drop(removed);
    app.update();
    open
}

/// The failure text: the verdict and which subsystem grew between the two snapshots.
fn explain(
    what: &str,
    verdict: &LeakVerdict,
    before: &memory::MemorySnapshot,
    after: &memory::MemorySnapshot,
) -> String {
    let growth = after.growth_since(before);
    format!(
        "{what}: {verdict:?}; largest grower {:?}; heap now:\n{}",
        growth.largest(),
        after.table()
    )
}

#[test]
fn a_thousand_level_load_unload_cycles_keep_the_heap_bounded() {
    let text = level_text();
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.update();
    let baseline_entities = non_resource_entities(app.world());
    let level_entities = PROPS + 1;

    let mut heap = LeakCheck::new(WARMUP, WINDOW);
    let mut scene = LeakCheck::new(WARMUP, WINDOW);
    let mut after_warmup = memory::snapshot();
    for n in 0..CYCLES {
        let open = cycle(&mut app, &text, n);
        assert!(
            open >= baseline_entities + level_entities,
            "cycle {n}: the level spawned {open} entities over a baseline of {baseline_entities}"
        );
        assert_eq!(
            non_resource_entities(app.world()),
            baseline_entities,
            "cycle {n}: unloading left entities behind"
        );
        assert_eq!(app.world().resource::<LevelManager>().current(), None);
        let snap = memory::snapshot();
        heap.sample(snap.live_bytes);
        scene.sample(snap.subsystem(Subsystem::Scene).live_bytes);
        if n + 1 == WARMUP {
            after_warmup = snap;
        }
    }
    let end = memory::snapshot();
    assert!(
        end.tracking,
        "the tagging allocator is this binary's global allocator"
    );
    assert!(
        end.subsystem(Subsystem::Scene).total_allocations > 0,
        "level spawn and unload allocate inside the scene scope"
    );

    let heap_verdict = heap.verdict(BOUND_BYTES).expect("1000 cycles fill ten windows");
    assert_eq!(heap_verdict.cycles, CYCLES);
    assert!(
        heap_verdict.bounded,
        "{}",
        explain("heap grew over the soak", &heap_verdict, &after_warmup, &end)
    );
    let scene_verdict = scene.verdict(BOUND_BYTES).expect("1000 cycles fill ten windows");
    assert!(
        scene_verdict.bounded,
        "{}",
        explain(
            "scene memory grew over the soak",
            &scene_verdict,
            &after_warmup,
            &end
        )
    );

    // Control: the same cycles plus a deliberate 64-byte leak each must fail the same check.
    let mut control = LeakCheck::new(WINDOW / 2, WINDOW / 2);
    for n in 0..CONTROL_CYCLES {
        cycle(&mut app, &text, CYCLES + n);
        let leaked: &'static mut [u8] = memory::scoped(Subsystem::Gameplay, || {
            Box::leak(vec![0u8; CONTROL_LEAK].into_boxed_slice())
        });
        assert_eq!(leaked.len(), CONTROL_LEAK);
        control.sample(memory::snapshot().live_bytes);
    }
    let control_verdict = control
        .verdict(BOUND_BYTES)
        .expect("300 cycles fill five windows");
    assert!(
        !control_verdict.bounded,
        "a 64-byte-per-cycle leak must exceed the bound: {control_verdict:?}"
    );
    let leaked = memory::snapshot().subsystem(Subsystem::Gameplay);
    assert!(
        leaked.live_bytes >= CONTROL_CYCLES * CONTROL_LEAK as u64,
        "the control leak is charged to its scope: {leaked:?}"
    );
}
