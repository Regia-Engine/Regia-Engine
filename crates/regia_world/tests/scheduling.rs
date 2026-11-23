// SPDX-License-Identifier: MPL-2.0
//! Spec 78 § 18 tests 3 (states), 4 (budgets), 5 (cancellation), 6 (failure), 10 (determinism), 13
//! (suspend), 14 (empty cells) and 15 (planner mapping), headless with the memory source, in lockstep
//! unless a test says otherwise. Counters and fixed steps only.

mod common;

use common::*;
use regia_asset::AssetId;
use regia_core::math::Vec3;
use regia_ecs::prelude::Messages;
use regia_scene::Transform;
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::{CellFailed, CellLoaded, CellUnloading, Readiness, StreamBudgets, StreamGates};
use regia_world::{
    CellReadError, CellState, PinWait, StreamTiming, StreamTrace, TraceEvent, TraceKind, WorldStreaming,
};

fn c(x: i64, z: i64) -> CellCoord {
    CellCoord::new(x, z)
}

fn cell_loaded_messages(app: &regia_app::App) -> usize {
    let messages = app.world().resource::<Messages<CellLoaded>>();
    messages.get_cursor().read(messages).count()
}

fn streaming(app: &mut regia_app::App) -> regia_ecs::prelude::Mut<'_, WorldStreaming> {
    app.world_mut().resource_mut::<WorldStreaming>()
}

#[test]
fn a_cell_walks_every_state_on_its_way_in_and_out() {
    let mut app = app();
    let world = build(&app, 6, 1, 3, 1, settings(40.0, 60.0));
    open(&mut app, &world, 2);
    let source = spawn_source(&mut app, 50.0, 50.0);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Unloaded));

    let mut seen = vec![CellState::Unloaded];
    for _ in 0..12 {
        app.update();
        let now = state(&app, c(0, 0)).expect("cell");
        if seen.last() != Some(&now) {
            seen.push(now);
        }
    }
    assert_eq!(seen.last(), Some(&CellState::Visible), "{seen:?}");
    assert!(seen.contains(&CellState::Loading), "{seen:?}");

    // Walk away: the cell goes `Unloading`, then `Unloaded`.
    move_to(&mut app, source, 550.0, 50.0);
    let mut out = Vec::new();
    for _ in 0..12 {
        app.update();
        let now = state(&app, c(0, 0)).expect("cell");
        if out.last() != Some(&now) {
            out.push(now);
        }
    }
    assert_eq!(out.last(), Some(&CellState::Unloaded), "{out:?}");
    assert!(out.contains(&CellState::Unloading), "{out:?}");
    assert_eq!(state(&app, c(5, 0)), Some(CellState::Visible));
}

#[test]
fn applying_spans_runs_when_the_cell_exceeds_the_entity_budget() {
    let mut app = app();
    let mut s = settings(40.0, 60.0);
    s.budgets = Some(StreamBudgets {
        apply_budget_entities: 4,
        ..s.effective_budgets()
    });
    let world = build(&app, 1, 1, 10, 1, s);
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    let mut applying_runs = 0;
    for _ in 0..20 {
        app.update();
        if state(&app, c(0, 0)) == Some(CellState::Applying) {
            applying_runs += 1;
            assert!(stats(&app).last_apply_entities <= 4);
        }
    }
    assert_eq!(
        applying_runs, 2,
        "10 entities at 4 per run stay `Applying` over two runs"
    );
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Visible));
    assert_eq!(count_members(&mut app), 10);
}

#[test]
fn a_walk_holds_every_budget() {
    let mut app = app();
    let mut s = settings(150.0, 250.0);
    let probe = build(&app, 40, 3, 6, 2, s.clone());
    let own = probe.index.cells[0].own_bytes;
    s.budgets = Some(StreamBudgets {
        max_loads_per_step: 3,
        max_inflight: 2,
        max_pending_bytes: own * 3,
        memory_budget_bytes: own * 15,
        apply_budget_entities: 8,
        ..s.effective_budgets()
    });
    let world = build(&app, 40, 3, 6, 2, s);
    open(&mut app, &world, 2);
    let source = spawn_source(&mut app, 50.0, 150.0);

    let mut applied_any = false;
    for step in 0..2000 {
        let x = 50.0 + step as f32 * 1.8;
        move_to(&mut app, source, x, 150.0);
        app.update();
        let now = stats(&app);
        assert!(now.resident_bytes_estimate <= own * 15, "step {step}: {now:?}");
        assert!(now.inflight <= 2, "step {step}: {now:?}");
        assert!(now.pending_bytes <= own * 3, "step {step}: {now:?}");
        assert!(now.last_apply_entities <= 8, "step {step}: {now:?}");
        applied_any |= now.last_apply_entities > 0;
    }
    assert!(applied_any);
    let end = stats(&app);
    assert_eq!(end.apply_over_budget, 0);
    assert!(end.loads_issued > 20, "{end:?}");
    // The end of the road is loaded, the start is not.
    assert_eq!(state(&app, c(0, 1)), Some(CellState::Unloaded));
    assert_eq!(
        state(&app, c(36, 1)).map(|s| s.is_spawned()),
        Some(true),
        "{:?}",
        state(&app, c(36, 1))
    );
}

#[test]
fn a_hierarchy_over_the_budget_spawns_alone_and_is_counted() {
    let mut app = app();
    let mut s = settings(40.0, 60.0);
    s.budgets = Some(StreamBudgets {
        apply_budget_entities: 3,
        ..s.effective_budgets()
    });
    let world = build(&app, 1, 1, 2, 5, s);
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    let mut most = 0;
    for _ in 0..10 {
        app.update();
        most = most.max(stats(&app).last_apply_entities);
    }
    assert_eq!(most, 5, "a whole hierarchy, never half of one");
    assert_eq!(stats(&app).apply_over_budget, 2);
    assert_eq!(count_members(&mut app), 10);
}

#[test]
fn a_cell_cancelled_while_queued_never_reads() {
    let mut app = app();
    let mut s = settings(40.0, 60.0);
    s.budgets = Some(StreamBudgets {
        max_inflight: 1,
        ..s.effective_budgets()
    });
    // Cells 0 and 1 are both wanted; the single read slot is held by a read that never finishes.
    let world = build(&app, 2, 1, 1, 1, s);
    world.source.hold(cell_id(c(0, 0)));
    world.source.hold(cell_id(c(1, 0)));
    open(&mut app, &world, 1000);
    let source = spawn_source(&mut app, 100.0, 50.0);
    steps(&mut app, 2);
    let queued = [c(0, 0), c(1, 0)]
        .into_iter()
        .find(|cell| state(&app, *cell) == Some(CellState::Queued))
        .expect("one cell queued behind the other");
    move_to(&mut app, source, 5000.0, 50.0);
    steps(&mut app, 1);
    assert_eq!(state(&app, queued), Some(CellState::Unloaded));
    assert_eq!(stats(&app).loads_cancelled, 2);
    world.source.release(cell_id(queued));
    assert_eq!(world.source.reads(), 0);
}

#[test]
fn a_cell_cancelled_while_loading_never_appears_and_the_result_is_counted_stale() {
    let mut app = app();
    let world = build(&app, 1, 1, 3, 1, settings(40.0, 60.0));
    world.source.hold(cell_id(c(0, 0)));
    open(&mut app, &world, 2);
    let source = spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 1);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Loading));
    move_to(&mut app, source, 5000.0, 50.0);
    steps(&mut app, 1);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Unloaded), "at once");
    world.source.release(cell_id(c(0, 0)));
    steps(&mut app, 5);
    assert_eq!(stats(&app).stale_discards, 1);
    assert_eq!(world.source.reads(), 0, "the token stopped the read");
    assert_eq!(world.source.cancelled_before_read(), 1);
    assert_eq!(count_members(&mut app), 0);
    assert_eq!(stats(&app).inflight, 0);
    assert_eq!(stats(&app).pending_bytes, 0);
}

#[test]
fn a_cell_cancelled_while_applying_leaves_no_entity() {
    let mut app = app();
    let mut s = settings(40.0, 60.0);
    s.budgets = Some(StreamBudgets {
        apply_budget_entities: 2,
        ..s.effective_budgets()
    });
    let world = build(&app, 1, 1, 10, 1, s);
    open(&mut app, &world, 1);
    let source = spawn_source(&mut app, 50.0, 50.0);
    while state(&app, c(0, 0)) != Some(CellState::Applying) {
        app.update();
        assert!(stats(&app).step < 20);
    }
    assert!(count_members(&mut app) > 0);
    move_to(&mut app, source, 5000.0, 50.0);
    steps(&mut app, 1);
    // What spawned goes on the same budget, here within the step.
    assert_ne!(state(&app, c(0, 0)), Some(CellState::Applying));
    assert_eq!(cell_loaded_messages(&app), 0);
    steps(&mut app, 8);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Unloaded));
    assert_eq!(count_members(&mut app), 0);
    assert_eq!(stats(&app).loads_cancelled, 1);
    assert_eq!(stats(&app).pending_bytes, 0);
}

#[test]
fn a_teleport_cancels_far_work_at_once_and_loads_the_target_first() {
    let mut app = app();
    let world = build(&app, 40, 1, 1, 1, settings(40.0, 60.0));
    for x in 0..2 {
        world.source.hold(cell_id(c(x, 0)));
    }
    open(&mut app, &world, 1000);
    let source = spawn_source(&mut app, 100.0, 50.0);
    steps(&mut app, 1);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Loading));
    move_to(&mut app, source, 3050.0, 50.0);
    steps(&mut app, 1);
    let now = stats(&app);
    assert_eq!(now.teleports, 1);
    assert_eq!(now.loads_cancelled, 2);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Unloaded));
    assert_eq!(state(&app, c(30, 0)), Some(CellState::Loading));
}

#[test]
fn a_missing_blob_fails_keeps_others_going_and_retries_three_times() {
    let mut app = app();
    let world = build(&app, 3, 1, 1, 1, settings(160.0, 200.0));
    let bad = cell_id(c(1, 0));
    world.source.remove(bad);
    open(&mut app, &world, 1);
    spawn_source(&mut app, 150.0, 50.0);
    let mut reasons = Vec::new();
    let mut cursor = app.world().resource::<Messages<CellFailed>>().get_cursor();
    for _ in 0..5 {
        app.update();
        let messages = app.world().resource::<Messages<CellFailed>>();
        reasons.extend(cursor.read(messages).map(|m| m.reason.clone()));
    }
    assert_eq!(reasons, vec!["cell_missing".to_string()]);
    assert_eq!(
        state(&app, c(1, 0)),
        Some(CellState::Failed("cell_missing".into()))
    );
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Visible));
    assert_eq!(state(&app, c(2, 0)), Some(CellState::Visible));
    assert_eq!(stats(&app).failures, 1);
    // Held: nothing new for `retry_after_steps`, then three retries, then it stays failed.
    steps(&mut app, 5);
    assert_eq!(stats(&app).failures, 1);
    steps(&mut app, 60);
    assert_eq!(stats(&app).failures, 4, "the first failure and three retries");
    assert_eq!(
        state(&app, c(1, 0)),
        Some(CellState::Failed("cell_missing".into()))
    );
    steps(&mut app, 60);
    assert_eq!(stats(&app).failures, 4);

    // An explicit retry once the blob is back.
    world.source.insert(bad, blob_bytes(&app, c(1, 0), 1, 1));
    {
        let mut ws = streaming(&mut app);
        ws.retry(level(), c(1, 0)).expect("retry");
    }
    steps(&mut app, 5);
    assert_eq!(state(&app, c(1, 0)), Some(CellState::Visible));
}

/// The blob file `build` makes for a cell.
fn blob_bytes(app: &regia_app::App, cell: CellCoord, per_cell: usize, size: usize) -> Vec<u8> {
    let blob = regia_world::format::CellBlob {
        format_version: regia_world::format::CELL_BLOB_FORMAT_VERSION,
        coord: cell,
        scene: cook_cell(app, cell, per_cell, size),
        anchor: [0, 0, 0],
        nav: None,
    };
    blob.to_bytes(cell_id(cell), Vec::new()).expect("encode")
}

