// SPDX-License-Identifier: MPL-2.0
//! PIE start's work, counted (M11 acceptance "PIE start < 1 s for `samples/sim_bridge`";
//! ADR-7703). The check used to be `start_elapsed_ms < 1000` in `pie.rs`'s unit tests, a wall-clock
//! bar that failed on a loaded build host and passed alone. Start is fast when it does a fixed
//! amount of work plus a constant amount per copied entity: one snapshot of the edit world, one
//! spawn of the copy, the play app's plugins and one settled frame. This binary counts that work
//! and fails when start takes a slow path:
//!
//! - **frames**: start runs exactly one frame (no settle loop) and copies exactly the level's
//!   entities, once;
//! - **allocations per entity**: the extra allocations of a 256-entity start over a 64-entity one,
//!   per extra entity, stay under [`PER_ENTITY_MAX`] (a copy that re-snapshots, re-parses or
//!   searches per entity grows with the level and exceeds it);
//! - **fixed allocations**: what start allocates with the per-entity share taken out stays under
//!   [`FIXED_MAX`] (a start that loads, scans or bakes something more than the play app's plugins
//!   exceeds it).
//!
//! The wall time is printed for information only. Process-wide counts (the play app's systems run
//! on the compute pool), so the binary holds one test and takes [`alloc_counter::exclusive`].

use regia_app::Diagnostics;
use regia_app::prelude::*;
use regia_editor::pie::{PieSession, start_play};
use regia_scene::level::{Level, LevelLibrary, LoadMode, open_level};
use regia_scene::prelude::{EntityGuid, LevelId, LevelMember, Name, Transform};
use regia_test_utils::alloc_counter::{self, AllocStats, CountingAlloc};

// The shared counting allocator (ADR-3291).
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

/// The sample-sized level: `samples/sim_bridge` holds a handful of named, transformed entities;
/// 64 of them bound it generously (the old test's size).
const SAMPLE: usize = 64;
/// The larger level the per-entity share is measured against.
const LARGE: usize = 256;
/// Allocations per extra copied entity (snapshot, spawn, level record), with headroom: 76 measured
/// (the build server, 3 runs of 3 the same, ADR-7703).
const PER_ENTITY_MAX: u64 = 120;
/// Allocations of a start with the per-entity share taken out (the play app, its plugins and one
/// frame), with headroom: 7422 measured (the build server, 3 runs of 3 the same, ADR-7703).
const FIXED_MAX: u64 = 15_000;

/// An edit app with an open level holding `n` named, transformed members (the old test's level).
fn edit_app(n: usize) -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let id = LevelId(regia_core::id::Guid::new_random());
    app.world_mut()
        .resource_mut::<LevelLibrary>()
        .insert(Level::from_ron_str(id, r#"( format_version: 1, entities: [] )"#).expect("parse"));
    open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
    for i in 0..n {
        app.world_mut().spawn((
            Name::new(format!("Entity{i}")),
            Transform::default(),
            LevelMember::from_level(id),
            EntityGuid(regia_core::id::Guid::new_random()),
        ));
    }
    app
}

/// Starts play on an `n`-entity level and returns the session with the allocations of the
/// `start_play` call alone (every thread: the first frame runs systems on the compute pool).
fn start(n: usize) -> (PieSession, AllocStats) {
    let mut app = edit_app(n);
    let before = alloc_counter::process_stats();
    let session = start_play(app.world_mut(), None).expect("play");
    let counted = alloc_counter::process_stats().since(before);
    // The edit app and the session drop after the window.
    drop(app);
    (session, counted)
}

/// Frames the play app ran (its diagnostics count every update, the first included).
fn frames_run(session: &PieSession) -> u64 {
    session.app.world().resource::<Diagnostics>().frames
}

#[test]
fn pie_start_is_fast_for_a_sample_sized_level() {
    let _alone = alloc_counter::exclusive();
    alloc_counter::quiet_tracing();
    // A first start warms what a process builds once (the compute pool's threads, type
    // registrations, interned labels): an editor has done that long before its first play.
    drop(start(SAMPLE));

    let (sample, at_sample) = start(SAMPLE);
    let (large, at_large) = start(LARGE);
    eprintln!(
        "PIE start (information only, not a bar): {} ms for {SAMPLE} entities, {} ms for {LARGE}; \
         allocations {} and {}",
        sample.start_elapsed_ms, large.start_elapsed_ms, at_sample.allocs, at_large.allocs
    );

    // One snapshot copy of exactly the level's entities and one settled frame.
    assert_eq!(sample.entities, SAMPLE, "start copies the level's entities once");
    assert_eq!(large.entities, LARGE, "start copies the level's entities once");
    assert_eq!(frames_run(&sample), 1, "start runs one settled frame");
    assert_eq!(frames_run(&large), 1, "start runs one settled frame");
    assert_eq!(sample.frames, 1);

    // The per-entity share and the fixed share of the work.
    let extra = (LARGE - SAMPLE) as u64;
    let per_entity = at_large.allocs.saturating_sub(at_sample.allocs) / extra;
    let fixed = at_sample.allocs.saturating_sub(per_entity * SAMPLE as u64);
    eprintln!("PIE start work: {per_entity} allocations per entity, {fixed} fixed");
    assert!(
        per_entity <= PER_ENTITY_MAX,
        "PIE start does {per_entity} allocations per copied entity (bar {PER_ENTITY_MAX}): \
         {} for {SAMPLE} entities, {} for {LARGE}",
        at_sample.allocs,
        at_large.allocs
    );
    assert!(
        fixed <= FIXED_MAX,
        "PIE start does {fixed} allocations besides its entities (bar {FIXED_MAX}): {} for \
         {SAMPLE} entities",
        at_sample.allocs
    );
}
