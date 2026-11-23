// SPDX-License-Identifier: MPL-2.0
//! Spec 66 § 8 unit tests: cell math, halo, tiers, stale cells, the scheduler (fixed steps, no
//! clock), determinism and budgets. No GPU.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{box_shape, compile, graph, node, snapshot};
use regia_core::math::hash::mix;
use regia_core::math::{Frustum, Vec3};
use regia_procgen::exec::RunOutput;
use regia_procgen::tiles::{
    AnchorSettings, AnchorView, ApplyBudget, CellCoord, GeneratorTiles, SchedTier, TierPlan, TierReach,
    TileCell, TileJob, TileKey, TileState, ViewGate, cells_in_rect, distance_to_cell, run_cell, run_tiled,
    stale_cells,
};
use regia_procgen::{CancelToken, Evaluator, ProcGraph, RunInputs};

// ------------------------------------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------------------------------------

/// Sorted instance translations (bits) of an output: the comparison key of tiled vs untiled.
fn instance_keys(o: &RunOutput) -> Vec<[u32; 3]> {
    let mut v: Vec<[u32; 3]> = o
        .instance_sets
        .iter()
        .flat_map(|s| s.instances.iter().map(|i| i.translation.map(f32::to_bits)))
        .collect();
    v.sort_unstable();
    v
}

/// `in.self → sample.region (jittered) → points.jitter (scale) → [prune] → spawn.mesh`.
fn scatter_graph(spacing: f64, jitter: f64, prune: bool) -> ProcGraph {
    let mut nodes = vec![
        (1, node("in.self", &[])),
        (
            2,
            node(
                "sample.region",
                &[
                    ("spacing_m", &format!("{spacing:?}")),
                    ("jitter", &format!("{jitter:?}")),
                ],
            ),
        ),
        (
            3,
            node(
                "points.jitter",
                &[
                    ("scale", "[(1.0, 1.0, 1.0), (4.0, 1.0, 4.0)]"),
                    ("scale_axes", "each"),
                ],
            ),
        ),
        (5, node("spawn.mesh", &[("entries", "[(mesh: \"rock.glb\")]")])),
    ];
    let mut edges = vec![(1, "out", 2, "region"), (2, "out", 3, "in")];
    if prune {
        nodes.push((4, node("points.prune", &[("max_footprint_m", "4.0")])));
        edges.push((3, "out", 4, "in"));
        edges.push((4, "out", 5, "in"));
    } else {
        edges.push((3, "out", 5, "in"));
    }
    graph(nodes, &edges)
}

fn untiled(g: &ProcGraph, inputs: &RunInputs) -> RunOutput {
    let c = compile(g);
    match Evaluator::new().run(&c, inputs, &CancelToken::new()) {
        Ok(r) => r.output,
        Err(e) => panic!("untiled run failed: {e}"),
    }
}

fn tiled(
    g: &ProcGraph,
    inputs: &RunInputs,
    base: f64,
    halo: f64,
    threads: usize,
) -> regia_procgen::tiles::TiledRun {
    let c = compile(g);
    let plan = TierPlan::build(&c, base).unwrap_or_else(|e| panic!("tier plan: {:?}", e.0));
    let bounds = inputs
        .world
        .self_info()
        .map(|s| s.bounds)
        .unwrap_or_else(|| box_shape([0.0; 3], [1.0; 3], 0.0));
    let ev = Evaluator::new();
    let job = TileJob {
        evaluator: &ev,
        graph: &c,
        plan: &plan,
        inputs,
        bounds: &bounds,
        halo_m: halo,
    };
    match run_tiled(&job, threads, &CancelToken::new()) {
        Ok(r) => r,
        Err(e) => panic!("tiled run failed: {e}"),
    }
}

/// A deterministic float in [0, 1) from two keys.
fn unit(a: u32, b: u32) -> f64 {
    f64::from(mix(a, b)) / 4_294_967_296.0
}

// ------------------------------------------------------------------------------------------------
// 1. Cell math and ownership
// ------------------------------------------------------------------------------------------------

#[test]
fn t01_cells_are_world_aligned_and_max_edge_belongs_to_next_cell() {
    assert_eq!(CellCoord::containing(0.0, 0.0, 64.0), CellCoord::new(0, 0));
    assert_eq!(CellCoord::containing(63.999, 10.0, 64.0), CellCoord::new(0, 0));
    assert_eq!(CellCoord::containing(64.0, 64.0, 64.0), CellCoord::new(1, 1));
    assert_eq!(CellCoord::containing(-0.001, -64.0, 64.0), CellCoord::new(-1, -1));
    assert_eq!(CellCoord::new(-1, 2).rect(64.0), [-64.0, 128.0, 0.0, 192.0]);
    assert_eq!(distance_to_cell([32.0, 32.0], CellCoord::new(0, 0), 64.0), 0.0);
    assert_eq!(distance_to_cell([-3.0, 68.0], CellCoord::new(0, 0), 64.0), 5.0);
    let cells = cells_in_rect([-10.0, -10.0, 70.0, 5.0], 64.0);
    assert_eq!(
        cells,
        vec![
            CellCoord::new(-1, -1),
            CellCoord::new(-1, 0),
            CellCoord::new(0, -1),
            CellCoord::new(0, 0),
            CellCoord::new(1, -1),
            CellCoord::new(1, 0),
        ]
    );
}

#[test]
fn t01_union_of_cells_equals_untiled_run_without_neighborhood_nodes() {
    // An off-grid box so cells are clipped on every side.
    let snap = snapshot(box_shape([10.5, 0.0, -7.25], [150.0, 20.0, 130.0], 0.0), None);
    let inputs = RunInputs::new(snap, 99);
    let g = scatter_graph(3.0, 0.8, false);
    let whole = untiled(&g, &inputs);
    let t = tiled(&g, &inputs, 64.0, 8.0, 1);
    assert!(t.cells.len() >= 20, "cells: {}", t.cells.len());
    let a = instance_keys(&whole);
    let b = instance_keys(&t.output);
    assert!(a.len() > 5000, "instances: {}", a.len());
    // No duplicates, none missing: the sorted lists are equal.
    let unique: BTreeSet<_> = b.iter().collect();
    assert_eq!(unique.len(), b.len(), "an instance was placed twice");
    assert_eq!(a, b);
    // Every cell owns only instances inside its half-open rectangle.
    for c in t.cells.values() {
        for s in &c.output.instance_sets {
            for i in &s.instances {
                let (x, z) = (f64::from(i.translation[0]), f64::from(i.translation[2]));
                assert!(x >= c.rect[0] && x < c.rect[2] && z >= c.rect[1] && z < c.rect[3]);
            }
        }
    }
}

// ------------------------------------------------------------------------------------------------
// 2. Halo
// ------------------------------------------------------------------------------------------------

/// Symmetric difference between two sorted instance lists.
fn difference(a: &[[u32; 3]], b: &[[u32; 3]]) -> usize {
    let sa: BTreeSet<_> = a.iter().collect();
    let sb: BTreeSet<_> = b.iter().collect();
    sa.symmetric_difference(&sb).count()
}

#[test]
fn t02_halo_keeps_prune_decisions_at_seams() {
    // The spec 66 § 8 test map: 2 km × 2 km, 400 000 candidates (spacing 2000 / √400000 ≈ 3.162 m),
    // footprints ≤ 4 m, halo 8 m. Flat: `sample.region` places at the region's center height.
    let snap = snapshot(box_shape([0.0; 3], [1000.0, 10.0, 1000.0], 0.0), None);
    let mut inputs = RunInputs::new(snap, 7);
    inputs.limits.points_per_item = 1_000_000;
    inputs.limits.total_elements = 10_000_000;
    inputs.limits.memory_bytes = 4 << 30;
    let spacing = 2000.0 / 400_000f64.sqrt();
    let g = scatter_graph(spacing, 0.9, true);
    let whole = untiled(&g, &inputs);
    let a = instance_keys(&whole);
    let with_halo = tiled(&g, &inputs, 128.0, 8.0, 4);
    let no_halo = tiled(&g, &inputs, 128.0, 0.0, 4);
    let d_halo = difference(&a, &instance_keys(&with_halo.output));
    let d_none = difference(&a, &instance_keys(&no_halo.output));
    let seam: u64 = with_halo.cells.values().map(|c| c.seam_mismatch_risk).sum();
    println!(
        "halo test: {} instances untiled; differing with halo 8 m: {d_halo}; with halo 0: {d_none}; seam_mismatch_risk {seam}",
        a.len()
    );
    assert!(a.len() > 50_000, "instances: {}", a.len());
    assert!(
        (d_halo as f64) <= 0.001 * a.len() as f64,
        "halo 8 m: {d_halo} of {} differ (> 0.1 %)",
        a.len()
    );
    assert!(
        d_none > d_halo,
        "halo 0 ({d_none}) should differ more than halo 8 m ({d_halo})"
    );
}

