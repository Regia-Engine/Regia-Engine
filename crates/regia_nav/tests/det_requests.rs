// SPDX-License-Identifier: MPL-2.0
//! Large-map checks of the deterministic grid search (ADR-8295): clusters of 16 and 32 on a
//! 4096 x 4096 canvas against a flat A* reference, and the request-queue perf probe (ignored; run
//! it by hand for evidence, never as a timing assertion).

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;

use regia_detsim::golden::scenarios::SplitMix64;
use regia_nav::det::grid::{
    COST_FLAT, GridSettings, MoveClass, NEIGHBOURS_8, NavGrid, NavStatic, Profile, ProfileDef, SizeClass,
    Stamp,
};
use regia_nav::det::requests::{PathRequests, RequestSettings};
use regia_nav::det::search::{self, Plan, PlanKind, Scratch};

/// The documented bound (book page "Large maps: cluster size"): a full plan's cell path costs at
/// most the flat optimum plus 1/20 (5 %), and exactly the optimum on open ground.
const SLACK_DIVISOR: u64 = 20;

/// A 4096 x 4096 canvas of 5 m cells, one walking profile, at cluster size `k`: long closed walls
/// with gaps (every 512 cells, both axes) and seeded obstacle discs.
fn big(k: usize) -> NavGrid {
    let side = 4096usize;
    let settings = Arc::new(GridSettings {
        cell_m: 5.0,
        canvas_cells: [side, side],
        move_classes: vec![MoveClass::GROUND],
        size_classes: vec![SizeClass::SMALL],
        profiles: vec![ProfileDef {
            move_class: 0,
            size_class: 0,
        }],
        cluster_cells: k,
        areas: Vec::new(),
    });
    let stat = Arc::new(NavStatic::build(None, settings).expect("static"));
    let mut closed = vec![0u8; side * side];
    let mut rng = SplitMix64::new(0x5eed_4096);
    for wall in (512..side).step_by(512) {
        let gaps: Vec<usize> = (0..3)
            .map(|_| (rng.next_u64() % (side as u64 - 16)) as usize)
            .collect();
        for t in 0..side {
            if gaps.iter().any(|g| (*g..*g + 12).contains(&t)) {
                continue;
            }
            closed[t * side + wall] = 1; // a wall along Z at x = wall
            if !gaps.iter().any(|g| (*g..*g + 12).contains(&(side - 1 - t))) {
                closed[wall * side + t] = 1; // a wall along X at z = wall
            }
        }
    }
    let mut grid = NavGrid::with_closed_cells(stat, closed).expect("grid");
    let half = side as f32 * 5.0 * 0.5;
    let discs: Vec<(u32, Stamp)> = (0..4000u32)
        .map(|id| {
            let x = rng.signed() * half;
            let z = rng.signed() * half;
            let r = 4.0 + rng.unit() * 40.0;
            (id, Stamp::from_world(x, z, r))
        })
        .collect();
    grid.sync_stamps(discs);
    grid
}

/// The cost of one step under the search's rules, `None` when the step is not allowed: the target
/// passable, the terrain step open, no corner cutting.
fn step_cost(grid: &NavGrid, p: Profile, cell: u32, dx: i32, dz: i32) -> Option<(u32, u32)> {
    let (x, z) = grid.xz(cell);
    let (nx, nz) = (x as i32 + dx, z as i32 + dz);
    if nx < 0 || nz < 0 || nx >= grid.width() as i32 || nz >= grid.height() as i32 {
        return None;
    }
    let next = grid.index(nx as usize, nz as usize);
    if !grid.passable(p, next) || !grid.can_step(p, cell, dx, dz) {
        return None;
    }
    let diagonal = dx != 0 && dz != 0;
    if diagonal {
        let a = grid.index(nx as usize, z);
        let b = grid.index(x, nz as usize);
        if !grid.passable(p, a)
            || !grid.passable(p, b)
            || !grid.can_step(p, cell, dx, 0)
            || !grid.can_step(p, cell, 0, dz)
        {
            return None;
        }
    }
    let base = if diagonal { 14 } else { 10 };
    Some((next, base * grid.cost(p, next) / COST_FLAT))
}