#[test]
fn a_corrupt_or_newer_blob_gets_its_reason() {
    let mut app = app();
    let world = build(&app, 3, 1, 1, 1, settings(250.0, 300.0));
    // Same length and a stale record hash: corrupt.
    let mut flipped = blob_bytes(&app, c(0, 0), 1, 1);
    let last = flipped.len() - 1;
    flipped[last] ^= 0xff;
    world.source.insert(cell_id(c(0, 0)), flipped);
    // Unreadable with a read error.
    world
        .source
        .set_error(cell_id(c(1, 0)), Some(CellReadError::Unavailable("arena".into())));
    // A newer format whose hash the record does know.
    let mut world = world;
    let newer = regia_world::format::CellBlob {
        format_version: regia_world::format::CELL_BLOB_FORMAT_VERSION + 1,
        coord: c(2, 0),
        scene: cook_cell(&app, c(2, 0), 1, 1),
        anchor: [0, 0, 0],
        nav: None,
    };
    let bytes = newer.to_bytes(cell_id(c(2, 0)), Vec::new()).expect("encode");
    world.index.cells[2].blob_hash = *blake3::hash(&bytes).as_bytes();
    world.source.insert(cell_id(c(2, 0)), bytes);
    open(&mut app, &world, 1);
    spawn_source(&mut app, 150.0, 50.0);
    steps(&mut app, 6);
    assert_eq!(
        state(&app, c(0, 0)),
        Some(CellState::Failed("cell_corrupt".into()))
    );
    assert_eq!(
        state(&app, c(1, 0)),
        Some(CellState::Failed("cell_unavailable".into()))
    );
    assert_eq!(
        state(&app, c(2, 0)),
        Some(CellState::Failed("cell_format_newer".into()))
    );
    assert_eq!(count_members(&mut app), 0);
}

#[test]
fn a_failed_cell_nobody_wants_goes_unloaded_and_starts_fresh() {
    let mut app = app();
    let world = build(&app, 1, 1, 1, 1, settings(40.0, 60.0));
    let id = cell_id(c(0, 0));
    world.source.set_error(id, Some(CellReadError::Io("disk".into())));
    open(&mut app, &world, 1);
    let source = spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 4);
    assert!(matches!(state(&app, c(0, 0)), Some(CellState::Failed(_))));
    move_to(&mut app, source, 5000.0, 50.0);
    steps(&mut app, 2);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Unloaded));
    world.source.set_error(id, None);
    move_to(&mut app, source, 50.0, 50.0);
    steps(&mut app, 6);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Visible));
}

#[test]
fn a_queued_cell_is_not_offered_again_and_a_freed_budget_loads_a_deferred_cell() {
    let mut app = app();
    let mut s = settings(250.0, 300.0);
    let probe = build(&app, 3, 1, 1, 1, s.clone());
    let own = probe.index.cells[0].own_bytes;
    // Room for two cells of three.
    s.budgets = Some(StreamBudgets {
        memory_budget_bytes: own * 2,
        ..s.effective_budgets()
    });
    let world = build(&app, 3, 1, 1, 1, s);
    open(&mut app, &world, 3);
    let source = spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 1);
    let issued = stats(&app).loads_issued;
    assert_eq!(issued, 2);
    // Plans run on a moved source while the reads are in flight; nothing is queued twice.
    move_to(&mut app, source, 80.0, 50.0);
    steps(&mut app, 1);
    assert_eq!(stats(&app).loads_issued, issued);
    steps(&mut app, 8);
    assert_eq!(
        state(&app, c(2, 0)),
        Some(CellState::Unloaded),
        "over the memory budget"
    );
    // Lower the need: the last cell comes once a nearer one is gone.
    let budgets = StreamBudgets {
        memory_budget_bytes: own * 3,
        ..app
            .world()
            .resource::<WorldStreaming>()
            .budgets(level())
            .copied()
            .expect("level")
    };
    streaming(&mut app)
        .set_budgets(level(), budgets)
        .expect("budgets");
    steps(&mut app, 6);
    assert_eq!(state(&app, c(2, 0)), Some(CellState::Visible));
}

#[test]
fn an_unload_that_frees_memory_triggers_the_plan_that_loads_a_deferred_cell() {
    let mut app = app();
    let mut s = settings(150.0, 200.0);
    let probe = build(&app, 6, 1, 1, 1, s.clone());
    let own = probe.index.cells[0].own_bytes;
    s.budgets = Some(StreamBudgets {
        memory_budget_bytes: own * 4,
        ..s.effective_budgets()
    });
    s.tuning.plan_interval_steps = 100_000;
    let world = build(&app, 6, 1, 1, 1, s);
    open(&mut app, &world, 1);
    let source = spawn_source(&mut app, 250.0, 50.0);
    steps(&mut app, 8);
    assert_eq!(stats(&app).resident_cells, 4);
    // A jump: the old cells hold their memory while they drain, then the rest load.
    move_to(&mut app, source, 550.0, 50.0);
    steps(&mut app, 1);
    let plans = stats(&app).plans;
    steps(&mut app, 8);
    assert!(stats(&app).plans > plans, "the finished unload planned again");
    assert_eq!(state(&app, c(5, 0)), Some(CellState::Visible));
    assert_eq!(state(&app, c(4, 0)), Some(CellState::Visible));
}

#[test]
fn cell_unloading_goes_out_one_run_before_the_entities_do() {
    let mut app = app();
    let world = build(&app, 1, 1, 2, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    let source = spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 5);
    assert_eq!(count_members(&mut app), 2);
    move_to(&mut app, source, 5000.0, 50.0);
    steps(&mut app, 1);
    let unloading: Vec<_> = {
        let messages = app.world().resource::<Messages<CellUnloading>>();
        let mut cursor = messages.get_cursor();
        cursor.read(messages).copied().collect()
    };
    assert_eq!(unloading.len(), 1);
    assert_eq!(
        count_members(&mut app),
        2,
        "members are still readable on `CellUnloading`"
    );
    steps(&mut app, 1);
    assert_eq!(count_members(&mut app), 0);
}

#[test]
fn cell_loaded_lists_every_entity_of_the_cell_with_its_root_where_cooked() {
    let mut app = app();
    let world = build(&app, 1, 1, 2, 2, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 3);
    let loaded: Vec<CellLoaded> = {
        let messages = app.world().resource::<Messages<CellLoaded>>();
        let mut cursor = messages.get_cursor();
        cursor.read(messages).cloned().collect()
    };
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].entities.len(), 4);
    let mut roots = 0;
    for e in &loaded[0].entities {
        if app.world().get::<regia_ecs::prelude::ChildOf>(*e).is_none() {
            let t = app.world().get::<Transform>(*e).expect("transform");
            assert!(t.translation.x == 0.0 || t.translation.x == 0.5, "{t:?}");
            roots += 1;
        }
    }
    assert_eq!(roots, 2);
}

#[test]
fn pins_pull_cells_and_ttls_let_them_go() {
    let mut app = app();
    let world = build(&app, 40, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    let pin = streaming(&mut app)
        .pin("look", [3050.0, 0.0, 50.0], 1.0, 0, Some(6))
        .expect("pin");
    steps(&mut app, 5);
    assert_eq!(state(&app, c(30, 0)), Some(CellState::Visible));
    steps(&mut app, 8);
    assert_eq!(state(&app, c(30, 0)), Some(CellState::Unloaded));
    assert!(streaming(&mut app).pin_info(pin).is_none());
}

#[test]
fn a_teleport_pin_counts_as_a_pending_wait_until_the_target_is_ready() {
    let mut app = app();
    let mut s = settings(40.0, 60.0);
    s.tuning.wait_on_open = false;
    let world = build(&app, 40, 1, 1, 1, s);
    open(&mut app, &world, 2);
    let pin = streaming(&mut app)
        .prepare_teleport([3050.0, 0.0, 50.0])
        .expect("pin");
    steps(&mut app, 1);
    assert_eq!(
        app.world().resource::<regia_world::StreamStats>().pending_waits,
        1
    );
    let center = [3050.0, 0.0, 50.0];
    let ready = |app: &regia_app::App, r: Readiness| {
        let gates = app.world().resource::<StreamGates>();
        app.world()
            .resource::<WorldStreaming>()
            .ready_near(gates, center, 40.0, r)
    };
    assert!(!ready(&app, Readiness::SimReady));
    steps(&mut app, 6);
    assert!(ready(&app, Readiness::SimReady));
    assert!(ready(&app, Readiness::Visible));
    assert_eq!(
        app.world().resource::<regia_world::StreamStats>().pending_waits,
        0
    );
    assert!(streaming(&mut app).unpin(pin));
}

#[test]
fn leaving_lockstep_lands_the_reads_it_held_back() {
    let mut app = app();
    let world = build(&app, 1, 1, 2, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1000);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 2);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Loading));
    streaming(&mut app).set_timing(StreamTiming::Async);
    for _ in 0..100_000 {
        app.update();
        if state(&app, c(0, 0)) == Some(CellState::Visible) {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Visible));
    // Back to lockstep: nothing left over waits for a read that will never come.
    streaming(&mut app).set_timing(StreamTiming::Lockstep { latency_steps: 1 });
    steps(&mut app, 3);
    assert_eq!(stats(&app).lockstep_stalls, 0);
}

#[test]
fn a_rate_limited_cell_loads_on_the_next_step_with_no_other_trigger() {
    let mut app = app();
    let mut s = settings(250.0, 300.0);
    s.budgets = Some(StreamBudgets {
        max_loads_per_step: 1,
        ..s.effective_budgets()
    });
    s.tuning.plan_interval_steps = 100_000;
    let world = build(&app, 3, 1, 1, 1, s);
    open(&mut app, &world, 1000);
    spawn_source(&mut app, 150.0, 50.0);
    steps(&mut app, 3);
    assert_eq!(
        stats(&app).loads_issued,
        3,
        "one load per plan, a plan per step while any wait"
    );
}

#[test]
fn measured_memory_over_the_budget_lowers_the_planner_budget_in_async_timing() {
    let mut app = app();
    let mut s = settings(250.0, 300.0);
    let probe = build(&app, 4, 1, 1, 1, s.clone());
    let own = probe.index.cells[0].own_bytes;
    s.budgets = Some(StreamBudgets {
        memory_budget_bytes: own * 2,
        ..s.effective_budgets()
    });
    let mut world = build(&app, 4, 1, 1, 1, s);
    // The cook underestimates: every record says half the real size.
    for record in &mut world.index.cells {
        record.own_bytes /= 2;
    }
    open(&mut app, &world, 2);
    streaming(&mut app).set_timing(StreamTiming::Async);
    spawn_source(&mut app, 50.0, 50.0);
    for _ in 0..2000 {
        app.update();
        std::thread::yield_now();
        if stats(&app).resident_cells == 4 {
            break;
        }
    }
    assert_eq!(stats(&app).resident_cells, 4, "the estimates say all four fit");
    steps(&mut app, 200);
    assert!(
        stats(&app).resident_cells < 4,
        "the measured bytes shed a cell: {:?}",
        stats(&app)
    );
}

#[test]
fn level_open_counts_as_a_wait_until_the_ground_around_the_start_sources_is_ready() {
    let mut app = app();
    let world = build(&app, 4, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 2);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 1);
    assert_eq!(
        app.world().resource::<regia_world::StreamStats>().pending_waits,
        1
    );
    steps(&mut app, 6);
    assert_eq!(
        app.world().resource::<regia_world::StreamStats>().pending_waits,
        0
    );
}

#[test]
fn a_suspended_app_stops_reads_through_its_clock() {
    let mut app = app();
    let world = build(&app, 3, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    app.world_mut()
        .resource_mut::<regia_app::Time>()
        .set_suspended(true);
    steps(&mut app, 3);
    assert!(app.world().resource::<WorldStreaming>().is_suspended());
    app.world_mut()
        .resource_mut::<regia_app::Time>()
        .set_suspended(false);
    steps(&mut app, 3);
    assert!(!app.world().resource::<WorldStreaming>().is_suspended());
}

#[test]
fn a_low_memory_signal_trims_until_the_next_resume() {
    use regia_platform::{AppLifecycle, LifecycleEvent, apply_transition};
    let mut app = app();
    app.init_resource::<AppLifecycle>();
    let mut s = settings(250.0, 300.0);
    let probe = build(&app, 5, 1, 1, 1, s.clone());
    let own = probe.index.cells[0].own_bytes;
    s.budgets = Some(StreamBudgets {
        memory_budget_bytes: own * 4,
        ..s.effective_budgets()
    });
    let world = build(&app, 5, 1, 1, 1, s);
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 8);
    assert_eq!(stats(&app).resident_cells, 4);
    apply_transition(&mut app, LifecycleEvent::LowMemory);
    steps(&mut app, 8);
    assert_eq!(stats(&app).resident_cells, 2);
    apply_transition(&mut app, LifecycleEvent::Resumed);
    steps(&mut app, 8);
    assert_eq!(stats(&app).resident_cells, 4);
}

