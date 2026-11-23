// SPDX-License-Identifier: MPL-2.0
//! Per-frame streaming cost. Counters and fixed steps only: no wall-clock budget.

mod common;

use common::*;
use regia_core::math::Vec3;
use regia_scene::Transform;
use regia_scene::hlod::{CellProxy, HlodConfig};
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::{
    HlodCut, HlodProxyResidency, Readiness, SourceKind, StreamBudgets, StreamingSource,
};
use regia_world::format::HlodIndex;
use regia_world::{CellState, PinWait, WorldStreaming};

fn c(x: i64, z: i64) -> CellCoord {
    CellCoord::new(x, z)
}

/// A far camera over a resident root proxy rebuilds the leaf map once, then not again until the
/// residency (or the view) changes (audit finding 1).
#[test]
fn perf_hlod_cut_is_rebuilt_only_when_viewers_or_residency_change() {
    let mut app = app();
    let mut world = build(&app, 8, 8, 1, 1, settings(40.0, 80.0));
    let proxies: Vec<CellProxy> = (0..8)
        .flat_map(|x| {
            (0..8).map(move |z| CellProxy {
                cell: c(x, z),
                min_y: 0.0,
                max_y: 1.0,
                bytes: 10,
            })
        })
        .collect();
    let config = HlodConfig {
        screen_size_threshold: 1.0,
        ..HlodConfig::default()
    };
    let grid = world.index.settings.grid().expect("grid");
    let index = HlodIndex {
        config,
        proxies,
        node_proxies: Vec::new(),
        node_anchors: Vec::new(),
        node_colors: Vec::new(),
        node_atlases: Vec::new(),
    };
    let tree = index.tree(&grid).expect("tree");
    assert_eq!(tree.roots().len(), 1, "an 8 by 8 tree merges to one root");
    let nodes = tree.nodes().len();
    let mesh = cell_id(c(0, 0));
    world.index.hlod = Some(HlodIndex {
        node_proxies: vec![Some(mesh); nodes],
        node_anchors: vec![[0; 3]; nodes],
        node_colors: vec![None; nodes],
        ..index
    });
    open(&mut app, &world, 1);
    app.world_mut()
        .resource_mut::<HlodProxyResidency>()
        .resident
        .extend(0..nodes as u32);
    // Far enough that `select` stops at the root, near enough to be a finite view.
    spawn_source(&mut app, 400.0, 8000.0);
    app.update();
    let (entries, proxy, hidden) = {
        let cut = app.world().resource::<HlodCut>();
        let proxy = cut.entries.first().is_some_and(|entry| entry.proxy);
        (cut.entries.len(), proxy, cut.hidden.len())
    };
    assert_eq!(entries, 1, "the far view draws the root proxy");
    assert!(proxy);
    assert_eq!(hidden, 64, "the root covers every leaf");
    let walks = app.world().resource::<WorldStreaming>().hlod_leaf_walks();
    let repairs = app.world().resource::<WorldStreaming>().hlod_repairs();
    assert_eq!(walks, 1, "the first repair walks the root's leaves once");
    assert_eq!(repairs, 1, "the first frame repairs the cut once");
    steps(&mut app, 4);
    assert_eq!(
        app.world().resource::<WorldStreaming>().hlod_leaf_walks(),
        walks,
        "a fixed view and a fixed residency must not walk the leaves again"
    );
    assert_eq!(
        app.world().resource::<WorldStreaming>().hlod_repairs(),
        repairs,
        "a fixed view and a fixed residency must not repair again"
    );
    app.world_mut()
        .resource_mut::<HlodProxyResidency>()
        .resident
        .clear();
    app.update();
    let after = app.world().resource::<WorldStreaming>();
    assert_eq!(
        after.hlod_repairs(),
        repairs + 1,
        "dropping residency rebuilds the cut once"
    );
    assert_eq!(
        after.hlod_leaf_walks(),
        walks,
        "a missing proxy counts its leaves from the stored table"
    );
    steps(&mut app, 3);
    let cached = app.world().resource::<WorldStreaming>();
    assert_eq!(cached.hlod_repairs(), repairs + 1, "the new cut stays cached");
    assert_eq!(
        cached.hlod_leaf_walks(),
        walks,
        "the cached cut does not walk leaves"
    );
}

/// An idle level must not walk unloaded cell records, and a pin wait that is already met must not
/// ask for its disk again until a cell in that disk changes state (audit finding 3).
#[test]
fn perf_idle_step_does_not_walk_unloaded_cells() {
    let mut app = app();
    let mut s = settings(200.0, 300.0);
    s.tuning.wait_on_open = false;
    s.budgets = Some(StreamBudgets {
        max_loads_per_step: 64,
        max_inflight: 64,
        memory_budget_bytes: 1 << 40,
        ..s.effective_budgets()
    });
    let world = build(&app, 24, 24, 1, 1, s);
    assert!(world.index.cells.len() >= 500);
    open(&mut app, &world, 1);
    // The player is the origin focus, so the grid stays on the cooked cells. The pin stands 40 m
    // outside cell (0, 0): its wait disk (50 m) covers that cell, and its load scale (0.1) makes
    // the same gap 400 m, past the 300 m unload radius, so the pin does not hold the cell.
    let player = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(50.0, 0.0, 50.0)),
            StreamingSource {
                kind: SourceKind::Player,
                drives_origin: true,
                view_bias: 0.0,
                ..Default::default()
            },
        ))
        .id();
    let pin = {
        let mut streaming = app.world_mut().resource_mut::<WorldStreaming>();
        let pin = streaming
            .pin("edge", [50.0, 0.0, 140.0], 0.1, 0, None)
            .expect("pin");
        streaming
            .set_pin_wait(
                pin,
                Some(PinWait {
                    radius_m: 50.0,
                    readiness: Readiness::SimReady,
                }),
            )
            .expect("wait");
        pin
    };
    steps(&mut app, 12);
    assert_eq!(
        state(&app, c(0, 0)),
        Some(CellState::Visible),
        "the cell in the pin disk is ready, so the wait can be met"
    );
    let visits = app.world().resource::<WorldStreaming>().cell_record_visits();
    let scans = app.world().resource::<WorldStreaming>().pin_wait_scans();
    assert!(scans >= 1, "the apply checked the pin while the cell was loading");
    steps(&mut app, 20);
    assert_eq!(
        app.world().resource::<WorldStreaming>().cell_record_visits(),
        visits,
        "idle steps walked unloaded cell records"
    );
    assert_eq!(
        app.world().resource::<WorldStreaming>().pin_wait_scans(),
        scans,
        "a met pin wait scanned its disk again"
    );
    // Leaving the unload radius changes the cell the pin is waiting on.
    move_to(&mut app, player, 5000.0, 50.0);
    steps(&mut app, 16);
    let scans_after = app.world().resource::<WorldStreaming>().pin_wait_scans();
    assert_eq!(
        state(&app, c(0, 0)),
        Some(CellState::Unloaded),
        "the cell in the pin disk unloaded"
    );
    assert!(
        scans_after > scans,
        "a cell in the pin disk changing state did not recompute the pin wait (scans {scans} -> {scans_after})"
    );
    assert!(app.world().resource::<WorldStreaming>().pin_info(pin).is_some());
}