/// Flat A* over every cell with the search's step rules: the optimal cost from `a` to `b`.
fn flat_optimum(grid: &NavGrid, p: Profile, a: u32, b: u32) -> Option<u64> {
    let n = grid.width() * grid.height();
    let mut g = vec![u32::MAX; n];
    let mut closed = vec![false; n];
    let mut heap = BinaryHeap::new();
    g[a as usize] = 0;
    heap.push(Reverse((search::octile(grid, a, b), a)));
    while let Some(Reverse((_, cell))) = heap.pop() {
        if closed[cell as usize] {
            continue;
        }
        closed[cell as usize] = true;
        if cell == b {
            return Some(u64::from(g[cell as usize]));
        }
        let gc = g[cell as usize];
        for (dx, dz) in NEIGHBOURS_8 {
            let Some((next, c)) = step_cost(grid, p, cell, dx, dz) else {
                continue;
            };
            let ng = gc + c;
            if ng < g[next as usize] {
                g[next as usize] = ng;
                heap.push(Reverse((ng + search::octile(grid, next, b), next)));
            }
        }
    }
    None
}

/// The cost of a plan's cell path, checking every step is legal.
fn path_cost(grid: &NavGrid, p: Profile, plan: &Plan) -> u64 {
    let mut total = 0u64;
    for w in plan.cells.windows(2) {
        let (ax, az) = grid.xz(w[0]);
        let (bx, bz) = grid.xz(w[1]);
        let (dx, dz) = (bx as i32 - ax as i32, bz as i32 - az as i32);
        let (next, c) = step_cost(grid, p, w[0], dx, dz).expect("every step of the cell path is legal");
        assert_eq!(next, w[1]);
        total += u64::from(c);
    }
    total
}

/// Polyline length of the waypoints in tenths of a cell.
fn polyline_tenths(grid: &NavGrid, plan: &Plan) -> f64 {
    let cell = f64::from(grid.cell_m());
    plan.waypoints
        .windows(2)
        .map(|w| {
            let (dx, dz) = (f64::from(w[1][0] - w[0][0]), f64::from(w[1][1] - w[0][1]));
            (dx * dx + dz * dz).sqrt() / cell * 10.0
        })
        .sum()
}

/// ADR-8295 acceptance: on a 4096 x 4096 canvas with walls and 4,000 discs, clusters of 16 and 32
/// plan full routes whose cell paths cost the flat A* optimum within the documented bound (exactly
/// on open ground), and string pulling never makes a route longer than its cell path.
#[test]
fn cluster_16_and_32_plan_equal_cost_paths_on_a_4k_canvas() {
    let p = Profile(0);
    for k in [16usize, 32] {
        let grid = big(k);
        assert_eq!(grid.cluster_cells(), k);
        assert_eq!(grid.clusters_x, 4096 / k);
        let mut scratch = Scratch::default();
        let mut rng = SplitMix64::new(0x5eed_0016 + k as u64);
        let (mut full, mut exact, mut worst) = (0u32, 0u32, 0.0f64);
        // Open ground: the first 61 x 21 box of passable cells along a diagonal scan; its corners
        // are an octile route apart.
        let open_box = (0..4000usize)
            .step_by(37)
            .map(|t| (t, (t * 7) % 4000))
            .find(|&(x0, z0)| {
                (z0..=z0 + 20).all(|z| (x0..=x0 + 60).all(|x| grid.passable(p, grid.index(x, z))))
            })
            .expect("an open box");
        let (oa, ob) = (
            grid.index(open_box.0, open_box.1),
            grid.index(open_box.0 + 60, open_box.1 + 20),
        );
        let open = search::plan(&grid, &mut scratch, p, grid.center(oa), grid.center(ob));
        for _ in 0..8 {
            let from = [rng.signed() * 9000.0, rng.signed() * 9000.0];
            let ang = rng.unit() * std::f32::consts::TAU;
            let len = 1500.0 + rng.unit() * 3500.0;
            let to = [from[0] + len * ang.cos(), from[1] + len * ang.sin()];
            let Some(plan) = search::plan(&grid, &mut scratch, p, from, to) else {
                continue;
            };
            if plan.kind != PlanKind::Full {
                println!("k {k}: {from:?} -> {to:?} is {:?}", plan.kind);
                continue;
            }
            full += 1;
            let (a, b) = (plan.cells[0], *plan.cells.last().expect("cells"));
            let cost = path_cost(&grid, p, &plan);
            let best = flat_optimum(&grid, p, a, b).expect("the reference reaches the goal");
            assert!(cost >= best, "k {k}: {cost} under the optimum {best}");
            assert!(
                cost * SLACK_DIVISOR <= best * (SLACK_DIVISOR + 1),
                "k {k}: {cost} vs optimum {best} ({from:?} -> {to:?})"
            );
            exact += u32::from(cost == best);
            worst = worst.max(cost as f64 / best as f64);
            // String pulling: never longer than the cell path (half a cell of slack for the
            // start and goal points off the cell centers).
            assert!(polyline_tenths(&grid, &plan) <= cost as f64 + 10.0);
        }
        let open = open.expect("open route");
        assert_eq!(open.kind, PlanKind::Full);
        assert_eq!((open.cells[0], *open.cells.last().expect("cells")), (oa, ob));
        let octile = u64::from(search::octile(&grid, oa, ob));
        assert_eq!(path_cost(&grid, p, &open), octile, "k {k}: open ground is exact");
        assert_eq!(flat_optimum(&grid, p, oa, ob), Some(octile));
        println!("k {k}: {full} full of 8, {exact} exact, worst ratio {worst:.4}");
        assert!(full >= 6, "k {k}: most long routes are full ({full} of 8)");
    }
}