#[test]
fn the_source_limit_is_an_error_with_its_code() {
    let mut app = app();
    let world = build(&app, 1, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    let mut ws = streaming(&mut app);
    for i in 0..regia_world::runtime::MAX_STREAM_SOURCES {
        ws.pin(&format!("p{i}"), [0.0; 3], 1.0, 0, None).expect("pin");
    }
    let error = ws.pin("over", [0.0; 3], 1.0, 0, None).expect_err("limit");
    assert_eq!(error.reason(), "stream_source_limit");
    assert_eq!(
        ws.set_pin_wait(
            regia_world::PinId(u64::MAX),
            Some(PinWait {
                radius_m: 1.0,
                readiness: Readiness::SimReady
            })
        )
        .expect_err("unknown")
        .reason(),
        "stream_unknown_pin"
    );
    assert_eq!(
        ws.retry(level(), c(9, 9)).expect_err("unknown").reason(),
        "stream_unknown_cell"
    );
    let bad = StreamBudgets {
        max_inflight: 0,
        ..StreamBudgets::default()
    };
    assert_eq!(
        ws.set_budgets(level(), bad).expect_err("bad").reason(),
        "stream_bad_budget"
    );
}

#[test]
fn an_empty_region_is_ready_at_once_and_outside_the_bounds_too() {
    let mut app = app();
    let world = build_with(&app, 4, 4, settings(40.0, 60.0), |cell| {
        // Only one corner has content.
        if cell == c(0, 0) { (1, 1) } else { (0, 0) }
    });
    open(&mut app, &world, 1);
    let gates = StreamGates::default();
    let ws = app.world().resource::<WorldStreaming>();
    assert!(ws.ready_near(&gates, [350.0, 0.0, 350.0], 90.0, Readiness::Visible));
    assert!(ws.ready_near(&gates, [5000.0, 0.0, 5000.0], 90.0, Readiness::Visible));
    assert!(!ws.ready_near(&gates, [50.0, 0.0, 50.0], 10.0, Readiness::SimReady));
    assert_eq!(ws.state(level(), c(3, 3)), None);
}

#[test]
fn a_suspended_runtime_issues_no_reads_and_lets_reads_in_flight_finish() {
    let mut app = app();
    let world = build(&app, 20, 1, 1, 1, settings(40.0, 60.0));
    world.source.hold(cell_id(c(0, 0)));
    open(&mut app, &world, 2);
    let source = spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 1);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Loading));
    streaming(&mut app).set_suspended(true);
    world.source.release(cell_id(c(0, 0)));
    move_to(&mut app, source, 150.0, 50.0);
    steps(&mut app, 6);
    assert_eq!(
        state(&app, c(0, 0)),
        Some(CellState::Visible),
        "the read in flight finished"
    );
    assert_eq!(
        state(&app, c(1, 0)),
        Some(CellState::Queued),
        "no new read while suspended"
    );
    let issued = stats(&app).loads_issued;
    steps(&mut app, 6);
    assert_eq!(stats(&app).loads_issued, issued);
    streaming(&mut app).set_suspended(false);
    steps(&mut app, 6);
    assert_eq!(state(&app, c(1, 0)), Some(CellState::Visible));
}

#[test]
fn a_memory_trim_halves_the_budget_and_sheds_the_farthest_cells() {
    let mut app = app();
    let mut s = settings(250.0, 300.0);
    let probe = build(&app, 5, 1, 1, 1, s.clone());
    let own = probe.index.cells[0].own_bytes;
    s.budgets = Some(StreamBudgets {
        memory_budget_bytes: own * 4,
        ..s.effective_budgets()
    });
    let world = build(&app, 5, 1, 1, 1, s);
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 8);
    assert_eq!(stats(&app).resident_cells, 4);
    streaming(&mut app).set_trimmed(true);
    steps(&mut app, 8);
    assert_eq!(stats(&app).resident_cells, 2);
    assert_eq!(state(&app, c(2, 0)), Some(CellState::Unloaded));
    streaming(&mut app).set_trimmed(false);
    steps(&mut app, 8);
    assert_eq!(state(&app, c(2, 0)), Some(CellState::Visible));
}

/// A scripted walk with a teleport; returns the hashes at every 100th step.
fn scripted_run(latency: u32) -> Vec<(u64, u64, usize)> {
    let mut app = app();
    let mut s = settings(150.0, 250.0);
    s.budgets = Some(StreamBudgets {
        max_loads_per_step: 2,
        max_inflight: 3,
        apply_budget_entities: 6,
        ..s.effective_budgets()
    });
    let world = build(&app, 60, 3, 4, 2, s);
    open(&mut app, &world, latency);
    let source = spawn_source(&mut app, 50.0, 150.0);
    let mut hashes = Vec::new();
    for step in 0..1000 {
        let x = if step < 600 {
            50.0 + step as f32
        } else {
            3000.0 + (step - 600) as f32
        };
        move_to(&mut app, source, x, 150.0);
        app.update();
        if step % 100 == 99 {
            let members = count_members(&mut app);
            let now = stats(&app);
            hashes.push((now.trace_hash, now.world_hash, members));
        }
    }
    hashes
}

#[test]
fn the_same_lockstep_run_twice_gives_the_same_hashes() {
    let a = scripted_run(2);
    let b = scripted_run(2);
    assert_eq!(a, b);
    assert_ne!(a, scripted_run(3), "the hash sees the timing");
}

#[test]
fn a_replay_of_an_async_run_matches_it() {
    // The async run: reads finish whenever the workers get to them.
    let mut app = app();
    let world = build(&app, 12, 3, 3, 1, settings(150.0, 250.0));
    let index = world.index.clone();
    open(&mut app, &world, 2);
    streaming(&mut app).set_timing(StreamTiming::Async);
    let source = spawn_source(&mut app, 50.0, 150.0);
    for step in 0..200 {
        move_to(&mut app, source, 50.0 + step as f32 * 5.0, 150.0);
        app.update();
    }
    let trace = streaming(&mut app).snapshot_trace();
    let (original_hash, original_members) = (stats(&app).world_hash, count_members(&mut app));
    assert!(original_members > 0);
    assert_eq!(stats(&app).main_thread_io_waits, 0);

    let mut replay = common::app();
    let world2 = common::World {
        index,
        source: world.source.clone(),
    };
    open(&mut replay, &world2, 2);
    replay
        .world_mut()
        .resource_mut::<WorldStreaming>()
        .set_timing(StreamTiming::Replay(trace));
    // The replay's sources do not matter: every decision comes from the trace.
    steps(&mut replay, 200);
    assert_eq!(stats(&replay).world_hash, original_hash);
    assert_eq!(count_members(&mut replay), original_members);
    assert_eq!(stats(&replay).lockstep_stalls, 0);
}

#[test]
fn a_lockstep_wait_past_the_cap_reports_a_stall_instead_of_hanging() {
    let mut app = app();
    let mut s = settings(40.0, 60.0);
    s.tuning.lockstep_wait_cap_ms = 1;
    let world = build(&app, 1, 1, 1, 1, s);
    world.source.hold(cell_id(c(0, 0)));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 5);
    assert_eq!(stats(&app).lockstep_stalls, 1);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Loading));
    world.source.release(cell_id(c(0, 0)));
    // The late read still lands; the wait for it only looks, and a step that sees it applies it.
    for _ in 0..200 {
        app.update();
        if state(&app, c(0, 0)) == Some(CellState::Visible) {
            return;
        }
        std::thread::yield_now();
    }
    panic!("the late read never landed: {:?}", state(&app, c(0, 0)));
}

#[test]
fn async_timing_never_waits_on_the_main_thread_and_reaches_every_wanted_cell() {
    let mut app = app();
    let world = build(&app, 8, 3, 3, 1, settings(150.0, 250.0));
    open(&mut app, &world, 2);
    streaming(&mut app).set_timing(StreamTiming::Async);
    spawn_source(&mut app, 350.0, 150.0);
    // Asynchronous, so this waits on a counter, with the steps as the bound.
    for _ in 0..100_000 {
        app.update();
        let all = app
            .world()
            .resource::<WorldStreaming>()
            .cells(level())
            .filter(|(cell, _)| (cell.x - 3).abs() <= 1 && cell.z == 1)
            .all(|(_, state)| state == CellState::Visible);
        if all {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(state(&app, c(3, 1)), Some(CellState::Visible));
    assert_eq!(stats(&app).main_thread_io_waits, 0);
}

/// Review finding: a replace of the level with itself must leave the new instance running.
#[test]
fn reopening_the_same_level_keeps_streaming() {
    let mut app = app();
    let world = build(&app, 1, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 4);
    open(&mut app, &world, 1);
    steps(&mut app, 6);
    assert!(app.world().resource::<WorldStreaming>().is_active());
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Visible));
    assert_eq!(count_members(&mut app), 1);
}

/// Review finding: the always-loaded set is cooked in world coordinates.
#[test]
fn the_always_loaded_set_lands_in_the_current_origin_space() {
    use regia_scene::floating_origin::Rebase;
    let mut app = app();
    let world = build(&app, 1, 1, 1, 1, settings(40.0, 60.0));
    let moved: Rebase = app
        .world_mut()
        .resource_mut::<regia_world::WorldOrigin>()
        .0
        .rebase_to([1, 0, 0]);
    assert_eq!(moved.shift[0], 1024.0);
    let always = cook_cell(&app, c(0, 0), 1, 1);
    let source = std::sync::Arc::new(world.source.clone());
    regia_world::open_streamed_from(
        app.world_mut(),
        level(),
        regia_scene::level::LoadMode::Replace,
        world.index.clone(),
        &always,
        source,
    )
    .expect("open");
    let mut query = app.world_mut().query::<&Transform>();
    let xs: Vec<f32> = query.iter(app.world()).map(|t| t.translation.x).collect();
    assert_eq!(xs, vec![-1024.0], "world 0 is 1024 m behind the origin");
}

/// Review finding: a read cancelled while async timing held it must give its slot back in lockstep.
#[test]
fn a_cancelled_read_from_before_lockstep_gives_its_slot_back() {
    let mut app = app();
    let mut s = settings(40.0, 60.0);
    s.budgets = Some(StreamBudgets {
        max_inflight: 1,
        ..s.effective_budgets()
    });
    let world = build(&app, 40, 1, 1, 1, s);
    world.source.hold(cell_id(c(0, 0)));
    open(&mut app, &world, 2);
    streaming(&mut app).set_timing(StreamTiming::Async);
    let source = spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 2);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Loading));
    move_to(&mut app, source, 3050.0, 50.0);
    steps(&mut app, 1);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Unloaded));
    streaming(&mut app).set_timing(StreamTiming::Lockstep { latency_steps: 1 });
    world.source.release(cell_id(c(0, 0)));
    for _ in 0..50 {
        app.update();
        std::thread::yield_now();
        if state(&app, c(30, 0)) == Some(CellState::Visible) {
            break;
        }
    }
    assert_eq!(
        state(&app, c(30, 0)),
        Some(CellState::Visible),
        "{:?}",
        stats(&app)
    );
    assert_eq!(stats(&app).inflight, 0);
}

#[test]
fn a_reopened_level_ignores_reads_of_the_closed_one() {
    let mut app = app();
    let world = build(&app, 1, 1, 1, 1, settings(40.0, 60.0));
    world.source.hold(cell_id(c(0, 0)));
    open(&mut app, &world, 1000);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 2);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Loading));
    regia_scene::level::unload_level(app.world_mut(), level()).expect("unload");
    steps(&mut app, 1);
    // A new open, and the old read lands late with the same level and cell.
    open(&mut app, &world, 2);
    world.source.release(cell_id(c(0, 0)));
    steps(&mut app, 8);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Visible));
    assert_eq!(count_members(&mut app), 1);
}

#[test]
fn closing_the_level_stops_streaming_and_leaves_no_entity() {
    let mut app = app();
    let world = build(&app, 3, 1, 2, 1, settings(250.0, 300.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 8);
    assert!(count_members(&mut app) > 0);
    regia_scene::level::unload_level(app.world_mut(), level()).expect("unload");
    steps(&mut app, 2);
    assert_eq!(count_members(&mut app), 0);
    assert!(!app.world().resource::<WorldStreaming>().is_active());
}

#[test]
fn no_asset_id_is_needed_for_an_unstreamed_app() {
    // The plugin is inert: an app with no streamed level runs its steps without touching a thing.
    let mut app = app();
    let _ = AssetId::default();
    steps(&mut app, 5);
    assert_eq!(stats(&app).plans, 0);
    let _ = Vec3::ZERO;
}

/// Spec 78 § 8.4: spawned cells show in a load set `render.memory_budget` reads, with their VRAM.
#[test]
fn spawned_cells_show_in_the_stream_cells_load_set() {
    let mut app = app();
    let sets = regia_asset::LoadSets::default();
    app.insert_resource(sets.clone());
    let mut world = build(&app, 3, 1, 2, 1, settings(40.0, 60.0));
    for cell in &mut world.index.cells {
        cell.vram_bytes = 1000;
    }
    world.index.content_hash = regia_world::format::WorldIndex::hash_cells(&world.index.cells);
    open(&mut app, &world, 1);
    let name = regia_world::stream_cells_set(level());
    let source = spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 8);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Visible));
    let set = sets.get(&name).expect("defined");
    assert_eq!(set.items.len(), 3);
    assert_eq!(set.bytes().1, 3000);
    let key = format!("{name}/{}", c(0, 0).label());
    assert!(sets.item_resident(regia_asset::LoadKind::Other, &key));
    let cells = stats(&app);
    assert_eq!(cells.resident_vram_estimate, 1000);
    assert!(!cells.vram_over_budget);

    // Walking away evicts it; closing the level forgets the set.
    move_to(&mut app, source, 950.0, 950.0);
    steps(&mut app, 12);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Unloaded));
    assert!(!sets.item_resident(regia_asset::LoadKind::Other, &key));
    regia_scene::level::unload_level(app.world_mut(), level()).expect("unload");
    assert!(sets.get(&name).is_none());
}