// ------------------------------------------------------------------------------------------------
// 3. Tiers
// ------------------------------------------------------------------------------------------------

/// `in.self → sample.region → tiles.tier(Whole) → tiles.tier(256) → [tiles.tier(32) → out.points]`.
fn tier_graph(fine: &str, coarse: &str, upward: bool) -> ProcGraph {
    let mut nodes = vec![
        (1, node("in.self", &[])),
        (2, node("sample.region", &[("spacing_m", "10.0")])),
        (3, node("tiles.tier", &[("cell_m", "Whole")])),
        (4, node("tiles.tier", &[("cell_m", coarse)])),
        (5, node("tiles.tier", &[("cell_m", fine)])),
        (6, node("out.points", &[("name", "\"fine\"")])),
        (7, node("out.points", &[("name", "\"coarse\"")])),
    ];
    let mut edges = vec![
        (1, "out", 2, "region"),
        (2, "out", 3, "in"),
        (3, "out", 4, "in"),
        (4, "out", 5, "in"),
        (5, "out", 6, "in"),
        (4, "out", 7, "in"),
    ];
    if upward {
        nodes.push((8, node("tiles.tier", &[("cell_m", "Meters(256.0)")])));
        edges.push((5, "out", 8, "in"));
    }
    graph(nodes, &edges)
}

#[test]
fn t03_tiers_compile_nest_and_reject_upward_edges() {
    let g = tier_graph("Meters(32.0)", "Meters(256.0)", false);
    let c = compile(&g);
    let plan = TierPlan::build(&c, 64.0).unwrap_or_else(|e| panic!("{:?}", e.0));
    let sizes: Vec<Option<f64>> = plan.tiers.iter().map(|t| t.cell_m).collect();
    assert_eq!(sizes, vec![None, Some(256.0), Some(32.0)]);

    let bad = TierPlan::build(&compile(&tier_graph("Meters(32.0)", "Meters(96.0)", false)), 64.0);
    assert!(bad.as_ref().is_err_and(|e| e.has("tier_not_nested")), "{bad:?}");

    let up = TierPlan::build(&compile(&tier_graph("Meters(32.0)", "Meters(256.0)", true)), 64.0);
    assert!(up.as_ref().is_err_and(|e| e.has("tier_upward_edge")), "{up:?}");
}

#[test]
fn t03_coarse_points_are_clipped_to_each_fine_cell_plus_halo() {
    let half = 300.0f32;
    let snap = snapshot(box_shape([0.0; 3], [half, 10.0, half], 0.0), None);
    let inputs = RunInputs::new(snap, 3);
    let g = tier_graph("Meters(32.0)", "Meters(256.0)", false);
    let halo = 6.0;
    let t = tiled(&g, &inputs, 64.0, halo, 1);
    let plan = TierPlan::build(&compile(&g), 64.0).unwrap_or_else(|e| panic!("{:?}", e.0));
    let fine = plan.tier_for_size(Some(32.0)).unwrap_or(0);
    // Grid points of `sample.region` (jitter 0): (k + 0.5) · 10 inside the bounds.
    let grid: Vec<f64> = (-40..40)
        .map(|k| (f64::from(k) + 0.5) * 10.0)
        .filter(|v| v.abs() <= f64::from(half))
        .collect();
    let mut checked = 0;
    for c in t.cells.values().filter(|c| c.key.tier == fine) {
        let r = [
            c.rect[0] - halo,
            c.rect[1] - halo,
            c.rect[2] + halo,
            c.rect[3] + halo,
        ];
        // The coarse cell's own run covered its rectangle plus halo; the fine rectangle plus halo is
        // inside it, so the expected count is the grid points in the fine rectangle plus halo.
        let n_x = grid.iter().filter(|x| **x >= r[0] && **x < r[2]).count();
        let n_z = grid.iter().filter(|z| **z >= r[1] && **z < r[3]).count();
        let injected = c
            .report
            .nodes
            .iter()
            .find(|l| l.id == 5)
            .map_or(usize::MAX, |l| l.elements);
        assert_eq!(injected, n_x * n_z, "cell {:?}", c.key);
        checked += 1;
    }
    assert!(checked >= 300, "fine cells checked: {checked}");
    // Coarse points are owned by the 256 m cells and fine points by the 32 m cells: both exports
    // hold every grid point exactly once.
    let total = grid.len() * grid.len();
    assert_eq!(t.output.points.get("coarse").map(|p| p.len()), Some(total));
    assert_eq!(t.output.points.get("fine").map(|p| p.len()), Some(total));
}

// ------------------------------------------------------------------------------------------------
// 4. Stale cells
// ------------------------------------------------------------------------------------------------

#[test]
fn t04_an_edit_marks_only_the_cells_whose_halo_contains_it() {
    let g = scatter_graph(4.0, 0.5, false);
    let plan = TierPlan::build(&compile(&g), 128.0).unwrap_or_else(|e| panic!("{:?}", e.0));
    let bounds = box_shape([0.0; 3], [1024.0, 10.0, 1024.0], 0.0);
    let mid = stale_cells(&plan, &bounds, 8.0, [64.0, 64.0, 64.5, 64.5]);
    assert_eq!(mid.len(), 1, "{mid:?}");
    let corner = stale_cells(&plan, &bounds, 8.0, [127.0, 127.0, 127.2, 127.2]);
    assert_eq!(corner.len(), 4, "{corner:?}");
    let edge = stale_cells(&plan, &bounds, 8.0, [127.0, 64.0, 127.2, 64.2]);
    assert_eq!(edge.len(), 2, "{edge:?}");
}

// ------------------------------------------------------------------------------------------------
// 5-8. Scheduler (fixed steps, no clock)
// ------------------------------------------------------------------------------------------------

fn settings(build: f32, release: f32) -> AnchorSettings {
    AnchorSettings {
        tiers: vec![TierReach {
            cell_m: 64.0,
            build_m: build,
            release_factor: release,
            view_bias: 0.5,
            view_gate: None,
        }],
        max_new_per_frame: Some(10_000),
        max_running: Some(10_000),
        ..AnchorSettings::default()
    }
}

fn one_tier(bounds: f64) -> GeneratorTiles {
    GeneratorTiles::new(
        [-bounds, -bounds, bounds, bounds],
        vec![SchedTier {
            index: 0,
            cell_m: Some(64.0),
        }],
    )
}

/// A fake host: every started cell runs and finishes at once with `elements` instances.
fn host(g: &mut GeneratorTiles, plan: &regia_procgen::tiles::StepPlan, elements: u64) {
    for k in &plan.start {
        g.set_running(*k);
        g.finished(*k, elements, 1024, [0; 32], 0.0);
    }
}

fn cell_distance(k: TileKey, p: Vec3) -> f64 {
    distance_to_cell([f64::from(p.x), f64::from(p.z)], k.cell, 64.0)
}