/// Perf evidence (not a test of timing): 1,000 requests submitted in one tick on 256 x 256 and
/// 4096 x 4096, budget on and off. Prints one line per case. Run with
/// `cargo test --release -p regia_nav --test det_requests -- --ignored --nocapture`.
#[test]
#[ignore = "perf probe: run by hand, numbers go to an evidence file"]
fn perf_thousand_requests_in_one_tick() {
    for (side, k) in [(256usize, 8usize), (4096, 8), (4096, 32)] {
        let grid = if side == 4096 {
            big(k)
        } else {
            NavGrid::new(Arc::new(
                NavStatic::build(
                    None,
                    Arc::new(GridSettings {
                        move_classes: vec![MoveClass::GROUND],
                        size_classes: vec![SizeClass::SMALL],
                        profiles: vec![ProfileDef {
                            move_class: 0,
                            size_class: 0,
                        }],
                        ..GridSettings::default()
                    }),
                )
                .expect("static"),
            ))
            .expect("grid")
        };
        let half = side as f32 * 5.0 * 0.5 * 0.95;
        for (layout, budget) in [
            ("scattered", true),
            ("scattered", false),
            ("squads", true),
            ("squads", false),
        ] {
            let mut settings = RequestSettings::for_grid(&grid);
            if !budget {
                settings.tick_budget = None;
            }
            let mut queue = PathRequests::new(settings);
            let mut scratch = Scratch::default();
            let mut rng = SplitMix64::new(0x5eed_9e7f);
            // Ten squads of 100 units; each squad shares a goal area. "scattered": every unit starts
            // anywhere on the map; "squads": a squad starts within 15 m of its rally point, as after
            // an RTS order to a group.
            let rally: Vec<[f32; 2]> = (0..10)
                .map(|_| [rng.signed() * half, rng.signed() * half])
                .collect();
            for unit in 0..1000u32 {
                let squad = (unit / 100) as usize;
                let from = if layout == "squads" {
                    [
                        rally[squad][0] + rng.signed() * 15.0,
                        rally[squad][1] + rng.signed() * 15.0,
                    ]
                } else {
                    [rng.signed() * half, rng.signed() * half]
                };
                let to = [(squad as f32 - 4.5) * half / 5.0, half * 0.5];
                queue.request(unit, from, to, Profile(0));
            }
            let start = std::time::Instant::now();
            queue.serve(&grid, &mut scratch);
            let first = start.elapsed();
            let mut ticks = 1u32;
            while queue.pending() > 0 {
                queue.serve(&grid, &mut scratch);
                ticks += 1;
            }
            let total = start.elapsed();
            let c = queue.counters();
            println!(
                "perf side {side} cluster {k} {layout} budget {} first_tick_ms {:.2} total_ms {:.2} ticks {ticks} served {} cache_hits {} expansions {} coarse {} work {}",
                if budget { "on" } else { "off" },
                first.as_secs_f64() * 1e3,
                total.as_secs_f64() * 1e3,
                c.served,
                c.cache_hits,
                c.expansions,
                c.coarse_expansions,
                c.work
            );
        }
    }
}