/// Load sets inserted after cells spawned still list those cells as resident.
#[test]
fn a_late_load_set_sees_the_cells_spawned_before_it() {
    let mut app = app();
    let world = build(&app, 3, 1, 2, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    let _source = spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 8);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Visible));
    let sets = regia_asset::LoadSets::default();
    app.insert_resource(sets.clone());
    steps(&mut app, 1);
    let key = format!("{}/{}", regia_world::stream_cells_set(level()), c(0, 0).label());
    assert!(sets.item_resident(regia_asset::LoadKind::Other, &key));
}

/// Local space is the process's. A rebase outlives the level that took it, so the last streamed level
/// to close brings the origin home: what stayed moves back and nothing is left offset.
#[test]
fn closing_the_last_streamed_level_brings_the_origin_home() {
    let mut app = app();
    let world = build(&app, 3, 1, 2, 1, settings(250.0, 300.0));
    open(&mut app, &world, 1);
    let source = spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 2);
    streaming(&mut app).request_rebase([2, 0, 0]);
    steps(&mut app, 2);
    let moved = app.world().get::<Transform>(source).expect("source").translation;
    assert_eq!(moved.x, 50.0 - 2048.0);
    assert_eq!(
        app.world().resource::<regia_scene::streaming::OriginOffset>().0,
        [2048.0, 0.0, 0.0]
    );

    regia_scene::level::unload_level(app.world_mut(), level()).expect("unload");
    steps(&mut app, 2);
    assert_eq!(
        app.world().resource::<regia_world::WorldOrigin>().0.cell(),
        [0, 0, 0]
    );
    assert_eq!(
        app.world().resource::<regia_scene::streaming::OriginOffset>().0,
        [0.0; 3]
    );
    let home = app.world().get::<Transform>(source).expect("source").translation;
    assert_eq!(home.x, 50.0, "the entity that stayed moved back with the origin");
}

/// A look-ahead source only adds cells: the camera's implicit source stays and keeps the origin's
/// focus, so a cutscene's prefetch cannot unload what is around the camera.
#[test]
fn a_look_ahead_source_keeps_the_implicit_camera_view_and_the_focus() {
    use regia_scene::stream_plan::SourceId;
    use regia_scene::streaming::{PrimaryStreamView, SourceKind, StreamViews, StreamingSource};
    let mut app = app();
    let world = build(&app, 8, 1, 2, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    app.world_mut().resource_mut::<PrimaryStreamView>().view = Some(([50.0, 0.0, 50.0], [0.0, 0.0, -1.0]));
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(750.0, 0.0, 50.0)),
        regia_scene::GlobalTransform::default(),
        StreamingSource {
            kind: SourceKind::Scripted,
            look_ahead: true,
            ..StreamingSource::default()
        },
    ));
    steps(&mut app, 4);
    let views = &app.world().resource::<StreamViews>().0;
    assert_eq!(views.len(), 2, "{views:?}");
    assert!(views.iter().any(|v| v.source == SourceId::Implicit));
    assert_eq!(
        app.world().resource::<WorldStreaming>().focus(),
        Some([50.0, 0.0, 50.0]),
        "the look-ahead point does not take the focus"
    );
    assert_eq!(
        state(&app, c(0, 0)),
        Some(CellState::Visible),
        "cells around the camera load"
    );
}

/// A `stream.rebase` request no step took dies with the last streamed level: the next level opens
/// with its origin at home.
#[test]
fn a_pending_rebase_request_does_not_outlive_the_last_level() {
    let mut app = app();
    let world = build(&app, 3, 1, 2, 1, settings(250.0, 300.0));
    open(&mut app, &world, 1);
    steps(&mut app, 2);
    streaming(&mut app).request_rebase([2, 0, 0]);
    // Closed before any step takes the request.
    assert!(streaming(&mut app).close_level(level()));
    regia_scene::level::unload_level(app.world_mut(), level()).expect("unload");
    steps(&mut app, 2);
    open(&mut app, &world, 1);
    steps(&mut app, 2);
    assert_eq!(
        app.world().resource::<regia_world::WorldOrigin>().0.cell(),
        [0, 0, 0]
    );
}

/// The trip home obeys `RebaseBlockers` as a rebase does: the origin stays, the block is counted,
/// and the origin goes home once the blocker leaves.
#[test]
fn the_trip_home_waits_for_rebase_blockers() {
    let mut app = app();
    let world = build(&app, 3, 1, 2, 1, settings(250.0, 300.0));
    open(&mut app, &world, 1);
    let source = spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 2);
    streaming(&mut app).request_rebase([2, 0, 0]);
    steps(&mut app, 2);
    let mut blockers = regia_scene::streaming::RebaseBlockers::default();
    blockers.0.insert("host_store");
    app.insert_resource(blockers);
    regia_scene::level::unload_level(app.world_mut(), level()).expect("unload");
    steps(&mut app, 2);
    assert_eq!(
        app.world().resource::<regia_world::WorldOrigin>().0.cell(),
        [2, 0, 0],
        "blocked: the origin stays"
    );
    assert_eq!(
        app.world()
            .get::<Transform>(source)
            .expect("source")
            .translation
            .x,
        50.0 - 2048.0
    );
    assert!(app.world().resource::<WorldStreaming>().stats().rebases_blocked > 0);

    app.world_mut()
        .resource_mut::<regia_scene::streaming::RebaseBlockers>()
        .0
        .clear();
    steps(&mut app, 2);
    assert_eq!(
        app.world().resource::<regia_world::WorldOrigin>().0.cell(),
        [0, 0, 0]
    );
    assert_eq!(
        app.world()
            .get::<Transform>(source)
            .expect("source")
            .translation
            .x,
        50.0
    );
    assert_eq!(
        app.world()
            .resource::<WorldStreaming>()
            .last_rebase()
            .map(|r| r.1),
        Some([-2, 0, 0]),
        "the trip home is the last rebase"
    );
}

/// A level with another origin cell size opens after a rebase only where local space stays put: on
/// its grid the same offset is another cell; off its grid the open is refused.
#[test]
fn a_level_with_another_origin_cell_size_keeps_local_space() {
    let mut app = app();
    let world = build(&app, 3, 1, 2, 1, settings(250.0, 300.0));
    open(&mut app, &world, 1);
    steps(&mut app, 2);
    streaming(&mut app).request_rebase([2, 0, 0]);
    steps(&mut app, 2);
    let before = app
        .world()
        .resource::<regia_world::WorldOrigin>()
        .to_world([0.0; 3]);
    assert_eq!(before, [2048.0, 0.0, 0.0]);

    let mut odd = world.index.clone();
    odd.settings.origin.cell_size_m = 1500.0;
    let other = regia_scene::level::LevelId(regia_core::id::Guid::from_u128(0xbeef));
    let refused = regia_world::open_streamed_from(
        app.world_mut(),
        other,
        regia_scene::level::LoadMode::Additive,
        odd,
        &empty_scene(),
        std::sync::Arc::new(world.source.clone()),
    );
    assert!(refused.is_err(), "2048 m is not on a 1500 m grid");

    let mut wide = world.index.clone();
    wide.settings.origin.cell_size_m = 2048.0;
    regia_world::open_streamed_from(
        app.world_mut(),
        other,
        regia_scene::level::LoadMode::Additive,
        wide,
        &empty_scene(),
        std::sync::Arc::new(world.source.clone()),
    )
    .expect("open on the 2048 m grid");
    let origin = app.world().resource::<regia_world::WorldOrigin>();
    assert_eq!(origin.0.cell(), [1, 0, 0]);
    assert_eq!(origin.to_world([0.0; 3]), before, "local space did not move");
}

/// Audit v033b (Med): switching to async timing accepted the reads lockstep held back by
/// draining a hash map, so the cells went live in a per-process order. They are accepted in issue
/// order: several runtimes (each with its own hash seed) all apply in the order they loaded.
#[test]
fn leaving_lockstep_accepts_the_held_reads_in_issue_order() {
    for _ in 0..4 {
        let mut app = app();
        let mut s = settings(1000.0, 1100.0);
        s.budgets = Some(StreamBudgets {
            max_inflight: 16,
            apply_budget_entities: 1000,
            ..s.effective_budgets()
        });
        let world = build(&app, 6, 1, 1, 1, s);
        open(&mut app, &world, 1000);
        spawn_source(&mut app, 50.0, 50.0);
        steps(&mut app, 3);
        for x in 0..6 {
            assert_eq!(state(&app, c(x, 0)), Some(CellState::Loading), "cell {x}");
        }
        let of_kind = |app: &mut regia_app::App, kind: TraceKind| -> Vec<CellCoord> {
            streaming(app)
                .trace()
                .filter(|event| event.kind == kind)
                .filter_map(|event| event.cell)
                .collect()
        };
        let loading = of_kind(&mut app, TraceKind::Loading);
        assert_eq!(loading.len(), 6);
        streaming(&mut app).set_timing(StreamTiming::Async);
        steps(&mut app, 3);
        assert_eq!(of_kind(&mut app, TraceKind::Applied), loading);
    }
}

/// ADR-2317: with the VRAM bound on, the cook's VRAM estimates limit the plan; off (the default),
/// they are only reported.
#[test]
fn the_vram_bound_limits_resident_cells_only_when_on() {
    let run = |bound: bool| {
        let mut app = app();
        let mut s = settings(900.0, 1000.0);
        if let Some(budgets) = s.budgets.as_mut() {
            budgets.vram_budget_bytes = 2500;
        }
        let mut world = build(&app, 3, 1, 2, 1, s);
        for cell in &mut world.index.cells {
            cell.vram_bytes = 1000;
        }
        world.index.content_hash = regia_world::format::WorldIndex::hash_cells(&world.index.cells);
        if bound {
            streaming(&mut app).set_vram_bound(true);
        }
        open(&mut app, &world, 1);
        spawn_source(&mut app, 50.0, 50.0);
        steps(&mut app, 12);
        let resident: Vec<CellCoord> = app
            .world()
            .resource::<WorldStreaming>()
            .cells(level())
            .filter(|(_, s)| *s == CellState::Visible)
            .map(|(c, _)| c)
            .collect();
        (resident, stats(&app))
    };
    let (all, reported) = run(false);
    assert_eq!(all.len(), 3);
    assert_eq!(reported.resident_vram_estimate, 3000);
    assert!(reported.vram_over_budget);

    let (bounded, stats) = run(true);
    // Nearest first: the cell under the source and its neighbor.
    assert_eq!(bounded, vec![c(0, 0), c(1, 0)]);
    assert_eq!(stats.resident_vram_estimate, 2000);
    assert!(!stats.vram_over_budget);
}

/// Turning the bound on with cells resident sheds the farthest until the rest fit, and lowering
/// the VRAM budget through `set_budgets` does the same.
#[test]
fn turning_the_vram_bound_on_sheds_the_farthest_cells() {
    let mut app = app();
    let mut s = settings(900.0, 1000.0);
    if let Some(budgets) = s.budgets.as_mut() {
        budgets.vram_budget_bytes = 2500;
    }
    let mut world = build(&app, 3, 1, 2, 1, s);
    for cell in &mut world.index.cells {
        cell.vram_bytes = 1000;
    }
    world.index.content_hash = regia_world::format::WorldIndex::hash_cells(&world.index.cells);
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 12);
    assert_eq!(stats(&app).resident_vram_estimate, 3000);
    streaming(&mut app).set_vram_bound(true);
    assert!(app.world().resource::<WorldStreaming>().vram_bound());
    steps(&mut app, 12);
    assert_eq!(state(&app, c(2, 0)), Some(CellState::Unloaded));
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Visible));
    assert_eq!(stats(&app).resident_vram_estimate, 2000);

    let mut budgets = *app
        .world()
        .resource::<WorldStreaming>()
        .budgets(level())
        .expect("open");
    budgets.vram_budget_bytes = 1500;
    streaming(&mut app).set_budgets(level(), budgets).expect("valid");
    steps(&mut app, 12);
    assert_eq!(state(&app, c(1, 0)), Some(CellState::Unloaded));
    assert_eq!(stats(&app).resident_vram_estimate, 1000);
}