#[test]
fn t05_straight_paths_request_and_release_each_cell_once() {
    let s = settings(200.0, 1.25);
    for path in 0..1000u32 {
        let mut g = one_tier(20_000.0);
        let start = Vec3::new(
            (unit(path, 1) * 4000.0 - 2000.0) as f32,
            0.0,
            (unit(path, 2) * 4000.0 - 2000.0) as f32,
        );
        let angle = unit(path, 3) * std::f64::consts::TAU;
        let dir = Vec3::new(angle.cos() as f32, 0.0, angle.sin() as f32);
        let mut requested: BTreeMap<TileKey, u32> = BTreeMap::new();
        let mut released: BTreeMap<TileKey, u32> = BTreeMap::new();
        let mut prev_wanted: Vec<TileKey> = Vec::new();
        for step in 0..80u32 {
            let pos = start + dir * (10.0 * step as f32);
            let anchors = [AnchorView::at(pos)];
            let before: BTreeSet<TileKey> = g.entries.keys().copied().collect();
            let plan = g.step(&anchors, &s, &mut ApplyBudget::default());
            for k in g.entries.keys() {
                if !before.contains(k) {
                    *requested.entry(*k).or_default() += 1;
                }
            }
            for k in &plan.release {
                *released.entry(*k).or_default() += 1;
                assert!(
                    cell_distance(*k, pos) > 250.0 - 1e-6,
                    "path {path} step {step}: {k:?} released at {} m",
                    cell_distance(*k, pos)
                );
            }
            // A cell wanted last step and still within build_m is resident now.
            for k in &prev_wanted {
                if cell_distance(*k, pos) <= 200.0 {
                    assert_eq!(
                        g.state(*k),
                        Some(TileState::Resident),
                        "path {path} step {step}: {k:?}"
                    );
                }
            }
            host(&mut g, &plan, 10);
            prev_wanted = g.wanted_keys(&anchors, &s);
        }
        assert!(
            requested.values().all(|n| *n == 1),
            "path {path}: a cell was requested twice"
        );
        assert!(
            released.values().all(|n| *n == 1),
            "path {path}: a cell was released twice"
        );
    }
}

#[test]
fn t06_oscillating_at_the_build_boundary_does_not_thrash() {
    let s = settings(200.0, 1.25);
    let mut g = one_tier(10_000.0);
    // Cell (4, 0) spans x 256..320: its distance from x = 56 is exactly 200 m.
    let mut requests = 0;
    let mut releases = 0;
    for step in 0..200u32 {
        let x = 56.0 + if step % 2 == 0 { 1.0 } else { -1.0 };
        let before = g.entries.len();
        let plan = g.step(
            &[AnchorView::at(Vec3::new(x, 0.0, 32.0))],
            &s,
            &mut ApplyBudget::default(),
        );
        if step > 1 {
            requests += g.entries.len().saturating_sub(before);
            releases += plan.release.len();
        }
        host(&mut g, &plan, 10);
    }
    assert_eq!((requests, releases), (0, 0));
    assert!(
        g.state(TileKey {
            tier: 0,
            cell: CellCoord::new(4, 0)
        })
        .is_some()
    );
}

#[test]
fn t07_teleport_drops_queued_cells_near_the_old_position_in_the_same_step() {
    let mut s = settings(200.0, 1.25);
    s.max_running = Some(2);
    s.max_new_per_frame = Some(2);
    let mut g = one_tier(20_000.0);
    let old = Vec3::new(0.0, 0.0, 0.0);
    g.step(&[AnchorView::at(old)], &s, &mut ApplyBudget::default());
    let queued_before = g
        .entries
        .values()
        .filter(|e| e.state == TileState::Queued)
        .count();
    assert!(queued_before > 10, "queued: {queued_before}");
    let plan = g.step(
        &[AnchorView::at(Vec3::new(5000.0, 0.0, 0.0))],
        &s,
        &mut ApplyBudget::default(),
    );
    assert_eq!(plan.dropped.len(), queued_before);
    assert_eq!(plan.cancel.len(), 2, "the two running cells are cancelled");
    assert!(
        g.entries
            .keys()
            .all(|k| cell_distance(*k, Vec3::new(5000.0, 0.0, 0.0)) <= 200.0)
    );
    assert_eq!(g.counters.teleports, 1);
}

fn camera(eye: Vec3, yaw_deg: f32) -> AnchorView {
    let (s, c) = (yaw_deg.to_radians().sin(), yaw_deg.to_radians().cos());
    let dir = Vec3::new(c, 0.0, s);
    let view = regia_core::glam::camera::rh::view::look_to_mat4(eye, dir, Vec3::Y);
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(60f32.to_radians(), 1.0, 0.1, 5000.0);
    AnchorView {
        pos: eye,
        view_dir: Some(dir),
        frustum: Some(Frustum::from_view_projection(&(proj * view))),
        reach_scale: 1.0,
    }
}

#[test]
fn t08_view_gate_skips_cells_behind_and_keeps_cells_inside_the_release_margin() {
    let mut s = settings(400.0, 1.25);
    s.tiers[0].view_gate = Some(ViewGate {
        build_margin_deg: 5.0,
        release_margin_deg: 40.0,
    });
    let mut g = one_tier(10_000.0);
    let eye = Vec3::new(0.0, 2.0, 0.0);
    let a = camera(eye, 0.0);
    let plan = g.step(&[a], &s, &mut ApplyBudget::default());
    host(&mut g, &plan, 10);
    g.step(&[a], &s, &mut ApplyBudget::default());
    // Facing +x: nothing far behind (x < -100) is built.
    assert!(!g.entries.is_empty());
    assert!(
        g.entries.keys().all(|k| k.cell.x >= -2),
        "{:?}",
        g.entries.keys().collect::<Vec<_>>()
    );
    let ahead = TileKey {
        tier: 0,
        cell: CellCoord::new(4, 0),
    };
    assert_eq!(g.state(ahead), Some(TileState::Resident));
    // Turn 45°: the cell ahead (center ~288 m along +x) is now ~45° off axis, outside the 30° + 5°
    // build cone but inside the 30° + 40° release cone: it stays.
    let turned = camera(eye, 45.0);
    let plan = g.step(&[turned], &s, &mut ApplyBudget::default());
    assert!(!plan.release.contains(&ahead));
    assert_eq!(g.state(ahead), Some(TileState::Resident));
    // Turn around: it is released.
    let back = camera(eye, 180.0);
    let plan = g.step(&[back], &s, &mut ApplyBudget::default());
    assert!(plan.release.contains(&ahead));
}

/// Audit wave 5 (procgen-g2 High): a camera 100 m up, pitched 50° down with a 50° field of view,
/// sees the ground from about 27 m to 214 m ahead. The 64 m cell centered 160 m ahead lies 32°
/// below the horizon, inside the view, yet tested at the eye's height it sat outside the top plane
/// and was never built. The cell is now a column, so it is built; a cell behind is still not.
#[test]
fn t08b_a_camera_looking_down_builds_the_ground_ahead() {
    let mut s = settings(400.0, 1.25);
    s.tiers[0].view_gate = Some(ViewGate {
        build_margin_deg: 5.0,
        release_margin_deg: 40.0,
    });
    let eye = Vec3::new(0.0, 100.0, 0.0);
    let pitch = 50f32.to_radians();
    let dir = Vec3::new(pitch.cos(), -pitch.sin(), 0.0);
    let view = regia_core::glam::camera::rh::view::look_to_mat4(eye, dir, Vec3::Y);
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(50f32.to_radians(), 1.0, 0.1, 5000.0);
    let a = AnchorView {
        pos: eye,
        view_dir: Some(dir),
        frustum: Some(Frustum::from_view_projection(&(proj * view))),
        reach_scale: 1.0,
    };
    let mut g = one_tier(10_000.0);
    let plan = g.step(&[a], &s, &mut ApplyBudget::default());
    let key = |x: i64| TileKey {
        tier: 0,
        cell: CellCoord::new(x, 0),
    };
    assert!(plan.start.contains(&key(2)), "{:?}", plan.start);
    assert!(!plan.start.contains(&key(-4)), "{:?}", plan.start);
}

// ------------------------------------------------------------------------------------------------
// 9. Determinism
// ------------------------------------------------------------------------------------------------