// ---------------------------------------------------------------------------
// Determinism review findings (audit sprint-audit-g-world-determinism).
// ---------------------------------------------------------------------------

use regia_app::prelude::{FixedTime, FixedUpdate, TimeUpdateStrategy};
use regia_core::math::Quat;
use regia_ecs::prelude::{
    ChildOf, Component, Has, IntoScheduleConfigs, Query, Res, ResMut, Resource, With, Without, bevy_ecs,
};
use regia_scene::streaming::{
    HlodProxyResidency, SourceKind, StreamAutoMember, StreamFrozen, StreamGuarded, StreamSet, StreamingSource,
};
use std::time::Duration;

/// Review finding (low): the world hash mixes the rebase's step but not its delta, so two runs
/// that rebase on the same step by different cells collide.
#[test]
fn the_world_hash_sees_the_rebase_delta() {
    let run = |delta: [i64; 3]| {
        let mut app = app();
        let world = build(&app, 3, 1, 1, 1, settings(250.0, 300.0));
        open(&mut app, &world, 1);
        steps(&mut app, 2);
        streaming(&mut app).request_rebase(delta);
        steps(&mut app, 2);
        assert!(
            streaming(&mut app).trace().any(|e| e.kind == TraceKind::Rebase),
            "the run rebased by {delta:?}"
        );
        stats(&app).world_hash
    };
    assert_ne!(
        run([1, 0, 0]),
        run([2, 0, 0]),
        "rebases that move the world differently must not share a world hash"
    );
}

/// Review finding (high): the level-open wait is a simulation gate (spec 78 § 8.7): it must clear
/// when the start cells reach `SimReady`, never on HLOD proxy residency — a peer without the
/// render hook has no residency insert, and a peer with slow proxy IO clears on another step.
#[test]
fn level_open_waits_for_sim_ready_not_proxy_residency() {
    use regia_scene::hlod::{CellProxy, HlodConfig};
    use regia_world::format::HlodIndex;

    let run = |insert_residency: bool| -> Option<usize> {
        let mut app = app();
        let mut world = build(&app, 2, 1, 1, 1, settings(40.0, 60.0));
        let proxies: Vec<CellProxy> = (0..2)
            .map(|x| CellProxy {
                cell: c(x, 0),
                min_y: 0.0,
                max_y: 1.0,
                bytes: 10,
            })
            .collect();
        let grid = world.index.settings.grid().expect("grid");
        let index = HlodIndex {
            config: HlodConfig {
                screen_size_threshold: 1.0,
                ..HlodConfig::default()
            },
            proxies,
            node_proxies: Vec::new(),
            node_anchors: Vec::new(),
            node_colors: Vec::new(),
            node_atlases: Vec::new(),
        };
        let nodes = index.tree(&grid).expect("tree").nodes().len() as u32;
        world.index.hlod = Some(HlodIndex {
            node_proxies: vec![Some(cell_id(c(0, 0))); nodes as usize],
            node_anchors: vec![[0; 3]; nodes as usize],
            node_colors: vec![None; nodes as usize],
            node_atlases: vec![None; nodes as usize],
            ..index
        });
        open(&mut app, &world, 1);
        if insert_residency {
            app.world_mut()
                .resource_mut::<HlodProxyResidency>()
                .resident
                .extend(0..nodes);
        }
        spawn_source(&mut app, 50.0, 50.0);
        let mut cleared = None;
        for step in 0..30 {
            app.update();
            if stats(&app).pending_waits == 0 {
                cleared = Some(step);
                break;
            }
        }
        cleared
    };
    let without = run(false).expect("the open wait clears without any proxy residency");
    let with = run(true).expect("the open wait clears with proxy residency");
    assert_eq!(
        without, with,
        "proxy residency must not gate, delay or advance the level-open wait"
    );
}

/// Review finding (high): a replay whose read misses the recorded apply step must keep the cell
/// due and land it when the bytes show up, the way the lockstep issued queue does — not pop the
/// entry and strand the cell in `Loading` forever.
#[test]
fn a_replay_recovers_a_read_that_misses_its_recorded_apply_step() {
    let mut app = app();
    let mut s = settings(40.0, 60.0);
    s.tuning.lockstep_wait_cap_ms = 1;
    let world = build(&app, 1, 1, 3, 1, s);
    let index = world.index.clone();
    let source = world.source.clone();
    open(&mut app, &world, 2);
    streaming(&mut app).set_timing(StreamTiming::Async);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 30);
    let trace = streaming(&mut app).snapshot_trace();
    let applied = trace
        .events
        .iter()
        .find(|e| e.kind == TraceKind::Applied && e.cell == Some(c(0, 0)))
        .map(|e| e.step)
        .expect("recorded apply of (0, 0)");
    let members = count_members(&mut app);
    assert!(members > 0);

    // Replay with the read delayed past its recorded apply step: the recording applied on time,
    // this replay's IO does not.
    let mut replay = common::app();
    let world2 = common::World {
        index,
        source: source.clone(),
    };
    open(&mut replay, &world2, 2);
    replay
        .world_mut()
        .resource_mut::<WorldStreaming>()
        .set_timing(StreamTiming::Replay(trace));
    source.set_delay(cell_id(c(0, 0)), applied + 5);
    for _ in 0..(applied as usize + 40) {
        replay.update();
    }
    assert_eq!(
        state(&replay, c(0, 0)),
        Some(CellState::Visible),
        "the late read still lands and the cell applies"
    );
    assert_eq!(count_members(&mut replay), members);
    assert_eq!(stats(&replay).lockstep_stalls, 1);
}

/// Review finding (high): a recorded failure and retry must replay at the recorded steps even
/// when this replay's read of the cell is still in flight at the recorded retry step: the trace
/// lands the failure, the recorded retry queues against it, and the retried read applies at the
/// recorded apply step.
#[test]
fn a_replay_applies_a_recorded_retry_while_its_read_is_in_flight() {
    // Record: one failure, then the retry succeeds.
    let mut app = app();
    let mut s = settings(40.0, 60.0);
    s.tuning.lockstep_wait_cap_ms = 1;
    let world = build(&app, 1, 1, 3, 1, s);
    let index = world.index.clone();
    let source = world.source.clone();
    open(&mut app, &world, 2);
    streaming(&mut app).set_timing(StreamTiming::Async);
    source.set_error(cell_id(c(0, 0)), Some(CellReadError::Missing));
    // Slow the first read so the recorded failure lands a step after the read issued (the
    // replay's `Failed`-while-`Loading` branch is the one under test; a read that fails in
    // its issue step never reaches it).
    source.set_delay(cell_id(c(0, 0)), 5);
    spawn_source(&mut app, 50.0, 50.0);
    for _ in 0..30 {
        app.update();
        if matches!(state(&app, c(0, 0)), Some(CellState::Failed(_))) {
            source.set_error(cell_id(c(0, 0)), None);
            // Slow the recorded retry's read so the recorded apply lands well after the retry
            // queues: the replay below needs a window where the cancelled first read's stale
            // result has been accepted while the retried read is still in flight.
            source.set_delay(cell_id(c(0, 0)), 20);
            break;
        }
    }
    for _ in 0..60 {
        if state(&app, c(0, 0)) == Some(CellState::Visible) {
            break;
        }
        app.update();
    }
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Visible), "recording");
    let trace = streaming(&mut app).snapshot_trace();
    let failed = trace
        .events
        .iter()
        .find(|e| e.kind == TraceKind::Failed)
        .map(|e| e.step)
        .expect("recorded failure");
    let queued = trace
        .events
        .iter()
        .find(|e| e.kind == TraceKind::Queued && e.step > failed)
        .map(|e| e.step)
        .expect("recorded retry");
    let applied = trace
        .events
        .iter()
        .find(|e| e.kind == TraceKind::Applied)
        .map(|e| e.step)
        .expect("recorded apply");
    assert!(applied > queued, "the retry applies after it queues: {trace:?}");
    let (hash, members) = (stats(&app).world_hash, count_members(&mut app));

    // Replay the same failure, but hold its read in flight past the recorded retry step; once
    // the trace lands the failure, let the retried read through (slower than the recording's,
    // still inside the recorded apply step).
    let mut replay = common::app();
    let world2 = common::World {
        index,
        source: source.clone(),
    };
    let own = world2.index.cells[0].own_bytes;
    open(&mut replay, &world2, 2);
    replay
        .world_mut()
        .resource_mut::<WorldStreaming>()
        .set_timing(StreamTiming::Replay(trace));
    source.set_error(cell_id(c(0, 0)), Some(CellReadError::Missing));
    source.set_delay(cell_id(c(0, 0)), queued + 5);
    let mut cleared = false;
    for _ in 0..(applied as usize + 40) {
        replay.update();
        if !cleared && matches!(state(&replay, c(0, 0)), Some(CellState::Failed(_))) {
            source.set_error(cell_id(c(0, 0)), None);
            source.set_delay(cell_id(c(0, 0)), 18);
            cleared = true;
        }
        // Review fix-world-determinism finding 1: the cancelled first read's reservation is
        // released exactly once. It is released when the trace lands the failure (ADR-5481,
        // per-read `CellRt::reserved_reads`), so the replay's stats match the recording's, and its late
        // stale `Abandoned` frees only its `inflight` slot. While the retry is in flight the
        // retry's reservation is the only one: a double release would read short here (masked
        // only by `saturating_sub`), a missed one long (integ-v048b, ADR-5495).
        if cleared && state(&replay, c(0, 0)) == Some(CellState::Loading) {
            let now = stats(&replay);
            assert!(
                now.inflight <= 2,
                "the cancelled read and the retry at most: {now:?}"
            );
            assert_eq!(
                now.pending_bytes, own,
                "the retry holds the only reservation: {now:?}"
            );
        }
    }
    assert!(
        cleared,
        "the trace's failure landed even though the read was still in flight"
    );
    assert_eq!(
        state(&replay, c(0, 0)),
        Some(CellState::Visible),
        "the recorded retry applies: {:?}",
        streaming(&mut replay).snapshot_trace()
    );
    assert_eq!(count_members(&mut replay), members);
    assert_eq!(
        stats(&replay).world_hash,
        hash,
        "the apply lands on the recorded step"
    );
}

/// One recorded streaming sample: every view's position and direction, and every cell's state,
/// in a deterministic order.
type Sample = (Vec<([f64; 3], Option<[f32; 3]>)>, Vec<(CellCoord, CellState)>);

/// The samples of every fixed step of a run.
#[derive(Resource, Default)]
struct Samples(Vec<Sample>);

#[derive(Component)]
struct Pawn;

#[derive(Component)]
struct OrbitCam;

/// Moves the pawn one cell east and yaws the camera per step, before the sources sample.
fn drive(
    mut pawn: Query<&mut Transform, (With<Pawn>, Without<OrbitCam>)>,
    mut cam: Query<&mut Transform, (With<OrbitCam>, Without<Pawn>)>,
) {
    for mut t in &mut pawn {
        t.translation.x += SIZE as f32;
    }
    for mut t in &mut cam {
        t.rotation = Quat::from_rotation_y(0.2) * t.rotation;
    }
}

/// Records the sampled views and cell states after `StreamSet::Apply` of each step.
fn record(
    mut samples: ResMut<Samples>,
    views: Res<regia_world::StreamViews>,
    streaming: Res<WorldStreaming>,
) {
    let views: Vec<_> = views.0.iter().map(|v| (v.pos, v.dir)).collect();
    let mut cells: Vec<_> = streaming.cells(level()).collect();
    cells.sort_by_key(|(cell, _)| (cell.x, cell.z));
    samples.0.push((views, cells));
}

/// Review finding (high): source views and the guard must sample this step's `Transform`s, not
/// last frame's `GlobalTransform` — a frame that banks several fixed steps (a hitch) must read
/// the same poses as frames that run one step each, for a parented source and a root camera.
#[test]
fn banked_fixed_steps_sample_the_same_poses_as_one_step_frames() {
    let sample_run = |banked: bool| {
        let mut app = app();
        app.world_mut().resource_mut::<FixedTime>().max_substeps = 5;
        let world = build(&app, 8, 1, 1, 1, settings(40.0, 60.0));
        open(&mut app, &world, 1);
        let pawn = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(50.0, 0.0, 50.0)),
                StreamingSource {
                    kind: SourceKind::Player,
                    view_bias: 0.0,
                    ..Default::default()
                },
                Pawn,
            ))
            .id();
        let child = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 0.0, 10.0)),
                StreamingSource {
                    kind: SourceKind::Player,
                    view_bias: 0.0,
                    ..Default::default()
                },
            ))
            .id();
        app.world_mut().entity_mut(child).insert(ChildOf(pawn));
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(50.0, 2.0, 50.0)),
            StreamingSource {
                kind: SourceKind::Camera,
                view_bias: 1.0,
                ..Default::default()
            },
            OrbitCam,
        ));
        app.init_resource::<Samples>();
        app.add_systems(FixedUpdate, drive.before(StreamSet::Sources));
        app.add_systems(FixedUpdate, record.after(StreamSet::Apply));
        if banked {
            // One frame of five banked steps (a half-step of slack so the float step length
            // cannot drop the fifth step), then single-step frames.
            app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
                5.5 / 60.0,
            )));
            app.update();
            app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
                1.0 / 60.0,
            )));
        } else {
            steps(&mut app, 5);
        }
        std::mem::take(&mut app.world_mut().resource_mut::<Samples>().0)
    };
    let one_per_frame = sample_run(false);
    let banked = sample_run(true);
    assert_eq!(one_per_frame.len(), 5, "five fixed steps");
    assert_eq!(
        one_per_frame, banked,
        "banked steps sample the same poses as one-step frames"
    );
}

/// The 64-source limit covers sources and pins together: a pin that would push a pawn out is
/// refused with the limit's code instead of silently dropping the pawn's cells.
#[test]
fn a_pin_is_refused_when_sources_already_fill_the_limit() {
    let mut app = app();
    let world = build(&app, 1, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    for _ in 0..regia_world::runtime::MAX_STREAM_SOURCES {
        spawn_source(&mut app, 10.0, 10.0);
    }
    steps(&mut app, 2);
    let error = streaming(&mut app)
        .pin("camp", [0.0; 3], 1.0, 0, None)
        .expect_err("limit");
    assert_eq!(error.reason(), "stream_source_limit");
    assert_eq!(stats(&app).sources_dropped, 0, "no pawn was dropped");
}

/// An index whose settings cannot run is reported as `stream_settings_invalid`, not as "streaming
/// is off".
#[test]
fn an_invalid_index_opens_with_the_settings_code() {
    let mut app = app();
    let world = build(&app, 1, 1, 1, 1, settings(40.0, 60.0));
    let mut bad = world.index.clone();
    bad.settings.cell_size_m = 1.0;
    let error = regia_world::open_streamed_from(
        app.world_mut(),
        level(),
        regia_scene::level::LoadMode::Replace,
        bad,
        &empty_scene(),
        std::sync::Arc::new(world.source.clone()),
    )
    .expect_err("invalid settings");
    assert_eq!(error.reason(), "stream_settings_invalid");
}

/// Each streamed level reads through the source it was opened with: opening a second level with
/// another source must not redirect the first level's later reads (they would miss).
#[test]
fn an_additive_level_does_not_replace_the_first_levels_source() {
    let mut app = app();
    let a = build(&app, 40, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &a, 1);
    let source = spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 4);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Visible));

    let b = build(&app, 1, 1, 1, 1, settings(40.0, 60.0));
    let other = regia_scene::level::LevelId(regia_core::id::Guid::from_u128(0xb00b));
    regia_world::open_streamed_from(
        app.world_mut(),
        other,
        regia_scene::level::LoadMode::Additive,
        b.index.clone(),
        &empty_scene(),
        std::sync::Arc::new(b.source.clone()),
    )
    .expect("open B");
    // A's far cell is only in A's source.
    move_to(&mut app, source, 3050.0, 50.0);
    steps(&mut app, 8);
    assert_eq!(
        state(&app, c(30, 0)),
        Some(CellState::Visible),
        "read through A's own source"
    );
}

/// A pin's scale must be in the range entity sources use (spec 78 § 7.2, `0.1..=8`): outside it
/// the pin is refused with `stream_settings_invalid` and not stored; the range ends are accepted.
#[test]
fn a_pin_radius_scale_outside_the_entity_source_range_is_refused() {
    let mut app = app();
    let world = build(&app, 3, 1, 2, 1, settings(250.0, 300.0));
    open(&mut app, &world, 1);
    let mut ws = streaming(&mut app);
    let big = ws.pin("big", [0.0; 3], 100.0, 0, None).expect_err("too big");
    let small = ws.pin("small", [0.0; 3], 0.001, 0, None).expect_err("too small");
    assert_eq!(big.reason(), "stream_settings_invalid");
    assert_eq!(small.reason(), "stream_settings_invalid");
    assert_eq!(ws.pins().count(), 0, "a refused pin is not stored");
    let top = ws.pin("top", [0.0; 3], 8.0, 0, None).expect("8 is in range");
    let bottom = ws.pin("bottom", [0.0; 3], 0.1, 0, None).expect("0.1 is in range");
    let scales: Vec<(regia_world::PinId, f32)> = ws.pins().map(|p| (p.id, p.radius_scale)).collect();
    assert!(scales.contains(&(top, 8.0)), "{scales:?}");
    assert!(scales.contains(&(bottom, 0.1)), "{scales:?}");
}

/// A teleport queued for a level dies with it: the next level does not receive the pawn.
#[test]
fn a_queued_teleport_does_not_outlive_the_last_level() {
    let mut app = app();
    let world = build(&app, 3, 1, 2, 1, settings(250.0, 300.0));
    open(&mut app, &world, 1);
    steps(&mut app, 2);
    let pawn = spawn_source(&mut app, 50.0, 50.0);
    let pin = streaming(&mut app)
        .pin("tp", [150.0, 0.0, 50.0], 1.0, 0, None)
        .expect("pin");
    streaming(&mut app).queue_move(pin, pawn, [150.0, 0.0, 50.0], Readiness::SimReady, 10.0);
    assert!(streaming(&mut app).close_level(level()));
    regia_scene::level::unload_level(app.world_mut(), level()).expect("unload");
    open(&mut app, &world, 1);
    steps(&mut app, 12);
    let x = app.world().get::<Transform>(pawn).expect("pawn").translation.x;
    assert!((x - 50.0).abs() < 1e-3, "the stale teleport fired: x = {x}");
    assert_eq!(streaming(&mut app).pins().count(), 0, "its pin is released");
}

/// ADR-8813: a teleport move belongs to the level whose cells hold its target. With levels A and B
/// open, closing A drops the move into A (it never fires, against B's grid or anything else) and
/// releases its pin; the move into B survives and fires.
#[test]
fn closing_one_level_drops_only_its_queued_teleports() {
    let mut app = app();
    // A: cells x 0..=2. B: cells x 5..=7 only, so a target at x = 150 m is A's and x = 650 m is B's.
    let a = build(&app, 3, 1, 2, 1, settings(250.0, 300.0));
    let mut b = build_with(&app, 8, 1, settings(250.0, 300.0), |cell| {
        if cell.x >= 5 { (2, 1) } else { (0, 0) }
    });
    b.index.bounds = [c(5, 0), c(7, 0)];
    open(&mut app, &a, 1);
    let other = regia_scene::level::LevelId(regia_core::id::Guid::from_u128(0xb00b));
    regia_world::open_streamed_from(
        app.world_mut(),
        other,
        regia_scene::level::LoadMode::Additive,
        b.index.clone(),
        &empty_scene(),
        std::sync::Arc::new(b.source.clone()),
    )
    .expect("open B");
    steps(&mut app, 2);
    let into_a = spawn_source(&mut app, 50.0, 50.0);
    let into_b = spawn_source(&mut app, 450.0, 50.0);
    let pin_a = streaming(&mut app)
        .pin("tp-a", [150.0, 0.0, 50.0], 1.0, 0, None)
        .expect("pin a");
    let pin_b = streaming(&mut app)
        .pin("tp-b", [650.0, 0.0, 50.0], 1.0, 0, None)
        .expect("pin b");
    // No step runs between queueing and closing A, so neither move can fire before that.
    streaming(&mut app).queue_move(pin_a, into_a, [150.0, 0.0, 50.0], Readiness::SimReady, 10.0);
    streaming(&mut app).queue_move(pin_b, into_b, [650.0, 0.0, 50.0], Readiness::SimReady, 10.0);
    assert_eq!(streaming(&mut app).queued_moves(), 2);
    assert_eq!(streaming(&mut app).queued_move_level(pin_a), Some(Some(level())));
    assert_eq!(streaming(&mut app).queued_move_level(pin_b), Some(Some(other)));
    assert!(streaming(&mut app).close_level(level()));
    regia_scene::level::unload_level(app.world_mut(), level()).expect("unload A");
    assert_eq!(
        streaming(&mut app).queued_moves(),
        1,
        "A's move is dropped with A"
    );
    let pins: Vec<regia_world::PinId> = streaming(&mut app).pins().map(|p| p.id).collect();
    assert!(
        !pins.contains(&pin_a),
        "the dropped move's pin is released: {pins:?}"
    );
    steps(&mut app, 12);
    let ax = app
        .world()
        .get::<Transform>(into_a)
        .expect("pawn a")
        .translation
        .x;
    assert!(
        (ax - 50.0).abs() < 1e-3,
        "A's teleport fired after A closed: x = {ax}"
    );
    let bx = app
        .world()
        .get::<Transform>(into_b)
        .expect("pawn b")
        .translation
        .x;
    assert!((bx - 650.0).abs() < 1e-3, "B's teleport still fires: x = {bx}");
}

/// One source with a NaN transform does not stall streaming around the good source.
#[test]
fn a_non_finite_source_does_not_freeze_streaming() {
    let mut app = app();
    let world = build(&app, 3, 1, 2, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    spawn_source(&mut app, f32::NAN, 50.0);
    steps(&mut app, 8);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Visible));
}

/// Spec 78 § 7: pins and enabled entity sources share the 64 cap. Sixty-four enabled sources fill
/// it, so the next pin is `stream_source_limit` and no entity source is dropped.
#[test]
fn a_pin_past_sixty_four_enabled_sources_is_refused_without_dropping_one() {
    let mut app = app();
    let world = build(&app, 1, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    for i in 0..regia_world::runtime::MAX_STREAM_SOURCES {
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(i as f32, 0.0, 50.0)),
            StreamingSource {
                enabled: true,
                ..StreamingSource::default()
            },
        ));
    }
    // One step records the enabled count before any pin.
    steps(&mut app, 1);
    let error = streaming(&mut app)
        .pin("extra", [0.0; 3], 1.0, 0, None)
        .expect_err("cap");
    assert_eq!(error.reason(), "stream_source_limit");
    assert_eq!(streaming(&mut app).pins().count(), 0);
    assert_eq!(stats(&app).sources_dropped, 0);
}

/// Spec 78 § 8.4: an async run that already applied a hierarchy does not also take one larger
/// than `apply_budget_entities`. That hierarchy waits until it is the only batch of a run.
#[test]
fn an_over_budget_hierarchy_waits_until_it_is_the_only_batch() {
    let mut app = app();
    let mut s = settings(250.0, 400.0);
    s.budgets = Some(StreamBudgets {
        apply_budget_entities: 10,
        max_inflight: 4,
        ..s.effective_budgets()
    });
    // Cell 0 is one hierarchy of 4 (fits). Cell 1 is one hierarchy of 20 (over the budget).
    let world = build_with(&app, 2, 1, s, |cell| if cell.x == 0 { (1, 4) } else { (1, 20) });
    open(&mut app, &world, 1);
    // `open` forces lockstep. Switch before any load is applied so both cells share an async run.
    streaming(&mut app).set_timing(StreamTiming::Async);
    spawn_source(&mut app, 50.0, 50.0);
    let mut runs = Vec::new();
    for _ in 0..30 {
        app.update();
        let applied = stats(&app).last_apply_entities;
        if applied > 0 {
            runs.push(applied);
        }
        if count_members(&mut app) == 24 && stats(&app).apply_over_budget >= 1 {
            break;
        }
    }
    assert!(
        !runs.contains(&24),
        "the 20-entity hierarchy does not join the run that already applied 4: {runs:?}"
    );
    assert!(runs.contains(&4), "{runs:?}");
    assert!(runs.contains(&20), "{runs:?}");
    assert_eq!(stats(&app).apply_over_budget, 1);
    assert_eq!(count_members(&mut app), 24);
}

fn cells_past_unloaded(app: &regia_app::App) -> usize {
    app.world()
        .resource::<WorldStreaming>()
        .cells(level())
        .filter(|(_, state)| !matches!(state, CellState::Unloaded | CellState::Failed(_)))
        .count()
}