#[test]
fn t09_cell_output_does_not_depend_on_path_or_concurrency() {
    let snap = snapshot(box_shape([0.0; 3], [600.0, 10.0, 600.0], 0.0), None);
    let inputs = RunInputs::new(snap, 21);
    let g = scatter_graph(6.0, 0.7, true);
    let c = compile(&g);
    let plan = TierPlan::build(&c, 64.0).unwrap_or_else(|e| panic!("{:?}", e.0));
    let bounds = box_shape([0.0; 3], [600.0, 10.0, 600.0], 0.0);
    let ev = Evaluator::new();
    let job = TileJob {
        evaluator: &ev,
        graph: &c,
        plan: &plan,
        inputs: &inputs,
        bounds: &bounds,
        halo_m: 8.0,
    };
    let s = settings(200.0, 1.25);
    let end = Vec3::new(100.0, 0.0, -50.0);
    let drive = |path: &[Vec3]| -> BTreeMap<TileKey, [u8; 32]> {
        let mut gt = GeneratorTiles::for_plan([-600.0, -600.0, 600.0, 600.0], &plan);
        let mut hashes = BTreeMap::new();
        for p in path.iter().chain(std::iter::repeat_n(&end, 3)) {
            let sp = gt.step(&[AnchorView::at(*p)], &s, &mut ApplyBudget::default());
            for k in &sp.start {
                gt.set_running(*k);
                match run_cell(&job, *k, &CancelToken::new()) {
                    Ok(cell) => {
                        gt.finished(*k, cell.output.instance_count() as u64, 0, cell.output_hash, 0.0)
                    }
                    Err(e) => panic!("cell {k:?}: {e}"),
                }
            }
        }
        for (k, e) in &gt.entries {
            if e.state == TileState::Resident
                && let Some(h) = e.output_hash
            {
                hashes.insert(*k, h);
            }
        }
        hashes
    };
    let a: Vec<Vec3> = (0..30)
        .map(|i| Vec3::new(-500.0 + 20.0 * i as f32, 0.0, -50.0))
        .collect();
    let b: Vec<Vec3> = (0..30)
        .map(|i| Vec3::new(100.0, 0.0, 450.0 - 16.0 * i as f32))
        .collect();
    let ha = drive(&a);
    let hb = drive(&b);
    let wanted: BTreeSet<TileKey> = GeneratorTiles::for_plan([-600.0, -600.0, 600.0, 600.0], &plan)
        .wanted_keys(&[AnchorView::at(end)], &s)
        .into_iter()
        .collect();
    assert!(wanted.len() > 20);
    for k in &wanted {
        assert_eq!(ha.get(k), hb.get(k), "cell {k:?}");
        assert!(ha.contains_key(k), "cell {k:?} not resident");
    }
    // 1 vs 4 concurrent runs.
    let one = run_tiled(&job, 1, &CancelToken::new()).unwrap_or_else(|e| panic!("{e}"));
    let four = run_tiled(&job, 4, &CancelToken::new()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(one.output_hash, four.output_hash);
    let h1: Vec<[u8; 32]> = one.cells.values().map(|c| c.output_hash).collect();
    let h4: Vec<[u8; 32]> = four.cells.values().map(|c| c.output_hash).collect();
    assert_eq!(h1, h4);
    // A standalone cell run equals the same cell in the full run.
    for (k, cell) in one.cells.iter().step_by(7) {
        let alone = run_cell(&job, *k, &CancelToken::new()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(alone.output_hash, cell.output_hash, "cell {k:?}");
    }
}

// ------------------------------------------------------------------------------------------------
// 10. Budgets
// ------------------------------------------------------------------------------------------------

#[test]
fn t10_element_budget_spreads_a_large_cell_over_frames() {
    let mut s = settings(10.0, 1.25);
    s.apply_budget_elements = Some(8000);
    let mut g = one_tier(10_000.0);
    let a = [AnchorView::at(Vec3::new(32.0, 0.0, 32.0))];
    let budget = |s: &AnchorSettings| ApplyBudget {
        elements: s.apply_budget_elements.map(u64::from),
    };
    let plan = g.step(&a, &s, &mut budget(&s));
    assert_eq!(plan.start.len(), 1);
    let key = plan.start[0];
    g.set_running(key);
    g.finished(key, 50_000, 1 << 20, [1; 32], 3.0);
    let mut frames = 0;
    loop {
        let plan = g.step(&a, &s, &mut budget(&s));
        frames += 1;
        if g.state(key) == Some(TileState::Resident) {
            assert!(plan.resident.contains(&key));
            break;
        }
        // Not drawn until complete.
        assert_eq!(g.state(key), Some(TileState::Applying));
        assert!(frames < 20);
    }
    assert_eq!(frames, 7, "50 000 elements at 8 000 per frame");
}

/// Audit wave 6 (stream H1): the memory cap released cells an anchor still wanted, and the next
/// step queued them again, so they were regenerated and released on every step. A capped cell now
/// stays out while it does not fit, the nearest cells stay resident, and it comes back once no
/// anchor wants it and one does again.
#[test]
fn t10b_the_memory_cap_does_not_regenerate_the_cells_it_released() {
    let mut s = settings(200.0, 1.25);
    s.max_resident_mb = Some(1);
    let mut g = one_tier(10_000.0);
    let here = [AnchorView::at(Vec3::new(32.0, 0.0, 32.0))];
    // Every cell holds 300 KiB: three fit under 1 MiB, the anchor wants far more.
    let finish = |g: &mut GeneratorTiles, plan: &regia_procgen::tiles::StepPlan| {
        for k in &plan.start {
            g.set_running(*k);
            g.finished(*k, 1, 300 << 10, [0; 32], 0.0);
        }
    };
    for _ in 0..10 {
        let plan = g.step(&here, &s, &mut ApplyBudget::default());
        finish(&mut g, &plan);
    }
    assert!(g.counters.memory_released > 0, "the cap was reached");
    let settled = (g.counters.started, g.counters.memory_released);
    for step in 0..40 {
        let plan = g.step(&here, &s, &mut ApplyBudget::default());
        assert!(
            plan.start.is_empty(),
            "step {step}: a released cell started again"
        );
        assert!(plan.release.is_empty(), "step {step}: a cell was released again");
        finish(&mut g, &plan);
    }
    assert_eq!((g.counters.started, g.counters.memory_released), settled);
    let resident: u64 = g.entries.values().map(|e| e.bytes).sum();
    assert!(resident <= 1 << 20, "{resident} bytes resident over a 1 MiB cap");
    let own = TileKey {
        tier: 0,
        cell: CellCoord::new(0, 0),
    };
    assert_eq!(
        g.state(own),
        Some(TileState::Resident),
        "the anchor's own cell stays"
    );
    // Away and back: the capped cells are forgotten and the anchor's cells load again.
    let away = [AnchorView::at(Vec3::new(9_000.0, 0.0, 9_000.0))];
    for _ in 0..3 {
        let plan = g.step(&away, &s, &mut ApplyBudget::default());
        finish(&mut g, &plan);
    }
    let plan = g.step(&here, &s, &mut ApplyBudget::default());
    assert!(
        plan.start.contains(&own),
        "back home, the anchor's cell starts again"
    );
}

/// An anchor that moves onto a cell the cap released gets that cell back: the cap releases a
/// farther resident cell instead, and the moved-to cell stays resident.
#[test]
fn t10d_an_anchor_moving_onto_a_capped_cell_loads_it() {
    let mut s = settings(200.0, 1.25);
    s.max_resident_mb = Some(1);
    let mut g = one_tier(10_000.0);
    let finish = |g: &mut GeneratorTiles, plan: &regia_procgen::tiles::StepPlan| {
        for k in &plan.start {
            g.set_running(*k);
            g.finished(*k, 1, 300 << 10, [0; 32], 0.0);
        }
    };
    let home = [AnchorView::at(Vec3::new(32.0, 0.0, 32.0))];
    for _ in 0..10 {
        let plan = g.step(&home, &s, &mut ApplyBudget::default());
        finish(&mut g, &plan);
    }
    let target = TileKey {
        tier: 0,
        cell: CellCoord::new(2, 0),
    };
    assert_eq!(g.state(target), None, "the cap released cell (2, 0)");
    let moved = [AnchorView::at(Vec3::new(160.0, 0.0, 32.0))];
    for _ in 0..10 {
        let plan = g.step(&moved, &s, &mut ApplyBudget::default());
        finish(&mut g, &plan);
    }
    assert_eq!(
        g.state(target),
        Some(TileState::Resident),
        "the cell under the anchor loads"
    );
    let resident: u64 = g.entries.values().map(|e| e.bytes).sum();
    assert!(resident <= 1 << 20, "{resident} bytes resident over a 1 MiB cap");
    let settled = g.counters.started;
    for step in 0..20 {
        let plan = g.step(&moved, &s, &mut ApplyBudget::default());
        assert!(plan.start.is_empty(), "step {step}: no churn once settled");
        finish(&mut g, &plan);
    }
    assert_eq!(g.counters.started, settled);
}

/// A single cell larger than the cap is released once and not regenerated while it is wanted.
#[test]
fn t10c_a_cell_larger_than_the_cap_is_not_regenerated_every_step() {
    let mut s = settings(10.0, 1.25);
    s.max_resident_mb = Some(1);
    let mut g = one_tier(10_000.0);
    let a = [AnchorView::at(Vec3::new(32.0, 0.0, 32.0))];
    for _ in 0..30 {
        let plan = g.step(&a, &s, &mut ApplyBudget::default());
        for k in &plan.start {
            g.set_running(*k);
            g.finished(*k, 1, 2 << 20, [0; 32], 0.0);
        }
    }
    assert_eq!(g.counters.started, 1, "generated once");
    assert_eq!(g.counters.memory_released, 1, "released once");
}

#[test]
fn t10_start_budgets_cap_new_and_running_cells() {
    let mut s = settings(400.0, 1.25);
    s.max_new_per_frame = Some(3);
    s.max_running = Some(5);
    let mut g = one_tier(10_000.0);
    let a = [AnchorView::at(Vec3::ZERO)];
    let p1 = g.step(&a, &s, &mut ApplyBudget::default());
    assert_eq!(p1.start.len(), 3);
    let p2 = g.step(&a, &s, &mut ApplyBudget::default());
    assert_eq!(p2.start.len(), 2, "5 running at most");
    // Priority: the anchor's own cells start first.
    assert!(p1.start.iter().all(|k| cell_distance(*k, Vec3::ZERO) == 0.0));
}

// ------------------------------------------------------------------------------------------------
// ECS: a tiled Baked generator through the lifecycle
// ------------------------------------------------------------------------------------------------

#[test]
fn ecs_tiled_generator_matches_its_untiled_run_and_records_cells() {
    use regia_app::prelude::*;
    use regia_procgen::tiles::{ProcGenTiles, TileSettings};
    use regia_procgen::world::{
        ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenerator, ProcGraphSource, ProcOutput, run_now,
    };
    let text = scatter_graph(5.0, 0.6, false).to_ron();
    let mut files = BTreeMap::new();
    files.insert("s.pgraph.ron".to_string(), text);
    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    let spawn = |app: &mut App, tiles: Option<TileSettings>| {
        app.world_mut()
            .spawn((
                ProcGenerator {
                    bounds: ProcBounds::Box {
                        half_extents: Vec3::new(150.0, 10.0, 100.0),
                    },
                    seed: Some(11),
                    tiles,
                    ..ProcGenerator::new("s.pgraph.ron")
                },
                regia_scene::Transform::default(),
            ))
            .id()
    };
    let plain = spawn(&mut app, None);
    let tiled_e = spawn(
        &mut app,
        Some(TileSettings {
            cell_m: 64.0,
            ..TileSettings::default()
        }),
    );
    run_now(app.world_mut(), plain);
    run_now(app.world_mut(), tiled_e);
    let out = |e: Entity| {
        app.world()
            .get::<ProcOutput>(e)
            .map(|o| instance_keys(&o.0))
            .unwrap_or_default()
    };
    let a = out(plain);
    assert!(a.len() > 1000, "{}", a.len());
    assert_eq!(a, out(tiled_e));
    let cells = app
        .world()
        .resource::<ProcGenTiles>()
        .generators
        .get(&tiled_e)
        .map_or(0, |g| g.entries.len());
    assert_eq!(cells, 6 * 4, "cells of a 300 m × 200 m box on a 64 m grid");
    // Audit M1: a tiled run reports its nodes, summed over the cells, not an empty run.
    let report = app
        .world()
        .get::<regia_procgen::world::ProcGenState>(tiled_e)
        .and_then(|s| s.report.clone())
        .expect("a report");
    let spawned = report.nodes.iter().map(|n| n.elements).max().unwrap_or(0);
    assert!(!report.nodes.is_empty());
    assert!(spawned >= a.len(), "{spawned} elements for {} instances", a.len());
    // An invalid base cell fails the run with `tile_cell_invalid`.
    let bad = spawn(
        &mut app,
        Some(TileSettings {
            cell_m: 100.0,
            ..TileSettings::default()
        }),
    );
    run_now(app.world_mut(), bad);
    let err = app
        .world()
        .get::<regia_procgen::world::ProcGenState>(bad)
        .and_then(|s| s.last_error.clone())
        .map(|e| e.1);
    assert_eq!(err.as_deref(), Some("tile_cell_invalid"));
}

// ------------------------------------------------------------------------------------------------
// 11. Baked tiled output
// ------------------------------------------------------------------------------------------------

#[test]
fn t11_rebaking_one_cell_keeps_the_other_cells_bytes_and_anchors_load_only_near_cells() {
    use regia_procgen::pgout::{PgoutHeader, decode, decode_cell, encode_cells, read_table, replace_cell};
    use regia_procgen::tiles::merge_outputs;
    let snap = snapshot(box_shape([0.0; 3], [512.0, 10.0, 512.0], 0.0), None);
    let inputs = RunInputs::new(snap, 4);
    let g = scatter_graph(8.0, 0.5, false);
    let t = tiled(&g, &inputs, 128.0, 4.0, 4);
    assert_eq!(t.cells.len(), 9 * 9);
    let header = PgoutHeader {
        graph_hash: [1; 32],
        param_hash: [2; 32],
        seed: 4,
        input_hash: [3; 32],
        output_hash: t.output_hash,
        quality: 5,
    };
    let cells: Vec<(u8, i32, i32, &RunOutput)> = t
        .cells
        .iter()
        .map(|(k, c)| (k.tier, k.cell.x as i32, k.cell.z as i32, c.output.as_ref()))
        .collect();
    let bytes = encode_cells(&header, &cells);
    let (h, merged) = decode(&bytes).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(h, header);
    assert_eq!(merged.hash(), t.output_hash);

    // Re-bake cell (1, -2) with different content: every other section is byte-identical.
    let (_, table) = read_table(&bytes).unwrap_or_else(|e| panic!("{e}"));
    let target = table
        .iter()
        .find(|c| (c.i, c.j) == (1, -2))
        .copied()
        .unwrap_or_else(|| panic!("no cell (1, -2)"));
    let mut changed = RunOutput::default();
    if let Some(set) = t
        .cells
        .values()
        .next()
        .and_then(|c| c.output.instance_sets.first())
    {
        let mut s = set.clone();
        s.instances.truncate(3);
        changed.instance_sets.push(s);
    }
    let rebaked =
        replace_cell(&bytes, target.tier, target.i, target.j, &changed).unwrap_or_else(|e| panic!("{e}"));
    let (h2, table2) = read_table(&rebaked).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(table2.len(), table.len());
    for (a, b) in table.iter().zip(&table2) {
        let sa = &bytes[a.offset as usize..(a.offset + a.len) as usize];
        let sb = &rebaked[b.offset as usize..(b.offset + b.len) as usize];
        if (a.i, a.j) == (1, -2) {
            assert_ne!(sa, sb);
        } else {
            assert_eq!(sa, sb, "cell ({}, {}) changed", a.i, a.j);
            assert_eq!(a.output_hash, b.output_hash);
        }
    }
    let (_, merged2) = decode(&rebaked).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(merged2.hash(), h2.output_hash);

    // Streaming baked cells with anchors: only the cells around the anchor are decoded.
    let s = settings(150.0, 1.25);
    let mut sched = GeneratorTiles::new(
        [-512.0, -512.0, 512.0, 512.0],
        vec![SchedTier {
            index: 0,
            cell_m: Some(128.0),
        }],
    );
    let anchor = [AnchorView::at(Vec3::new(300.0, 0.0, 300.0))];
    let mut s128 = s.clone();
    s128.tiers[0].cell_m = 128.0;
    let plan = sched.step(&anchor, &s128, &mut ApplyBudget::default());
    let mut loaded = 0usize;
    let mut near = Vec::new();
    for k in &plan.start {
        let c = table
            .iter()
            .find(|c| (c.tier, i64::from(c.i), i64::from(c.j)) == (k.tier, k.cell.x, k.cell.z))
            .unwrap_or_else(|| panic!("{k:?} not in the file"));
        let o = decode_cell(&bytes, c).unwrap_or_else(|e| panic!("{e}"));
        loaded += 1;
        near.push((*k, o));
    }
    assert!(
        loaded > 0 && loaded < table.len() / 4,
        "loaded {loaded} of {}",
        table.len()
    );
    // The loaded cells are exactly the ones of the full run.
    for (k, o) in &near {
        assert_eq!(o.hash(), t.cells[k].output_hash);
    }
    let _ = merge_outputs(near.iter().map(|(k, o)| (k, o)))
        .expect("the loaded cells of one bake agree on attribute types");
}

/// v0.5.1 (deep-zai-procgen-runtime finding 9): two cells that export the same point-set name with
/// an attribute of the same name but different types must not silently drop the second cell's
/// points from the merged (and baked) output: the merge refuses with `attr_type_conflict` naming
/// both cells and the attribute.
#[test]
fn conflicting_attribute_types_across_cells_fail_the_merge() {
    use regia_procgen::RunError;
    use regia_procgen::data::{AttrColumn, Point, PointSet};
    use regia_procgen::tiles::merge_outputs;
    let key = |i: i64| TileKey {
        tier: 0,
        cell: CellCoord::new(i, 0),
    };
    let cell = |x: i64, kind: AttrColumn| {
        let mut out = RunOutput::default();
        let mut points = PointSet::new();
        for k in 0..3 {
            points.push(Point {
                position: [x as f32 + k as f32, 0.0, 0.0],
                ..Point::default()
            });
        }
        points.attrs.insert("kind".to_string(), kind);
        out.points.insert("trees".to_string(), points);
        out
    };
    let a = cell(0, AttrColumn::Float(vec![1.0, 2.0, 3.0].into()));
    let b = cell(1, AttrColumn::Int(vec![4, 5, 6].into()));
    let err = merge_outputs([(&key(0), &a), (&key(1), &b)])
        .expect_err("cells that disagree on an attribute's type fail the merge");
    let RunError::Node { code, message, .. } = &err else {
        panic!("a merge failure is a node error: {err:?}");
    };
    assert_eq!(code, "attr_type_conflict");
    assert!(
        message.contains("kind") && message.contains("trees"),
        "the error names the attribute and the point set: {message}"
    );
    assert_eq!(
        err.to_string(),
        "node 0: attr_type_conflict: cells tier 0 cell (0, 0) and tier 0 cell (1, 0) give point \
         set \"trees\" an attribute \"kind\" of different types",
        "the error names both cells and the attribute"
    );
    // The same-schema cells still merge (the first cell's points are not dropped either way).
    let c = cell(1, AttrColumn::Float(vec![4.0, 5.0, 6.0].into()));
    let merged = merge_outputs([(&key(0), &a), (&key(1), &c)]).expect("matching schemas merge");
    assert_eq!(merged.points["trees"].len(), 6);
}

/// v0.5.1 (deep-zai-procgen-runtime finding 9, bake path): a baked tiled output whose cells
/// disagree on an attribute's type fails to decode with `attr_type_conflict` instead of silently
/// dropping the second cell's points.
#[test]
fn decoding_a_bake_with_conflicting_attribute_types_fails() {
    use regia_procgen::data::{AttrColumn, Point, PointSet};
    use regia_procgen::pgout::{PgoutHeader, decode, encode_cells};
    let cell = |x: i64, kind: AttrColumn| {
        let mut out = RunOutput::default();
        let mut points = PointSet::new();
        for k in 0..3 {
            points.push(Point {
                position: [x as f32 + k as f32, 0.0, 0.0],
                ..Point::default()
            });
        }
        points.attrs.insert("kind".to_string(), kind);
        out.points.insert("trees".to_string(), points);
        out
    };
    let a = cell(0, AttrColumn::Float(vec![1.0, 2.0, 3.0].into()));
    let b = cell(1, AttrColumn::Int(vec![4, 5, 6].into()));
    let header = PgoutHeader {
        graph_hash: [1; 32],
        param_hash: [2; 32],
        seed: 4,
        input_hash: [3; 32],
        output_hash: [9; 32],
        quality: 5,
    };
    let bytes = encode_cells(&header, &[(0, 0, 0, &a), (0, 1, 0, &b)]);
    let err = decode(&bytes).expect_err("conflicting cells refuse to decode");
    assert_eq!(err.code, "attr_type_conflict");
    assert!(
        err.message.contains("kind") && err.message.contains("trees"),
        "the error names the attribute and the point set: {}",
        err.message
    );
}

/// Baked streaming without a file settles on one error, a bake clears it, and a file from another seed
/// is served marked stale, as a whole bake is on load.
#[test]
fn baked_streaming_reports_a_missing_or_outdated_file() {
    use regia_app::prelude::*;
    use regia_procgen::tiles::{AnchorSource, ProcGenTiles, TileSettings};
    use regia_procgen::world::{
        ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenState, ProcGenerator, ProcGraphSource, bake,
    };
    use regia_scene::components::EntityGuid;
    let dir = regia_test_utils::unique_temp_dir("procgen-stream-missing");
    let text = scatter_graph(5.0, 0.6, false).to_ron();
    let mut files = BTreeMap::new();
    files.insert("s.pgraph.ron".to_string(), text);
    let app = |seed: u32| {
        let mut app = App::new();
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(files.clone()),
            bake_dir: Some(dir.clone()),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        let e = app
            .world_mut()
            .spawn((
                ProcGenerator {
                    bounds: ProcBounds::Box {
                        half_extents: Vec3::new(128.0, 10.0, 128.0),
                    },
                    seed: Some(seed),
                    tiles: Some(TileSettings {
                        cell_m: 64.0,
                        runtime: Some(AnchorSettings {
                            anchors: AnchorSource::Tagged,
                            base_reach_m: 80.0,
                            ..AnchorSettings::default()
                        }),
                        ..TileSettings::default()
                    }),
                    ..ProcGenerator::new("s.pgraph.ron")
                },
                EntityGuid(regia_core::id::Guid(0x52)),
                regia_scene::Transform::default(),
                regia_scene::GlobalTransform::default(),
            ))
            .id();
        (app, e)
    };
    let code = |app: &App, e| {
        app.world()
            .resource::<ProcGenTiles>()
            .errors
            .get(&e)
            .map(|(c, _)| c.clone())
    };
    let (mut first, e) = app(11);
    first.update();
    first.update();
    assert_eq!(code(&first, e).as_deref(), Some("procgen_bake_missing"));
    bake(first.world_mut(), e).unwrap_or_else(|(c, m)| panic!("{c}: {m}"));
    assert_eq!(code(&first, e), None, "a bake ends the wait");
    first.update();
    assert_eq!(code(&first, e), None);
    assert!(!first.world().get::<ProcGenState>(e).is_some_and(|s| s.stale));

    let (mut other, e) = app(12);
    other.update();
    other.update();
    assert_eq!(code(&other, e), None, "the file is still served");
    assert!(other.world().get::<ProcGenState>(e).is_some_and(|s| s.stale));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spec 66 § 4.3 "Baked + anchors", § 8 test 11: a baked tiled generator with anchors loads only the
/// cells around the anchor from its file, evaluates nothing, and each loaded cell equals the bake's.
#[test]
fn baked_cells_stream_around_an_anchor_without_evaluating() {
    use regia_app::prelude::*;
    use regia_procgen::tiles::{AnchorSource, ProcGenAnchor, ProcGenTileHost, ProcGenTiles, TileSettings};
    use regia_procgen::world::{
        ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenState, ProcGenerator, ProcGraphSource, ProcOutput,
        bake, bake_path, verify,
    };
    use regia_scene::components::{EntityGuid, Generated};
    let dir = regia_test_utils::unique_temp_dir("procgen-stream");
    let text = scatter_graph(5.0, 0.6, false).to_ron();
    let mut files = BTreeMap::new();
    files.insert("s.pgraph.ron".to_string(), text);
    let settings = || ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files.clone()),
        bake_dir: Some(dir.clone()),
        ..ProcGenSettings::default()
    };
    let generator = |runtime: Option<AnchorSettings>| ProcGenerator {
        bounds: ProcBounds::Box {
            half_extents: Vec3::new(256.0, 10.0, 256.0),
        },
        seed: Some(11),
        tiles: Some(TileSettings {
            cell_m: 64.0,
            runtime,
            ..TileSettings::default()
        }),
        ..ProcGenerator::new("s.pgraph.ron")
    };
    let guid = EntityGuid(regia_core::id::Guid(0x51));

    // Bake with the generator streaming: no output is published, the file holds every cell.
    let mut app = App::new();
    app.insert_resource(settings());
    app.add_plugins(ProcGenPlugin);
    let reach = AnchorSettings {
        anchors: AnchorSource::Tagged,
        base_reach_m: 80.0,
        ..AnchorSettings::default()
    };
    let e = app
        .world_mut()
        .spawn((
            generator(Some(reach.clone())),
            guid,
            regia_scene::Transform::default(),
        ))
        .id();
    bake(app.world_mut(), e).expect("bake");
    assert!(
        app.world().get::<ProcOutput>(e).is_none(),
        "a streamed bake publishes nothing"
    );
    let bytes = std::fs::read(bake_path(&dir, guid.0)).expect("file");
    let (_, table) = regia_procgen::pgout::read_table(&bytes).expect("table");
    assert!(table.len() >= 64, "{} cells", table.len());
    assert!(
        verify(app.world_mut(), e).expect("verify").ok,
        "verify runs the tiles"
    );
    // An anchor in the corner of the world: one spot, few cells.
    app.world_mut().spawn((
        ProcGenAnchor::default(),
        regia_scene::Transform::from_translation(Vec3::new(-200.0, 0.0, -200.0)),
    ));
    for _ in 0..8 {
        app.update();
    }
    let tiles = app.world().resource::<ProcGenTiles>();
    let g = tiles.generators.get(&e).expect("scheduled");
    let resident: Vec<TileKey> = g
        .entries
        .iter()
        .filter(|(_, en)| en.state == TileState::Resident)
        .map(|(k, _)| *k)
        .collect();
    let loaded = g.counters.cells_loaded;
    assert!(!resident.is_empty());
    assert!(
        (loaded as usize) < table.len() / 4,
        "loaded {loaded} of {} cells",
        table.len()
    );
    assert_eq!(loaded as usize, resident.len());
    let state = app.world().get::<ProcGenState>(e).expect("state");
    let evaluations = state.evaluations;
    assert_eq!(evaluations, 0, "no evaluation at runtime");
    // Each resident cell equals its section in the file.
    let host = app.world().get::<ProcGenTileHost>(e).expect("host");
    for k in &resident {
        let c = host.cell(*k).expect("cell");
        let entry = table
            .iter()
            .find(|t| (t.tier, i64::from(t.i), i64::from(t.j)) == (k.tier, k.cell.x, k.cell.z))
            .expect("in the table");
        assert_eq!(c.output_hash, entry.output_hash);
    }
    let shown = app
        .world_mut()
        .query_filtered::<&ProcOutput, With<Generated>>()
        .iter(app.world())
        .count();
    assert_eq!(shown, resident.len(), "one output entity per resident cell");

    // Without a bake the host reports it instead of evaluating.
    let mut empty = App::new();
    empty.insert_resource(ProcGenSettings {
        bake_dir: Some(dir.join("elsewhere")),
        ..settings()
    });
    empty.add_plugins(ProcGenPlugin);
    let e2 = empty
        .world_mut()
        .spawn((generator(Some(reach)), guid, regia_scene::Transform::default()))
        .id();
    empty.update();
    let tiles = empty.world().resource::<ProcGenTiles>();
    assert_eq!(
        tiles.errors.get(&e2).map(|(c, _)| c.as_str()),
        Some("procgen_bake_missing")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spec 66 § 7: the overlay lists every tier's cells with their states and the anchor circles.
#[test]
fn the_debug_overlay_shows_cells_by_state_and_anchor_circles() {
    use regia_app::prelude::*;
    use regia_procgen::debug_view::tile_overlay;
    use regia_procgen::tiles::{AnchorSource, ProcGenAnchor, TileSettings};
    use regia_procgen::world::{ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenerator, ProcGraphSource};
    let text = scatter_graph(5.0, 0.6, false).to_ron();
    let mut files = BTreeMap::new();
    files.insert("s.pgraph.ron".to_string(), text);
    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.world_mut().spawn((
        ProcGenerator {
            mode: regia_procgen::world::ProcGenMode::Anchored,
            bounds: ProcBounds::Box {
                half_extents: Vec3::new(256.0, 10.0, 256.0),
            },
            seed: Some(11),
            tiles: Some(TileSettings {
                cell_m: 64.0,
                runtime: Some(AnchorSettings {
                    anchors: AnchorSource::Tagged,
                    base_reach_m: 80.0,
                    max_new_per_frame: Some(2),
                    ..AnchorSettings::default()
                }),
                ..TileSettings::default()
            }),
            ..ProcGenerator::new("s.pgraph.ron")
        },
        regia_scene::Transform::default(),
    ));
    app.world_mut().spawn((
        ProcGenAnchor::default(),
        regia_scene::Transform::from_translation(Vec3::new(10.0, 0.0, 10.0)),
    ));
    app.update();
    let overlay = tile_overlay(app.world_mut());
    assert_eq!(overlay.len(), 1);
    let o = &overlay[0];
    assert_eq!(o.bounds, [-256.0, -256.0, 256.0, 256.0]);
    assert_eq!(o.circles.len(), 1);
    assert_eq!(o.circles[0].1, 80.0);
    assert!((o.circles[0].2 - 100.0).abs() < 1e-3, "release at 1.25 × build");
    assert!(
        o.cells
            .iter()
            .any(|c| c.state == TileState::Queued && c.priority.is_some()),
        "queued cells carry their priority"
    );
    assert!(
        o.cells.iter().any(|c| c.state != TileState::Queued),
        "started cells"
    );
    let w = o.cells[0].rect[2] - o.cells[0].rect[0];
    assert_eq!(w, 64.0, "cell rectangles in meters");
}

/// Spec 78 § 7.4: with a streamed level open its sources are anchors, in local coordinates, and no
/// streamed level means no change.
#[test]
fn streaming_sources_are_anchors_in_local_coordinates() {
    use regia_procgen::tiles::{AnchorSource, gather_anchors};
    use regia_scene::stream_plan::{SourceId, SourceView};
    use regia_scene::streaming::{OriginOffset, StreamViews};
    let mut world = regia_ecs::prelude::World::new();
    assert!(gather_anchors(&mut world, AnchorSource::PrimaryView).is_empty());
    world.insert_resource(StreamViews(Vec::new()));
    assert!(gather_anchors(&mut world, AnchorSource::PrimaryView).is_empty());

    let mut far = SourceView::at(SourceId::Pin(1), [1034.0, 2.0, -2038.0]);
    far.radius_scale = 2.0;
    world.insert_resource(StreamViews(vec![far]));
    world.insert_resource(OriginOffset([1024.0, 0.0, -2048.0]));
    let anchors = gather_anchors(&mut world, AnchorSource::PrimaryView);
    assert_eq!(anchors.len(), 1);
    assert_eq!(anchors[0].pos.to_array(), [10.0, 2.0, 10.0]);
    assert_eq!(anchors[0].reach_scale, 2.0);
}

/// v0.5.1 (deep-zai-procgen-runtime finding 8): a baked-streaming generator that found no file in
/// the first level's bake directory opens the bake once the host moves the directory (a level
/// change); the `procgen_bake_missing` latch used to hold until an explicit bake.
#[test]
fn a_new_bake_directory_clears_the_missing_bake_latch() {
    use regia_app::prelude::*;
    use regia_procgen::tiles::{AnchorSource, ProcGenAnchor, ProcGenTiles, TileSettings};
    use regia_procgen::world::{
        ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenerator, ProcGraphSource, bake,
    };
    use regia_scene::components::EntityGuid;
    let root = regia_test_utils::unique_temp_dir("procgen-stream-latch");
    let (baked, empty) = (root.join("cave.procgen"), root.join("start.procgen"));
    let mut files = BTreeMap::new();
    files.insert(
        "s.pgraph.ron".to_string(),
        scatter_graph(5.0, 0.6, false).to_ron(),
    );
    let app = |dir: &std::path::Path| {
        let mut app = App::new();
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(files.clone()),
            bake_dir: Some(dir.to_path_buf()),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        let e = app
            .world_mut()
            .spawn((
                ProcGenerator {
                    bounds: ProcBounds::Box {
                        half_extents: Vec3::new(128.0, 10.0, 128.0),
                    },
                    seed: Some(11),
                    tiles: Some(TileSettings {
                        cell_m: 64.0,
                        runtime: Some(AnchorSettings {
                            anchors: AnchorSource::Tagged,
                            base_reach_m: 80.0,
                            ..AnchorSettings::default()
                        }),
                        ..TileSettings::default()
                    }),
                    ..ProcGenerator::new("s.pgraph.ron")
                },
                EntityGuid(regia_core::id::Guid(0x53)),
                regia_scene::Transform::default(),
                regia_scene::GlobalTransform::default(),
            ))
            .id();
        (app, e)
    };
    let (mut tool, e) = app(&baked);
    bake(tool.world_mut(), e).unwrap_or_else(|(c, m)| panic!("{c}: {m}"));

    let (mut game, e) = app(&empty);
    game.world_mut().spawn((
        ProcGenAnchor::default(),
        regia_scene::Transform::from_translation(Vec3::new(0.0, 0.0, 0.0)),
    ));
    game.update();
    game.update();
    let code = |app: &App| {
        app.world()
            .resource::<ProcGenTiles>()
            .errors
            .get(&e)
            .map(|(c, _)| c.clone())
    };
    assert_eq!(code(&game).as_deref(), Some("procgen_bake_missing"));
    game.world_mut().resource_mut::<ProcGenSettings>().bake_dir = Some(baked.clone());
    for _ in 0..6 {
        game.update();
    }
    assert_eq!(code(&game), None, "the new directory's bake is open");
    let tiles = game.world().resource::<ProcGenTiles>();
    let loaded = tiles.generators.get(&e).map_or(0, |g| g.counters.cells_loaded);
    assert!(loaded > 0, "cells stream from the new directory's bake");
    let _ = std::fs::remove_dir_all(&root);
}

/// v0.5.1 (deep-zai-procgen-runtime finding 8, remainder): while the bake directory stays the
/// same, a bake file that appears after the generator reported `procgen_bake_missing` (an external
/// copy, not `procgen.bake`) is opened on a later update: the latch clears and the cells stream.
#[test]
fn the_missing_bake_latch_clears_when_the_file_appears_in_the_same_directory() {
    use regia_app::prelude::*;
    use regia_procgen::tiles::{AnchorSource, ProcGenAnchor, ProcGenTiles, TileSettings};
    use regia_procgen::world::{
        ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenerator, ProcGraphSource, bake,
    };
    use regia_scene::components::EntityGuid;
    let root = regia_test_utils::unique_temp_dir("procgen-stream-latch-same-dir");
    let (stage, live) = (root.join("stage.procgen"), root.join("live.procgen"));
    std::fs::create_dir_all(&live).expect("the watched directory exists, empty");
    let mut files = BTreeMap::new();
    files.insert(
        "s.pgraph.ron".to_string(),
        scatter_graph(5.0, 0.6, false).to_ron(),
    );
    let (mut baker_app, baker_e) = {
        let mut app = App::new();
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(files.clone()),
            bake_dir: Some(stage.clone()),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        let e = app
            .world_mut()
            .spawn((
                ProcGenerator {
                    bounds: ProcBounds::Box {
                        half_extents: Vec3::new(128.0, 10.0, 128.0),
                    },
                    seed: Some(11),
                    tiles: Some(TileSettings {
                        cell_m: 64.0,
                        ..TileSettings::default()
                    }),
                    ..ProcGenerator::new("s.pgraph.ron")
                },
                EntityGuid(regia_core::id::Guid(0x53)),
                regia_scene::Transform::default(),
                regia_scene::GlobalTransform::default(),
            ))
            .id();
        (app, e)
    };
    bake(baker_app.world_mut(), baker_e).unwrap_or_else(|(c, m)| panic!("{c}: {m}"));

    let (mut game, e) = {
        let mut app = App::new();
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(files),
            bake_dir: Some(live.clone()),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        let e = app
            .world_mut()
            .spawn((
                ProcGenerator {
                    bounds: ProcBounds::Box {
                        half_extents: Vec3::new(128.0, 10.0, 128.0),
                    },
                    seed: Some(11),
                    tiles: Some(TileSettings {
                        cell_m: 64.0,
                        runtime: Some(AnchorSettings {
                            anchors: AnchorSource::Tagged,
                            base_reach_m: 80.0,
                            ..AnchorSettings::default()
                        }),
                        ..TileSettings::default()
                    }),
                    ..ProcGenerator::new("s.pgraph.ron")
                },
                EntityGuid(regia_core::id::Guid(0x53)),
                regia_scene::Transform::default(),
                regia_scene::GlobalTransform::default(),
            ))
            .id();
        (app, e)
    };
    game.world_mut().spawn((
        ProcGenAnchor::default(),
        regia_scene::Transform::from_translation(Vec3::new(0.0, 0.0, 0.0)),
    ));
    game.update();
    game.update();
    let code = |app: &App| {
        app.world()
            .resource::<ProcGenTiles>()
            .errors
            .get(&e)
            .map(|(c, _)| c.clone())
    };
    assert_eq!(
        code(&game).as_deref(),
        Some("procgen_bake_missing"),
        "the directory holds no bake yet"
    );
    // The file appears in the SAME directory (an external copy; `procgen.bake` was not called).
    let baked: Vec<std::path::PathBuf> = std::fs::read_dir(&stage)
        .map(|d| {
            d.filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default();
    assert!(!baked.is_empty(), "the stage directory holds the bake");
    for from in baked {
        let to = live.join(from.file_name().unwrap_or_default());
        std::fs::copy(&from, &to).expect("external copy");
    }
    for _ in 0..6 {
        game.update();
    }
    assert_eq!(
        code(&game),
        None,
        "the bake that appeared in the same directory is open"
    );
    let tiles = game.world().resource::<ProcGenTiles>();
    let loaded = tiles.generators.get(&e).map_or(0, |g| g.counters.cells_loaded);
    assert!(loaded > 0, "cells stream from the appeared bake");
    let _ = std::fs::remove_dir_all(&root);
}