/// A fixed view that still has cells past `max_loads_per_step` must keep queuing loads without
/// walking the whole registry again (audit finding 2).
#[test]
fn perf_rate_limited_plan_does_not_rescan_the_world_each_step() {
    let mut app = app();
    let mut s = settings(800.0, 1200.0);
    s.budgets = Some(StreamBudgets {
        max_loads_per_step: 2,
        max_inflight: 8,
        memory_budget_bytes: 1 << 40,
        ..s.effective_budgets()
    });
    let world = build(&app, 16, 16, 1, 1, s);
    let registered = world.index.cells.len() as u64;
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    app.update();
    let visits = app.world().resource::<WorldStreaming>().plan_cell_visits();
    let queued = cells_past_unloaded(&app);
    assert_eq!(visits, registered, "the first plan visits every registered cell");
    assert!(queued >= 1, "the first plan queued a load");
    steps(&mut app, 6);
    let later = cells_past_unloaded(&app);
    assert!(
        later > queued,
        "later rate-limited steps still queue loads ({queued} then {later})"
    );
    assert_eq!(
        app.world().resource::<WorldStreaming>().plan_cell_visits(),
        visits,
        "a fixed view must not walk the registry again while the plan is only rate-limited"
    );
}

/// Replay reads the trace in place. Copying every past event makes a long trace cost its length
/// on every fixed step (audit finding 5).
#[test]
fn perf_replay_step_does_not_clone_past_events() {
    let mut app = app();
    let world = build(&app, 1, 1, 1, 1, settings(40.0, 80.0));
    open(&mut app, &world, 1);
    let mut events = Vec::with_capacity(2001);
    for i in 0..2000 {
        events.push(TraceEvent {
            step: 0,
            level: None,
            cell: None,
            kind: TraceKind::Rebase,
            detail: Some(format!("past event {i}")),
        });
    }
    events.push(TraceEvent {
        step: 1,
        level: Some(level()),
        cell: Some(c(0, 0)),
        kind: TraceKind::Queued,
        detail: None,
    });
    streaming(&mut app).set_timing(StreamTiming::Replay(StreamTrace { events }));
    app.update();
    // The copy is the whole trace, including events the cursor has already passed. Checked
    // before the cell state: an in-memory source issues the read in the same step, so the cell
    // is `Loading` by the time the frame returns.
    assert_eq!(
        app.world().resource::<WorldStreaming>().replay_events_copied(),
        0,
        "the step copied the trace instead of reading it in place"
    );
    assert!(
        matches!(state(&app, c(0, 0)), Some(CellState::Queued | CellState::Loading)),
        "the event at the cursor still runs, got {:?}",
        state(&app, c(0, 0))
    );
}

/// An async despawn of a hierarchy larger than `apply_budget_entities` ends that run.
/// The cell still finalizes, and a load that became ready in the same run does not spawn.
#[test]
fn an_over_budget_despawn_does_not_also_spawn() {
    let mut app = app();
    let mut s = settings(40.0, 60.0);
    s.budgets = Some(StreamBudgets {
        apply_budget_entities: 10,
        apply_budget_ms: 1.0e6,
        max_inflight: 4,
        ..s.effective_budgets()
    });
    // Cell 0 despawns as one batch of 20. Cell 1 spawns as one batch of 4, which still fits
    // the async floor once the 20 has been counted.
    let world = build_with(&app, 2, 1, s, |cell| if cell.x == 0 { (1, 20) } else { (1, 4) });
    open(&mut app, &world, 1);
    streaming(&mut app).set_timing(StreamTiming::Async);
    world.source.hold(cell_id(c(1, 0)));
    let source = spawn_source(&mut app, 50.0, 50.0);
    let mut saw_visible = false;
    for _ in 0..40 {
        app.update();
        if state(&app, c(0, 0)) == Some(CellState::Visible) {
            saw_visible = true;
            break;
        }
    }
    assert!(
        saw_visible,
        "cell 0 never became visible: {:?}",
        state(&app, c(0, 0))
    );
    // 180 m is 80 m outside cell 0 and inside cell 1, past the 60 m unload radius.
    move_to(&mut app, source, 180.0, 50.0);
    let mut ready = false;
    for _ in 0..20 {
        app.update();
        if state(&app, c(0, 0)) == Some(CellState::Unloading)
            && state(&app, c(1, 0)) == Some(CellState::Loading)
        {
            ready = true;
            break;
        }
    }
    assert!(
        ready,
        "cell 0 {:?}, cell 1 {:?}, members {}",
        state(&app, c(0, 0)),
        state(&app, c(1, 0)),
        count_members(&mut app)
    );
    assert_eq!(
        count_members(&mut app),
        20,
        "the announced unload has not despawned yet"
    );
    let over_before = stats(&app).apply_over_budget;
    world.source.release(cell_id(c(1, 0)));
    app.update();
    assert_eq!(
        stats(&app).last_apply_entities,
        20,
        "over-budget despawn must not also spawn: members {} cell0 {:?} cell1 {:?} over {}",
        count_members(&mut app),
        state(&app, c(0, 0)),
        state(&app, c(1, 0)),
        stats(&app).apply_over_budget
    );
    assert_eq!(count_members(&mut app), 0, "cell 1 must wait for the next run");
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Unloaded));
    assert!(
        !state(&app, c(1, 0)).is_some_and(|s| s.is_spawned()),
        "cell 1 spawned in the over-budget run: {:?}",
        state(&app, c(1, 0))
    );
    assert_eq!(stats(&app).apply_over_budget, over_before + 1);
}

/// An over-budget despawn batch ends its run even when its cell has more hierarchies left
/// (rev-kimi-15 finding 1). Spec 78 § 8.4: "A hierarchy larger than the budget starts only
/// when the run has applied nothing yet, and that run then stops." The next run takes the
/// second hierarchy the same way, and only the run after that spawns cell 1.
#[test]
fn an_over_budget_despawn_with_roots_left_does_not_also_spawn() {
    let mut app = app();
    let mut s = settings(40.0, 60.0);
    s.budgets = Some(StreamBudgets {
        apply_budget_entities: 10,
        apply_budget_ms: 1.0e6,
        max_inflight: 4,
        ..s.effective_budgets()
    });
    // Cell 0: two hierarchies of 20 (each over the budget of 10). Cell 1: one of 4.
    let world = build_with(&app, 2, 1, s, |cell| if cell.x == 0 { (2, 20) } else { (1, 4) });
    open(&mut app, &world, 1);
    streaming(&mut app).set_timing(StreamTiming::Async);
    world.source.hold(cell_id(c(1, 0)));
    let source = spawn_source(&mut app, 50.0, 50.0);
    let mut saw_visible = false;
    for _ in 0..40 {
        app.update();
        if state(&app, c(0, 0)) == Some(CellState::Visible) {
            saw_visible = true;
            break;
        }
    }
    assert!(
        saw_visible,
        "cell 0 never became visible: {:?}",
        state(&app, c(0, 0))
    );
    // 180 m is 80 m outside cell 0 and inside cell 1, past the 60 m unload radius.
    move_to(&mut app, source, 180.0, 50.0);
    let mut ready = false;
    for _ in 0..20 {
        app.update();
        if state(&app, c(0, 0)) == Some(CellState::Unloading)
            && state(&app, c(1, 0)) == Some(CellState::Loading)
        {
            ready = true;
            break;
        }
    }
    assert!(
        ready,
        "cell 0 {:?}, cell 1 {:?}",
        state(&app, c(0, 0)),
        state(&app, c(1, 0))
    );
    assert_eq!(
        count_members(&mut app),
        40,
        "the announced unload has not despawned yet"
    );
    let over_before = stats(&app).apply_over_budget;
    world.source.release(cell_id(c(1, 0)));

    // Run 1: the first hierarchy of 20 goes over the budget, and nothing else applies.
    app.update();
    let applied = stats(&app).last_apply_entities;
    let members = count_members(&mut app);
    let cell1 = state(&app, c(1, 0));
    assert_eq!(
        applied,
        20,
        "over-budget despawn must end the run: members {members} cell0 {:?} cell1 {cell1:?}",
        state(&app, c(0, 0))
    );
    assert_eq!(members, 20, "one hierarchy of cell 0 is left");
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Unloading));
    assert!(
        !cell1.as_ref().is_some_and(|s| s.is_spawned()),
        "cell 1 spawned in the over-budget run: {cell1:?}"
    );
    assert_eq!(stats(&app).apply_over_budget, over_before + 1);

    // Run 2: the second hierarchy goes over the budget too; it finalizes cell 0 and ends the run.
    app.update();
    let cell1 = state(&app, c(1, 0));
    assert_eq!(stats(&app).last_apply_entities, 20);
    assert_eq!(count_members(&mut app), 0, "cell 1 must wait for the next run");
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Unloaded));
    assert!(
        !cell1.as_ref().is_some_and(|s| s.is_spawned()),
        "cell 1 spawned in the over-budget run: {cell1:?}"
    );
    assert_eq!(stats(&app).apply_over_budget, over_before + 2);

    // A later run spawns cell 1.
    let mut spawned = false;
    for _ in 0..10 {
        app.update();
        if count_members(&mut app) == 4 {
            spawned = true;
            break;
        }
    }
    assert!(spawned, "cell 1 never spawned: {:?}", state(&app, c(1, 0)));
}

/// A replay that lands a recorded failure while that cell's read is still in flight must
/// release the reservation once. A second in-flight cell keeps its own bytes reserved.
#[test]
fn a_replay_in_flight_failure_releases_its_reservation_once() {
    let own_of = |world: &World, cell: CellCoord| {
        world
            .index
            .cells
            .iter()
            .find(|record| record.coord == cell)
            .map(|record| record.own_bytes)
            .expect("cell")
    };
    let mut app = app();
    let mut s = settings(100.0, 200.0);
    s.budgets = Some(StreamBudgets {
        max_inflight: 2,
        max_pending_bytes: 1 << 30,
        ..s.effective_budgets()
    });
    s.tuning.max_retries = 0;
    s.tuning.lockstep_wait_cap_ms = 1;
    let world = build(&app, 2, 1, 1, 1, s);
    let own0 = own_of(&world, c(0, 0));
    let own1 = own_of(&world, c(1, 0));
    assert!(own0 > 0 && own1 > 0, "own0 {own0} own1 {own1}");
    let index = world.index.clone();
    let source = world.source.clone();
    open(&mut app, &world, 1);
    streaming(&mut app).set_timing(StreamTiming::Async);
    source.set_error(cell_id(c(0, 0)), Some(CellReadError::Missing));
    source.set_delay(cell_id(c(0, 0)), 3);
    source.set_delay(cell_id(c(1, 0)), 30);
    spawn_source(&mut app, 50.0, 50.0);
    let mut oracle = None;
    for _ in 0..50 {
        app.update();
        if matches!(state(&app, c(0, 0)), Some(CellState::Failed(_)))
            && state(&app, c(1, 0)) == Some(CellState::Loading)
        {
            oracle = Some(stats(&app).pending_bytes);
            break;
        }
    }
    let oracle = oracle.unwrap_or_else(|| {
        panic!(
            "recording never overlapped a failure with the other read: cell0 {:?} cell1 {:?} pending {} inflight {}",
            state(&app, c(0, 0)),
            state(&app, c(1, 0)),
            stats(&app).pending_bytes,
            stats(&app).inflight
        )
    });
    assert_eq!(
        oracle, own1,
        "the recording should be holding only cell 1: own0 {own0} own1 {own1}"
    );
    let trace = streaming(&mut app).snapshot_trace();

    let mut replay = common::app();
    let world2 = common::World {
        index,
        source: source.clone(),
    };
    open(&mut replay, &world2, 1);
    replay
        .world_mut()
        .resource_mut::<WorldStreaming>()
        .set_timing(StreamTiming::Replay(trace));
    let mut saw = false;
    for _ in 0..80 {
        replay.update();
        if matches!(state(&replay, c(0, 0)), Some(CellState::Failed(_)))
            && state(&replay, c(1, 0)) == Some(CellState::Loading)
        {
            assert_eq!(
                stats(&replay).pending_bytes,
                oracle,
                "in-flight failure subtracted twice: own0 {own0} own1 {own1} inflight {}",
                stats(&replay).inflight
            );
            assert_eq!(stats(&replay).inflight, 1, "cell 1's read is still in flight");
            saw = true;
            break;
        }
    }
    assert!(
        saw,
        "replay never overlapped: cell0 {:?} cell1 {:?} pending {}",
        state(&replay, c(0, 0)),
        state(&replay, c(1, 0)),
        stats(&replay).pending_bytes
    );
}

/// A `ChildOf` cycle has no world pose. Walking it up to the depth cap must not sample a
/// source tens of kilometers from where it stands.
#[test]
fn a_parent_cycle_does_not_place_a_streaming_source() {
    use regia_ecs::prelude::ChildOf;
    use regia_scene::streaming::StreamingSource;
    let mut app = app();
    let world = build(&app, 2, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    let a = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(100.0, 0.0, 0.0)),
            StreamingSource {
                enabled: true,
                ..StreamingSource::default()
            },
        ))
        .id();
    let b = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(100.0, 0.0, 0.0)),
            ChildOf(a),
        ))
        .id();
    app.world_mut().entity_mut(a).insert(ChildOf(b));
    assert_eq!(
        app.world().get::<ChildOf>(a).map(|link| link.parent()),
        Some(b),
        "the cycle must stick"
    );
    assert_eq!(
        app.world().get::<ChildOf>(b).map(|link| link.parent()),
        Some(a),
        "the cycle must stick"
    );
    steps(&mut app, 8);
    assert_eq!(
        app.world().get::<ChildOf>(a).map(|link| link.parent()),
        Some(b),
        "the cycle must still be there after the steps"
    );
    assert_eq!(
        app.world().get::<ChildOf>(b).map(|link| link.parent()),
        Some(a),
        "the cycle must still be there after the steps"
    );
    let views = &app.world().resource::<regia_world::StreamViews>().0;
    assert!(!views.is_empty(), "the healthy source is sampled");
    for view in views {
        assert!(
            view.pos[0].abs() < 1000.0 && view.pos[2].abs() < 1000.0,
            "a parent cycle must not walk into a far streaming position: {views:?}"
        );
    }
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Visible));
}

/// The per-step freeze state of every guarded body (review fix-world-determinism finding 2).
#[derive(Resource, Default)]
struct FrozenSamples(Vec<Vec<bool>>);

/// Moves the rig parent towards the cell interior every fixed step, before the guard samples:
/// the guarded body crosses the guard-margin boundary of the x = 100 cell edge at step 6.
fn drive_rig(mut rigs: Query<&mut Transform, With<GuardedRig>>) {
    for mut t in &mut rigs {
        t.translation.x -= 0.25;
    }
}

/// Records every guarded body's freeze state after `StreamSet::Guard` of each step.
fn record_frozen(mut samples: ResMut<FrozenSamples>, bodies: Query<Has<StreamFrozen>, With<StreamGuarded>>) {
    samples.0.push(bodies.iter().collect());
}

#[derive(Component)]
struct GuardedRig;

/// Review fix-world-determinism finding 2: the guard's pose path (`step_pose`, systems.rs) must
/// sample this step's `Transform`s for a parented guarded body under a rotated, scaled parent —
/// pure translations commute, so the source/camera test above cannot catch a reversed compose
/// order, and without a guarded body the guard's pose path had no banked-steps coverage at all.
#[test]
fn banked_fixed_steps_freeze_the_same_parented_guarded_bodies() {
    let frozen_run = |banked: bool| {
        let mut app = app();
        app.world_mut().resource_mut::<FixedTime>().max_substeps = 5;
        let world = build(&app, 2, 1, 1, 1, settings(40.0, 60.0));
        open(&mut app, &world, 2);
        spawn_source(&mut app, 50.0, 50.0);
        // A rotated, scaled parent: the compose order of the pose walk matters here.
        let parent = app
            .world_mut()
            .spawn((
                Transform {
                    translation: Vec3::new(50.0, 0.0, 50.0),
                    rotation: Quat::from_rotation_y(0.6),
                    scale: Vec3::new(1.5, 1.0, 0.75),
                },
                GuardedRig,
            ))
            .id();
        // Starts inside the guard margin of the x = 100 edge of cell (0, 0) (world x ≈ 97),
        // then the rig walks it into the cell interior over the run.
        let body = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(38.0, 1.0, 0.0)),
                StreamGuarded,
                StreamAutoMember,
            ))
            .id();
        app.world_mut().entity_mut(body).insert(ChildOf(parent));
        app.init_resource::<FrozenSamples>();
        app.add_systems(FixedUpdate, drive_rig.before(StreamSet::Guard));
        app.add_systems(FixedUpdate, record_frozen.after(StreamSet::Guard));
        // Three single-step frames, one banked frame of five steps (the body crosses the
        // guard-margin boundary inside it), then single-step frames again: a pose read that
        // latches per frame shows here, a per-step pose walk cannot.
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
        if !banked {
            steps(&mut app, 12);
        } else {
            steps(&mut app, 3);
            app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
                5.5 / 60.0,
            )));
            app.update();
            app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
                1.0 / 60.0,
            )));
            steps(&mut app, 4);
        }
        std::mem::take(&mut app.world_mut().resource_mut::<FrozenSamples>().0)
    };
    let one_per_frame = frozen_run(false);
    let banked = frozen_run(true);
    assert_eq!(one_per_frame.len(), 12, "twelve fixed steps");
    assert_eq!(
        one_per_frame, banked,
        "banked steps freeze the same guarded bodies on the same steps"
    );
    assert!(
        one_per_frame.iter().flatten().any(|f| *f),
        "the body froze before its cell was ready"
    );
    assert!(
        one_per_frame.iter().flatten().any(|f| !f),
        "and woke once its cell was ready"
    );
}

/// deep-zai-terrain-stream #4: a level opened while the low-memory trim is active plans at half the
/// budget from its first step, like the levels that were open when the trim started (spec 78 § 19).
#[test]
fn a_level_opened_during_a_memory_trim_plans_at_half_the_budget() {
    let mut app = app();
    let mut s = settings(250.0, 300.0);
    let probe = build(&app, 5, 1, 1, 1, s.clone());
    let own = probe.index.cells[0].own_bytes;
    s.budgets = Some(StreamBudgets {
        memory_budget_bytes: own * 4,
        ..s.effective_budgets()
    });
    let world = build(&app, 5, 1, 1, 1, s);
    streaming(&mut app).set_trimmed(true);
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 8);
    assert_eq!(
        stats(&app).resident_cells,
        2,
        "the trim applies to a level opened during it"
    );
    streaming(&mut app).set_trimmed(false);
    steps(&mut app, 8);
    assert_eq!(stats(&app).resident_cells, 4);
}

/// deep-zai-terrain-stream #6: `unpin` cancels the teleport queued on that pin; the move never fires
/// once the target becomes ready.
#[test]
fn unpinning_cancels_a_queued_teleport() {
    let mut app = app();
    let world = build(&app, 3, 1, 2, 1, settings(250.0, 300.0));
    open(&mut app, &world, 1);
    steps(&mut app, 2);
    let pawn = spawn_source(&mut app, 50.0, 50.0);
    let pin = streaming(&mut app)
        .pin("tp", [150.0, 0.0, 50.0], 1.0, 0, None)
        .expect("pin");
    streaming(&mut app).queue_move(pin, pawn, [150.0, 0.0, 50.0], Readiness::SimReady, 10.0);
    assert!(streaming(&mut app).unpin(pin));
    steps(&mut app, 12);
    assert_eq!(
        state(&app, c(1, 0)),
        Some(CellState::Visible),
        "the target is ready"
    );
    let x = app.world().get::<Transform>(pawn).expect("pawn").translation.x;
    assert!((x - 50.0).abs() < 1e-3, "the cancelled teleport fired: x = {x}");
    assert_eq!(
        streaming(&mut app).queued_moves(),
        0,
        "the move left with its pin"
    );
}

/// deep-zai-terrain-stream #6: a move on a pin that does not exist (never made, unpinned or expired)
/// is refused, so a `teleport` + `unpin` loop cannot grow the move queue.
#[test]
fn a_move_on_an_unknown_pin_is_refused() {
    let mut app = app();
    let world = build(&app, 3, 1, 2, 1, settings(250.0, 300.0));
    open(&mut app, &world, 1);
    steps(&mut app, 2);
    let pawn = spawn_source(&mut app, 50.0, 50.0);
    for _ in 0..100 {
        let pin = streaming(&mut app)
            .pin("tp", [150.0, 0.0, 50.0], 1.0, 0, None)
            .expect("pin");
        assert!(streaming(&mut app).unpin(pin));
        streaming(&mut app).queue_move(pin, pawn, [150.0, 0.0, 50.0], Readiness::SimReady, 10.0);
    }
    assert_eq!(
        streaming(&mut app).queued_moves(),
        0,
        "moves on removed pins are refused"
    );
}

/// deep-zai-terrain-stream #6: a pin that expires by its ttl takes its move with it. The target
/// cell is out of the pawn's reach, so only the pin would have loaded it: the stale move must not
/// stay queued (or fire later).
#[test]
fn a_pin_that_expires_takes_its_move_with_it() {
    let mut app = app();
    let world = build(&app, 3, 1, 2, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    steps(&mut app, 2);
    let pawn = spawn_source(&mut app, 50.0, 50.0);
    let pin = streaming(&mut app)
        .pin("tp", [250.0, 0.0, 50.0], 1.0, 0, Some(0))
        .expect("pin");
    streaming(&mut app).queue_move(pin, pawn, [250.0, 0.0, 50.0], Readiness::SimReady, 10.0);
    assert_eq!(streaming(&mut app).queued_moves(), 1);
    steps(&mut app, 12);
    assert_eq!(streaming(&mut app).pins().count(), 0, "the pin expired");
    assert_eq!(streaming(&mut app).queued_moves(), 0, "its move left with it");
    let x = app.world().get::<Transform>(pawn).expect("pawn").translation.x;
    assert!((x - 50.0).abs() < 1e-3, "the expired teleport fired: x = {x}");
}

/// deep-zai-terrain-stream #15: byte sums over content-controlled sizes saturate. Two open levels
/// whose resident cells each claim 2^63 bytes publish `u64::MAX`, not a debug-build overflow panic.
#[test]
fn resident_byte_sums_saturate_across_levels() {
    let mut app = app();
    let mut s = settings(250.0, 300.0);
    s.budgets = Some(StreamBudgets {
        memory_budget_bytes: u64::MAX,
        max_pending_bytes: u64::MAX,
        ..s.effective_budgets()
    });
    let huge = |mut world: World| {
        for cell in &mut world.index.cells {
            cell.own_bytes = 1 << 63;
        }
        world.index.content_hash = regia_world::format::WorldIndex::hash_cells(&world.index.cells);
        world
    };
    let first = huge(build(&app, 1, 1, 1, 1, s.clone()));
    let other = regia_scene::level::LevelId(regia_core::id::Guid::from_u128(0xbeef));
    let second = huge(build_empty_level(other, 1, 1, s));
    open(&mut app, &first, 1);
    open_additive(&mut app, other, &second);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 8);
    assert_eq!(state(&app, c(0, 0)), Some(CellState::Visible));
    assert_eq!(
        app.world().resource::<WorldStreaming>().state(other, c(0, 0)),
        Some(CellState::Visible)
    );
    assert_eq!(stats(&app).resident_bytes_estimate, u64::MAX);
}

/// deep-zai-terrain-stream #7: an area the grid refuses (its bounding box passes 2^22 cells) is not
/// ready, and a pin waiting on it stays pending: both fail closed, as the level-open wait does.
/// rev-zai-rc-b terrain 4 (w9-pack-stamp-and-content, ADR-7612): such a wait is now refused when
/// it is set on an open level (`stream_wait_area_refused`); one set before the level opened is
/// still scanned fail closed.
#[test]
fn an_area_the_grid_refuses_is_never_ready() {
    let far_wait = Some(PinWait {
        radius_m: 300_000.0,
        readiness: Readiness::Visible,
    });
    let mut app = app();
    let world = build(&app, 3, 1, 1, 1, settings(250.0, 300.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 8);
    let gates = StreamGates::default();
    let ws = app.world().resource::<WorldStreaming>();
    assert!(ws.ready_near(&gates, [50.0, 0.0, 50.0], 10.0, Readiness::Visible));
    assert!(
        !ws.ready_near(&gates, [50.0, 0.0, 50.0], 300_000.0, Readiness::Visible),
        "a refused area is not ready"
    );
    let pin = streaming(&mut app)
        .pin("far", [50.0, 0.0, 50.0], 1.0, 0, None)
        .expect("pin");
    let error = streaming(&mut app)
        .set_pin_wait(pin, far_wait)
        .expect_err("a wait the grid refuses was accepted");
    assert_eq!(error.reason(), "stream_wait_area_refused");
    steps(&mut app, 2);
    assert_eq!(stats(&app).pending_waits, 0, "the refused wait was stored");

    // Set before any level is open, there is no grid to check it against: once the level opens
    // the scan fails closed and the wait stays pending.
    let mut app = common::app();
    let world = build(&app, 3, 1, 1, 1, settings(250.0, 300.0));
    let pin = streaming(&mut app)
        .pin("far", [50.0, 0.0, 50.0], 1.0, 0, None)
        .expect("pin");
    streaming(&mut app)
        .set_pin_wait(pin, far_wait)
        .expect("no level to check against");
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 8);
    assert_eq!(stats(&app).pending_waits, 1, "a refused wait is not met");
}
