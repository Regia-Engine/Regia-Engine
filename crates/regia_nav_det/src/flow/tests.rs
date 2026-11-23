// SPDX-License-Identifier: MPL-2.0
//! Flow field tests: the integration field against a brute-force reference, descent, unreachable
//! cells, nearest goals, the clustered flow against a full field, incremental rebuilds, the cache,
//! sampling, and the determinism pin.

use std::sync::Arc;

use regia_detsim::golden::scenarios::SplitMix64;
use regia_detsim::golden::{self, DetRun, DetScenario, GoldenMode, REPORT_PREFIX, StateHasher};
use regia_detsim::tick::TickRate;

use super::*;
use crate::grid::{CLUSTER, GridSettings, HeightCells, NavStatic, Stamp};

/// The preset with a flat canvas of `w` by `h` cells (5 m).
fn canvas(w: usize, h: usize) -> Arc<GridSettings> {
    Arc::new(GridSettings {
        canvas_cells: [w, h],
        ..GridSettings::hover_ground_jet()
    })
}

fn flat(w: usize, h: usize) -> NavGrid {
    NavGrid::new(Arc::new(NavStatic::build(None, canvas(w, h)).expect("canvas"))).expect("grid")
}

/// A heightfield of `cols` by `rows` 5 m samples from a function of the sample (x, z), heights
/// quantized to 1/32 m so every target reads the same bits.
fn ground(cols: usize, rows: usize, h: impl Fn(usize, usize) -> f32) -> NavGrid {
    let mut heights = Vec::with_capacity(cols * rows);
    for z in 0..rows {
        for x in 0..cols {
            heights.push((h(x, z) * 32.0).round() / 32.0);
        }
    }
    let cells = HeightCells::centered(cols, rows, 5.0, heights);
    let settings = Arc::new(GridSettings::hover_ground_jet());
    NavGrid::new(Arc::new(
        NavStatic::build(Some(&cells), settings).expect("ground"),
    ))
    .expect("grid")
}

/// Seeded rough ground: a few bumps and ridges, so slopes give costs above flat, some cliffs block
/// and each movement class differs.
fn rough(cols: usize, rows: usize, seed: u64) -> NavGrid {
    let mut rng = SplitMix64::new(seed);
    let bumps: Vec<(f32, f32, f32, f32)> = (0..6)
        .map(|_| {
            (
                rng.unit() * cols as f32,
                rng.unit() * rows as f32,
                2.0 + rng.unit() * 5.0,
                rng.signed() * 14.0,
            )
        })
        .collect();
    let ridge_x = (rng.next_u64() % cols as u64) as usize;
    let gap = (rng.next_u64() % rows as u64) as usize;
    ground(cols, rows, move |x, z| {
        let mut h = 0.0;
        for (bx, bz, r, a) in &bumps {
            let (dx, dz) = (x as f32 - bx, z as f32 - bz);
            let d2 = (dx * dx + dz * dz) / (r * r);
            if d2 < 1.0 {
                h += a * (1.0 - d2);
            }
        }
        if x == ridge_x && z.abs_diff(gap) > 2 {
            h += 40.0;
        }
        h
    })
}

/// Random obstacle discs (ids 1..), most of them small.
fn stamps(grid: &NavGrid, rng: &mut SplitMix64, count: u32) -> Vec<(u32, Stamp)> {
    let hx = grid.width() as f32 * grid.cell_m() * 0.5;
    let hz = grid.height() as f32 * grid.cell_m() * 0.5;
    (1..=count)
        .map(|id| {
            let x = rng.signed() * hx;
            let z = rng.signed() * hz;
            let r = 1.0 + rng.unit() * 9.0;
            (id, Stamp::from_world(x, z, r))
        })
        .collect()
}

fn random_cell(grid: &NavGrid, rng: &mut SplitMix64) -> u32 {
    (rng.next_u64() % (grid.width() * grid.height()) as u64) as u32
}

/// A passable cell near a random one.
fn open_cell(grid: &NavGrid, profile: Profile, rng: &mut SplitMix64) -> u32 {
    for _ in 0..64 {
        let c = random_cell(grid, rng);
        if let Some(p) = grid.nearest_passable(profile, c, 8) {
            return p;
        }
    }
    0
}

/// The brute-force reference: relax `I(c) = min over legal moves (I(n) + step)` over every cell
/// until nothing changes. Written from the definition (the grid's step and corner rules), not from
/// the module's code.
fn reference(grid: &NavGrid, profile: Profile, goals: &[u32]) -> Vec<u32> {
    let n = grid.width() * grid.height();
    let mut v = vec![UNREACHED; n];
    for g in goals {
        if grid.passable(profile, *g) {
            v[*g as usize] = 0;
        }
    }
    let (w, h) = (grid.width() as i32, grid.height() as i32);
    loop {
        let mut changed = false;
        for c in 0..n as u32 {
            if !grid.passable(profile, c) {
                continue;
            }
            let (x, z) = grid.xz(c);
            for (dx, dz) in NEIGHBOURS_8 {
                let (nx, nz) = (x as i32 + dx, z as i32 + dz);
                if nx < 0 || nz < 0 || nx >= w || nz >= h {
                    continue;
                }
                let t = grid.index(nx as usize, nz as usize);
                if !grid.passable(profile, t)
                    || !grid.can_step(profile, c, dx, dz)
                    || v[t as usize] == UNREACHED
                {
                    continue;
                }
                let diag = dx != 0 && dz != 0;
                if diag {
                    let a = grid.index(nx as usize, z);
                    let b = grid.index(x, nz as usize);
                    if !grid.passable(profile, a)
                        || !grid.passable(profile, b)
                        || !grid.can_step(profile, c, dx, 0)
                        || !grid.can_step(profile, c, 0, dz)
                    {
                        continue;
                    }
                }
                let step = if diag { 14 } else { 10 } * grid.cost(profile, t) / COST_FLAT;
                let cand = v[t as usize] + step;
                if cand < v[c as usize] {
                    v[c as usize] = cand;
                    changed = true;
                }
            }
        }
        if !changed {
            return v;
        }
    }
}

/// The seeded grids the reference runs on: flat with stamps, and rough ground with stamps.
fn seeded_grids() -> Vec<NavGrid> {
    let mut out = Vec::new();
    for seed in 0..4u64 {
        let mut rng = SplitMix64::new(0xf10e + seed);
        let mut g = if seed % 2 == 0 {
            flat(40, 34)
        } else {
            rough(40, 34, seed)
        };
        let set = stamps(&g, &mut rng, 10);
        g.sync_stamps(set);
        out.push(g);
    }
    out
}

/// Acceptance: the integration field equals the brute-force reference on seeded grids with stamps,
/// for every profile and one to three goals, and every direction is the reference's best move
/// (lowest `(value + step, neighbor index)`).
#[test]
fn integration_equals_dijkstra_reference() {
    let mut rng = SplitMix64::new(7);
    let mut checked = 0usize;
    for grid in seeded_grids() {
        for p in 0..5u8 {
            let profile = Profile(p);
            for goals_n in 1..=3 {
                let goals: Vec<u32> = (0..goals_n)
                    .map(|_| open_cell(&grid, profile, &mut rng))
                    .collect();
                let field = FlowField::build(&grid, profile, &goals, FlowSettings::default());
                let want = reference(&grid, profile, &goals);
                assert_eq!(
                    field.integration_values(),
                    &want[..],
                    "profile {p}, goals {goals:?}"
                );
                for c in 0..want.len() as u32 {
                    let v = want[c as usize];
                    match field.dir(c) {
                        FlowDir::Unreachable => assert_eq!(v, UNREACHED),
                        FlowDir::Goal => assert_eq!(v, 0),
                        FlowDir::Step(k) => {
                            let (dx, dz) = NEIGHBOURS_8[k as usize];
                            assert!(can_move(&grid, profile, c, dx, dz));
                            // The best of every legal move.
                            let mut best = (UNREACHED, u32::MAX);
                            for (ex, ez) in NEIGHBOURS_8 {
                                if !can_move(&grid, profile, c, ex, ez) {
                                    continue;
                                }
                                let (x, z) = grid.xz(c);
                                let t = grid.index((x as i32 + ex) as usize, (z as i32 + ez) as usize);
                                if want[t as usize] == UNREACHED {
                                    continue;
                                }
                                let total =
                                    want[t as usize] + step_cost(&grid, profile, t, ex != 0 && ez != 0);
                                if (total, t) < best {
                                    best = (total, t);
                                }
                            }
                            assert_eq!(best.0, v, "cell {c}");
                            assert_eq!(field.next_cell(c), Some(best.1), "tie by index at cell {c}");
                            checked += 1;
                        }
                    }
                }
            }
        }
    }
    assert!(checked > 10_000, "{checked} moves checked");
}

/// Walk the field from `cell`: (steps, summed step cost, final cell). Panics on a cycle.
fn walk(
    grid: &NavGrid,
    profile: Profile,
    mut next: impl FnMut(u32) -> Option<u32>,
    start: u32,
) -> (u32, u32, u32) {
    let (mut at, mut steps, mut cost) = (start, 0u32, 0u32);
    let limit = (grid.width() * grid.height()) as u32;
    while let Some(n) = next(at) {
        let (ax, az) = grid.xz(at);
        let (nx, nz) = grid.xz(n);
        assert!(ax.abs_diff(nx) <= 1 && az.abs_diff(nz) <= 1, "one cell per move");
        assert!(can_move(
            grid,
            profile,
            at,
            nx as i32 - ax as i32,
            nz as i32 - az as i32
        ));
        cost += step_cost(grid, profile, n, ax != nx && az != nz);
        at = n;
        steps += 1;
        assert!(steps <= limit, "cycle from {start}");
    }
    (steps, cost, at)
}

/// The walled map: a cliff wall with a gap, two building stamps, on the 256 x 256 preset canvas.
fn walled() -> NavGrid {
    let mut g = ground(256, 256, |x, z| {
        if (126..=129).contains(&x) && !(140..=146).contains(&z) {
            60.0
        } else {
            0.0
        }
    });
    g.sync_stamps(vec![
        (60, Stamp::from_world(-60.0, 30.0, 9.0)),
        (61, Stamp::from_world(40.0, 70.0, 12.0)),
    ]);
    g
}

/// Acceptance: from every reachable cell, following the directions reaches a goal in at most
/// `integration / 10` moves (each move costs at least 10), the values fall strictly on every move
/// (no cycles), and the summed step costs equal the cell's integration value.
#[test]
fn every_reachable_cell_descends_to_a_goal() {
    let mut grids = seeded_grids();
    grids.push(walled());
    let mut rng = SplitMix64::new(11);
    let mut walks = 0u32;
    for grid in &grids {
        for p in [Profile::HOVER, Profile::FOOT, Profile::HOVER_LARGE] {
            let goals = [open_cell(grid, p, &mut rng), open_cell(grid, p, &mut rng)];
            let field = FlowField::build(grid, p, &goals, FlowSettings::default());
            let n = (grid.width() * grid.height()) as u32;
            let stride = if n > 20_000 { 7 } else { 1 };
            for c in (0..n).step_by(stride) {
                let Some(v) = field.integration(c) else {
                    continue;
                };
                let mut last = v;
                let (steps, cost, end) = walk(
                    grid,
                    p,
                    |at| {
                        let next = field.next_cell(at)?;
                        let nv = field.integration(next).expect("a move lands on a reached cell");
                        assert!(nv < last, "values fall on every move");
                        last = nv;
                        Some(next)
                    },
                    c,
                );
                assert_eq!(field.dir(end), FlowDir::Goal);
                assert!(field.goals().contains(&end));
                assert!(steps <= v / 10, "{steps} moves for value {v}");
                assert_eq!(cost, v, "arrival cost equals the integration value");
                walks += 1;
            }
        }
    }
    assert!(walks > 5_000, "{walks}");
}

/// Acceptance: cells walled off from every goal, blocked cells, a field with its only goal in a
/// cliff and a field with no goals are all unreachable (no integration value, no direction).
#[test]
fn unreachable_cells_are_marked() {
    // A cliff ring of radius 40-55 m around the center: inside is cut off from outside.
    let ring = ground(64, 64, |x, z| {
        let (dx, dz) = (x as f32 - 32.0, z as f32 - 32.0);
        let r2 = dx * dx + dz * dz;
        if (64.0..=121.0).contains(&r2) { 80.0 } else { 0.0 }
    });
    let p = Profile::HOVER;
    let outside = ring.index(2, 2);
    let inside = ring.index(32, 32);
    let field = FlowField::build(&ring, p, &[outside], FlowSettings::default());
    assert_eq!(field.integration(inside), None);
    assert_eq!(field.dir(inside), FlowDir::Unreachable);
    assert_eq!(field.next_cell(inside), None);
    assert_eq!(field.direction_at(ring.center(inside)), None);
    assert!(field.integration(ring.index(60, 60)).is_some());
    let mut blocked = 0;
    let mut cut_off = 0;
    for c in 0..(64 * 64) as u32 {
        let (x, z) = ring.xz(c);
        let (dx, dz) = (x as f32 - 32.0, z as f32 - 32.0);
        let walled_in = dx * dx + dz * dz < 64.0;
        if !ring.passable(p, c) {
            blocked += 1;
            assert_eq!(field.dir(c), FlowDir::Unreachable, "blocked cell {c}");
            assert_eq!(field.integration(c), None);
        } else if walled_in {
            cut_off += 1;
            assert_eq!(field.dir(c), FlowDir::Unreachable, "walled-in cell {c}");
        }
    }
    assert!(
        blocked > 50 && cut_off > 50,
        "{blocked} blocked, {cut_off} cut off"
    );
    // The ring's inside is reachable from the inside.
    let inner = FlowField::build(&ring, p, &[inside], FlowSettings::default());
    assert!(inner.integration(ring.index(30, 33)).is_some());
    assert_eq!(inner.integration(outside), None);
    // A goal in the cliff is dropped: nothing is reachable.
    let cliff = (0..(64 * 64) as u32)
        .find(|c| !ring.passable(p, *c))
        .expect("a cliff cell");
    let none = FlowField::build(&ring, p, &[cliff, 1 << 30], FlowSettings::default());
    assert!(none.goals().is_empty());
    assert_eq!(none.stats().reached, 0);
    assert!(none.dir_bytes().iter().all(|d| *d == DIR_NONE));
    let empty = FlowField::build(&ring, p, &[], FlowSettings::default());
    assert!(empty.integration_values().iter().all(|v| *v == UNREACHED));
}

/// Acceptance: with several goals every cell's value is the smallest of the single-goal fields and
/// its walk ends at a goal whose single-goal value is that smallest.
#[test]
fn multiple_goals_take_the_nearest() {
    let mut rng = SplitMix64::new(23);
    for grid in seeded_grids() {
        let p = Profile::FOOT;
        let goals: Vec<u32> = (0..3).map(|_| open_cell(&grid, p, &mut rng)).collect();
        let both = FlowField::build(&grid, p, &goals, FlowSettings::default());
        let single: Vec<FlowField> = goals
            .iter()
            .map(|g| FlowField::build(&grid, p, &[*g], FlowSettings::default()))
            .collect();
        for c in 0..(grid.width() * grid.height()) as u32 {
            let best = single.iter().filter_map(|f| f.integration(c)).min();
            assert_eq!(both.integration(c), best, "cell {c}");
            if let Some(v) = best {
                let (_, _, end) = walk(&grid, p, |at| both.next_cell(at), c);
                let gi = goals.iter().position(|g| *g == end).expect("ends at a goal");
                assert_eq!(
                    single[gi].integration(c),
                    Some(v),
                    "cell {c} ends at its nearest goal"
                );
            }
        }
    }
    // Two goals on open ground: the midline splits them.
    let g = flat(60, 20);
    let (a, b) = (g.index(5, 10), g.index(55, 10));
    let f = FlowField::build(&g, Profile::HOVER, &[a, b], FlowSettings::default());
    assert_eq!(f.next_cell(g.index(20, 10)), Some(g.index(19, 10)));
    assert_eq!(f.next_cell(g.index(40, 10)), Some(g.index(41, 10)));
}

/// The 1024 x 1024 canvas with seeded stamps (obstacle discs of 1-10 m, plus a few long walls of
/// discs) for the clustered flow.
fn big_canvas() -> NavGrid {
    let mut g = flat(1024, 1024);
    let mut rng = SplitMix64::new(0xb16);
    let mut set = stamps(&g, &mut rng, 1500);
    let mut id = 10_000;
    for w in 0..6 {
        let z = -2000.0 + w as f32 * 700.0;
        let x0 = rng.signed() * 1500.0;
        for k in 0..60 {
            set.push((id, Stamp::from_world(x0 + k as f32 * 15.0, z, 9.0)));
            id += 1;
        }
    }
    g.sync_stamps(set);
    g
}

/// Arrival bound of a clustered flow against the full field: `full * 9 / 8 + 2 * 14 * CLUSTER`
/// (12.5 % plus one cluster diagonal each way). Recorded in the book page.
fn bound(full: u32) -> u32 {
    full + full / 8 + 2 * 14 * CLUSTER as u32
}

/// Mean excess of `cluster` over `full` in percent (for the printed evidence).
fn excess_pct(cluster: u64, full: u64) -> f64 {
    let denom = full.max(1);
    (cluster as f64 / denom as f64 - 1.0) * 100.0
}

/// Acceptance: on a 1024 x 1024 canvas with stamps the clustered flow reaches exactly the cells the
/// full field reaches; from sampled cells, following it arrives at a goal with exactly the cost it
/// reports; and that cost is never below the full field's and within [`bound`] of it.
#[test]
fn cluster_flow_equals_the_full_field_route_cost() {
    let grid = big_canvas();
    let p = Profile::HOVER;
    let goal = grid
        .nearest_passable(p, grid.cell_at(300.0, -200.0), 20)
        .expect("goal");
    let full = FlowField::build(&grid, p, &[goal], FlowSettings::default());
    let mut flow = ClusterFlow::build(&grid, p, &[goal], 1 << 20);
    let n = (grid.width() * grid.height()) as u32;
    let mut reached = 0u32;
    for c in 0..n {
        let a = full.integration(c);
        let b = flow.integration(&grid, c);
        assert_eq!(a.is_some(), b.is_some(), "reachability at cell {c}");
        if let (Some(a), Some(b)) = (a, b) {
            assert!(b >= a, "cell {c}: clustered {b} below the full field's {a}");
            assert!(b <= bound(a), "cell {c}: clustered {b} past the bound of {a}");
            reached += 1;
        }
    }
    assert!(reached > n / 2, "{reached} reached");
    let mut rng = SplitMix64::new(5);
    let (mut sum_full, mut sum_cluster, mut exact) = (0u64, 0u64, 0u32);
    let samples = 200;
    for _ in 0..samples {
        let c = open_cell(&grid, p, &mut rng);
        let Some(v) = flow.integration(&grid, c) else {
            continue;
        };
        let (_, cost, end) = walk(&grid, p, |at| flow.next_cell(&grid, at), c);
        assert_eq!(end, goal);
        assert_eq!(cost, v, "arrival cost equals the clustered value");
        let f = full.integration(c).expect("reached");
        sum_full += u64::from(f);
        sum_cluster += u64::from(v);
        exact += u32::from(f == v);
    }
    let s = flow.stats();
    println!(
        "cluster flow 1024^2: mean excess {:.2} %, exact {exact}/{samples}, local builds {}, regions {:?}",
        excess_pct(sum_cluster, sum_full),
        s.local_builds,
        flow.route_size()
    );
    // Every local field was built once, then reused.
    assert!(s.local_hits > 0);
    assert_eq!(s.route_builds, 1);
}

/// On open ground the clustered flow reaches every cell and stays within one cluster diagonal
/// each way of the full field (`2 * 14 * CLUSTER`); most cells are exact. Prints the share.
#[test]
fn cluster_flow_stays_close_on_open_ground() {
    let grid = flat(200, 136);
    let cells = (grid.width() * grid.height()) as u32;
    for (gx, gz) in [(10usize, 7usize), (150, 100), (199, 0)] {
        let goal = grid.index(gx, gz);
        let full = FlowField::build(&grid, Profile::HOVER, &[goal], FlowSettings::default());
        let mut flow = ClusterFlow::build(&grid, Profile::HOVER, &[goal], 1 << 20);
        let mut exact = 0u32;
        let mut worst = 0u32;
        for c in 0..cells {
            let a = full.integration(c).expect("open");
            let b = flow.integration(&grid, c).expect("open");
            assert!(b >= a, "cell {c}");
            exact += u32::from(a == b);
            worst = worst.max(b - a);
        }
        println!("open ground goal ({gx}, {gz}): exact {exact} of {cells}, worst excess {worst}");
        assert!(worst <= 2 * 14 * CLUSTER as u32, "worst excess {worst}");
        assert!(exact > cells / 2, "{exact} exact");
    }
}

/// ADR-8839: a clustered flow follows the grid's own cluster size (`GridSettings::cluster_cells`)
/// and reads two-byte labels at 16 and above. At every size from 2 to 32, on a 256 x 256 canvas with
/// stamps: reachability equals the full field's at every cell, values are never below the full
/// field's and within the bound with that size's cluster diagonal, and walking the flow from
/// sampled cells arrives at the goal with exactly the reported value. Before ADR-8839 the flow
/// assumed 8-cell clusters and one-byte labels: at 16 it indexed an empty label row (panic), at 4
/// its cluster rectangles were wrong.
#[test]
fn cluster_flow_follows_the_grid_cluster_size() {
    for size in [2usize, 4, 8, 16, 32] {
        let settings = Arc::new(GridSettings {
            cluster_cells: size,
            ..(*canvas(256, 256)).clone()
        });
        let mut grid =
            NavGrid::new(Arc::new(NavStatic::build(None, settings).expect("canvas"))).expect("grid");
        assert_eq!(grid.cluster_cells(), size);
        let mut rng = SplitMix64::new(0xc1u64 + size as u64);
        let mut set = stamps(&grid, &mut rng, 400);
        grid.sync_stamps(set.clone());
        let p = Profile::HOVER;
        let goal = grid
            .nearest_passable(p, grid.cell_at(120.0, -80.0), 20)
            .expect("goal");
        let full = FlowField::build(&grid, p, &[goal], FlowSettings::default());
        let mut flow = ClusterFlow::build(&grid, p, &[goal], 1 << 20);
        let slack = 2 * 14 * size as u32;
        let n = (grid.width() * grid.height()) as u32;
        let mut reached = 0u32;
        for c in 0..n {
            let a = full.integration(c);
            let b = flow.integration(&grid, c);
            assert_eq!(
                a.is_some(),
                b.is_some(),
                "cluster {size}: reachability at cell {c}"
            );
            if let (Some(a), Some(b)) = (a, b) {
                assert!(b >= a, "cluster {size}: cell {c}: {b} below the full field's {a}");
                assert!(
                    b <= a + a / 8 + slack,
                    "cluster {size}: cell {c}: {b} past the bound of {a}"
                );
                reached += 1;
            }
        }
        assert!(reached > n / 2, "cluster {size}: {reached} reached");
        for _ in 0..60 {
            let c = open_cell(&grid, p, &mut rng);
            let Some(v) = flow.integration(&grid, c) else {
                continue;
            };
            let (_, cost, end) = walk(&grid, p, |at| flow.next_cell(&grid, at), c);
            assert_eq!(end, goal, "cluster {size}");
            assert_eq!(cost, v, "cluster {size}: arrival cost equals the clustered value");
        }
        // Each local field covers exactly one cluster of the grid's size.
        let key = flow.region_of(&grid, goal).expect("goal region");
        assert_eq!(key.0 as usize, grid.cluster_of(goal));
        let local = flow.field_for(&grid, goal).expect("goal field").window();
        assert_eq!(local, CellRect::cluster(&grid, grid.cluster_of(goal)));
        assert!(
            local.width() <= size && local.height() <= size,
            "cluster {size}: {local:?}"
        );
        // One more stamp: the synced flow equals one built fresh on the changed grid.
        set.push((9_999, Stamp::from_world(150.0, -40.0, 10.0)));
        let sync = grid.sync_stamps(set);
        assert!(sync.changed && !sync.dirty_clusters.is_empty());
        assert!(grid.passable(p, goal), "the stamp leaves the goal open");
        flow.on_sync(&grid, &sync);
        let mut fresh = ClusterFlow::build(&grid, p, &[goal], 1 << 20);
        for c in 0..n {
            assert_eq!(
                flow.integration(&grid, c),
                fresh.integration(&grid, c),
                "cluster {size}: cell {c} after the sync"
            );
        }
    }
}

/// ADR-8839: `ClusterFlow::view` answers, cell for cell, what `integration` and `dir` answer, at
/// the default cluster size and at 16 (two-byte labels).
#[test]
fn cluster_flow_view_matches_its_cells() {
    for size in [8usize, 16] {
        let settings = Arc::new(GridSettings {
            cluster_cells: size,
            ..(*canvas(256, 256)).clone()
        });
        let mut grid =
            NavGrid::new(Arc::new(NavStatic::build(None, settings).expect("canvas"))).expect("grid");
        let mut rng = SplitMix64::new(0x71e4);
        grid.sync_stamps(stamps(&grid, &mut rng, 300));
        let p = Profile::HOVER;
        let goal = grid
            .nearest_passable(p, grid.cell_at(-200.0, 150.0), 20)
            .expect("goal");
        let mut flow = ClusterFlow::build(&grid, p, &[goal], 1 << 20);
        let rect = CellRect {
            x0: 100,
            z0: 90,
            x1: 140,
            z1: 130,
        };
        let view = flow.view(&grid, rect, 4096);
        assert_eq!(view.rect, rect);
        assert!(!view.truncated);
        for (i, (dir, value)) in view.dirs.iter().zip(&view.integration).enumerate() {
            let cell = grid.index(100 + i % 40, 90 + i / 40);
            assert_eq!(flow.integration(&grid, cell).unwrap_or(UNREACHED), *value);
            let want = match flow.dir(&grid, cell) {
                FlowDir::Step(k) => k,
                FlowDir::Goal => DIR_GOAL,
                FlowDir::Unreachable => DIR_NONE,
            };
            assert_eq!(dir & DIR_MASK, want, "cluster {size}: cell {cell}");
        }
        assert!(view.integration.iter().any(|v| *v != UNREACHED));
        let capped = flow.view(&grid, rect, 100);
        assert!(capped.truncated && capped.rect.area() <= 100);
    }
}

/// A local capacity far below the working set evicts in batches and still answers exactly as an
/// uncapped flow does.
#[test]
fn a_small_local_capacity_evicts_and_stays_correct() {
    let grid = walled();
    let p = Profile::HOVER;
    let goal = grid.cell_at(400.0, 300.0);
    let mut small = ClusterFlow::build(&grid, p, &[goal], 16);
    let mut big = ClusterFlow::build(&grid, p, &[goal], 1 << 20);
    for c in (0..(grid.width() * grid.height()) as u32).step_by(5) {
        assert_eq!(small.integration(&grid, c), big.integration(&grid, c), "cell {c}");
        assert!(small.cached_fields() <= 16);
    }
    assert!(small.stats().local_evicted > 0);
    assert_eq!(big.stats().local_evicted, 0);
}

/// Acceptance: after a stamp change, a clustered flow rebuilds only the local fields of the dirty
/// clusters, their neighbors and the regions whose seeds changed (counters), and the result equals
/// a flow built fresh on the changed grid, cell for cell. Through the cache as well.
#[test]
fn dirty_clusters_rebuild_only_their_fields() {
    let mut grid = flat(256, 256);
    let p = Profile::FOOT;
    let goal = grid.cell_at(-500.0, -500.0);
    let mut cache = FlowCache::new(FlowCacheSettings::default());
    let n = (grid.width() * grid.height()) as u32;
    let warm = |cache: &mut FlowCache, grid: &NavGrid| {
        let flow = cache.cluster_flow(grid, p, &[goal]);
        for c in 0..n {
            flow.integration(grid, c);
        }
    };
    warm(&mut cache, &grid);
    let before = cache.cluster_stats();
    let fields = cache.cluster_flow(&grid, p, &[goal]).cached_fields() as u32;
    assert_eq!(before.local_builds, fields, "each local field built once");
    assert_eq!(fields, (grid.clusters_x * grid.clusters_z) as u32);
    // A stamp in the far corner (upstream of nothing: the goal is the opposite corner).
    let sync = grid.sync_stamps(vec![(1, Stamp::from_world(560.0, 560.0, 12.0))]);
    assert!(sync.changed && !sync.dirty_clusters.is_empty());
    cache.on_sync(&grid, &sync);
    warm(&mut cache, &grid);
    let after = cache.cluster_stats();
    let rebuilt = after.local_builds - before.local_builds;
    let dropped = after.local_dropped - before.local_dropped;
    println!(
        "dirty {} clusters: dropped {dropped}, rebuilt {rebuilt}, revalidated {} of {fields}",
        sync.dirty_clusters.len(),
        after.local_revalidated - before.local_revalidated
    );
    assert!(dropped > 0 && dropped <= 9 * sync.dirty_clusters.len() as u32);
    assert!(rebuilt >= dropped);
    assert!(rebuilt < fields / 8, "{rebuilt} of {fields} rebuilt");
    assert_eq!(after.route_builds - before.route_builds, 1);
    // The same as a fresh flow on the changed grid.
    let mut fresh = ClusterFlow::build(&grid, p, &[goal], 1 << 20);
    let flow = cache.cluster_flow(&grid, p, &[goal]);
    for c in 0..n {
        assert_eq!(
            flow.integration(&grid, c),
            fresh.integration(&grid, c),
            "cell {c}"
        );
        assert_eq!(flow.dir(&grid, c), fresh.dir(&grid, c), "cell {c}");
    }
    // An unchanged sync is a no-op.
    let again = grid.sync_stamps(vec![(1, Stamp::from_world(560.0, 560.0, 12.0))]);
    assert!(!again.changed);
    let generation = cache.generation();
    cache.on_sync(&grid, &again);
    assert_eq!(cache.generation(), generation);
    // A stamp next to the goal changes every route's seeds downstream: more rebuilds, still equal.
    let near_goal = grid.sync_stamps(vec![
        (1, Stamp::from_world(560.0, 560.0, 12.0)),
        (2, Stamp::from_world(-470.0, -470.0, 10.0)),
    ]);
    cache.on_sync(&grid, &near_goal);
    let mut fresh = ClusterFlow::build(&grid, p, &[goal], 1 << 20);
    let flow = cache.cluster_flow(&grid, p, &[goal]);
    for c in (0..n).step_by(3) {
        assert_eq!(
            flow.integration(&grid, c),
            fresh.integration(&grid, c),
            "cell {c}"
        );
    }
}

/// Acceptance: the cache answers the same goal set (in any order, with duplicates) from one build,
/// misses on another profile or goal set, misses after a changed stamp sync and evicts the least
/// recently used field at its capacity.
#[test]
fn cache_hits_on_the_same_goal() {
    let mut grid = flat(64, 64);
    let p = Profile::HOVER;
    let (a, b, c) = (grid.index(3, 4), grid.index(50, 40), grid.index(10, 60));
    let mut cache = FlowCache::new(FlowCacheSettings {
        capacity: 2,
        ..FlowCacheSettings::default()
    });
    let first = cache
        .field(&grid, p, &[a, b], FlowSettings::default())
        .bits_hash();
    let again = cache
        .field(&grid, p, &[b, a, b], FlowSettings::default())
        .bits_hash();
    assert_eq!(first, again);
    assert_eq!((cache.stats().hits, cache.stats().misses), (1, 1));
    cache.field(&grid, Profile::FOOT, &[a, b], FlowSettings::default());
    assert_eq!((cache.stats().hits, cache.stats().misses), (1, 2));
    // Capacity 2: a third goal set evicts the least recently used (the HOVER [a, b] field).
    cache.field(&grid, p, &[c], FlowSettings::default());
    assert_eq!(cache.stats().evictions, 1);
    assert_eq!(cache.len(), 2);
    cache.field(&grid, Profile::FOOT, &[a, b], FlowSettings::default());
    assert_eq!(cache.stats().hits, 2);
    cache.field(&grid, p, &[a, b], FlowSettings::default());
    assert_eq!(cache.stats().misses, 4, "evicted, so built again");
    // Other settings are another field.
    let los = FlowSettings {
        los_cells: 8,
        ..FlowSettings::default()
    };
    cache.field(&grid, p, &[a, b], los);
    assert_eq!(cache.stats().misses, 5);
    // A changed sync moves the generation: the next lookup builds on the new grid.
    let keys = cache.keys();
    assert!(keys.iter().all(|k| k.generation == 0));
    let sync = grid.sync_stamps(vec![(9, Stamp::from_world(0.0, 0.0, 6.0))]);
    cache.on_sync(&grid, &sync);
    assert!(cache.is_empty());
    assert_eq!(cache.stats().invalidated, 2);
    let rebuilt = cache
        .field(&grid, p, &[a, b], FlowSettings::default())
        .bits_hash();
    assert_ne!(rebuilt, first, "the stamp changed the field");
    assert_eq!(cache.keys()[0].generation, 1);
    assert_eq!(
        rebuilt,
        FlowField::build(&grid, p, &[a, b], FlowSettings::default()).bits_hash()
    );
    let misses = cache.stats().misses;
    cache.field(&grid, p, &[b, a], FlowSettings::default());
    assert_eq!(cache.stats().misses, misses);
    // Clustered flows hit on the same goal too, and reuse their local fields.
    let hits = cache.stats().hits;
    cache.cluster_flow(&grid, p, &[c]).integration(&grid, a);
    let builds = cache.cluster_stats().local_builds;
    cache.cluster_flow(&grid, p, &[c, c]).integration(&grid, a);
    assert_eq!(cache.stats().hits, hits + 1);
    assert_eq!(cache.cluster_stats().local_builds, builds);
    assert!(cache.cluster_stats().local_hits > 0);
}

/// Line of sight: with `los_cells` on, open cells near a single goal steer straight at it; cells
/// behind a wall do not; a multi-goal field never flags.
#[test]
fn line_of_sight_cells_steer_straight_at_the_goal() {
    let g = ground(48, 48, |x, z| {
        if x == 24 && (10..30).contains(&z) {
            60.0
        } else {
            0.0
        }
    });
    let p = Profile::FOOT;
    let goal = g.index(30, 20);
    let settings = FlowSettings {
        los_cells: 40,
        ..FlowSettings::default()
    };
    let f = FlowField::build(&g, p, &[goal], settings);
    assert!(f.stats().los_cells > 100);
    let seen = g.index(40, 27);
    assert!(f.los(seen));
    let pos = g.center(seen);
    let to = g.center(goal);
    let d = f.direction_at(pos).expect("reachable");
    let (dx, dz) = (to[0] - pos[0], to[1] - pos[1]);
    // Parallel to the line to the goal, unit length.
    assert!((d[0] * dz - d[1] * dx).abs() < 1e-3);
    assert!((d[0] * d[0] + d[1] * d[1] - 1.0).abs() < 1e-5);
    let hidden = g.index(10, 20);
    assert!(!f.los(hidden), "behind the wall");
    assert_eq!(
        f.direction_at(g.center(hidden)),
        Some(UNIT[f.dir_bytes()[hidden as usize] as usize & 7])
    );
    // Off by default; two goals never flag.
    assert_eq!(
        FlowField::build(&g, p, &[goal], FlowSettings::default())
            .stats()
            .los_cells,
        0
    );
    let two = FlowField::build(&g, p, &[goal, g.index(2, 2)], settings);
    assert_eq!(two.stats().los_cells, 0);
    // The flag does not change integration values or moves.
    let plain = FlowField::build(&g, p, &[goal], FlowSettings::default());
    assert_eq!(plain.integration_values(), f.integration_values());
    let masked: Vec<u8> = f.dir_bytes().iter().map(|d| d & DIR_MASK).collect();
    assert_eq!(plain.dir_bytes(), &masked[..]);
}

/// Sampling: the nearest cell by default; the bilinear blend mixes the four cells around the
/// point; off-grid points and blocked cells give `None`; NaN gives `None`.
#[test]
fn sampling_reads_the_nearest_cell_or_blends() {
    let g = flat(32, 32);
    let goal = g.index(16, 16);
    let f = FlowField::build(&g, Profile::HOVER, &[goal], FlowSettings::default());
    let west = g.center(g.index(4, 16));
    assert_eq!(f.direction_at(west), Some([1.0, 0.0]));
    assert_eq!(f.direction_at(g.center(goal)), Some([0.0, 0.0]));
    assert_eq!(f.direction_at([1.0e6, 0.0]), None);
    assert_eq!(f.direction_at([f32::NAN, 0.0]), None);
    let s = std::f32::consts::FRAC_1_SQRT_2;
    assert_eq!(f.direction_at(g.center(g.index(4, 4))), Some([s, s]));
    let blend = FlowField::build(
        &g,
        Profile::HOVER,
        &[goal],
        FlowSettings {
            blend: true,
            ..FlowSettings::default()
        },
    );
    // On a cell center the blend is that cell.
    assert_eq!(blend.direction_at(west), Some([1.0, 0.0]));
    // Halfway between a north cell (4, 3) and a north-east cell (4, 4): the average of the two.
    let a = g.center(g.index(4, 3));
    let b = g.center(g.index(4, 4));
    let (va, vb) = (f.direction_at(a).expect("a"), f.direction_at(b).expect("b"));
    assert_ne!(va, vb);
    let mid = [a[0], (a[1] + b[1]) * 0.5];
    let v = blend.direction_at(mid).expect("blend");
    assert_eq!(v, [va[0] * 0.5 + vb[0] * 0.5, va[1] * 0.5 + vb[1] * 0.5]);
    // A blocked cell: no direction.
    let mut walled = flat(32, 32);
    walled.sync_stamps(vec![(1, Stamp::from_world(0.0, 0.0, 6.0))]);
    let wf = FlowField::build(
        &walled,
        Profile::HOVER,
        &[walled.index(2, 2)],
        FlowSettings::default(),
    );
    assert!(!walled.passable(Profile::HOVER, walled.cell_at(0.0, 0.0)));
    assert_eq!(wf.direction_at([0.0, 0.0]), None);
}

/// ADR-8839: a field's arrows reach the deterministic debug sink as `DebugKind::Flow` primitives,
/// one per arrow of [`FlowField::arrows`] in the same order: a circle on the goal, otherwise a
/// shaft that starts at the cell center and points along the cell's steering vector, plus a
/// two-line head. The cap holds; `FlowDebug` draws only when the switch holds `FLOW`; a clustered
/// flow draws its cached local fields only.
#[test]
fn flow_arrows_draw_through_the_debug_sink() {
    use regia_detsim::debug::{DebugKind, DebugPrim, DebugRecorder, DebugSource, DebugWhat};

    let g = walled();
    let p = Profile::HOVER;
    let goal = g.cell_at(400.0, 300.0);
    let settings = FlowSettings {
        los_cells: 12,
        ..FlowSettings::default()
    };
    let f = FlowField::build(&g, p, &[goal], settings);
    let (gx, gz) = g.xz(goal);
    let rect = CellRect {
        x0: gx as u32 - 20,
        z0: gz as u32 - 20,
        x1: gx as u32 + 20,
        z1: gz as u32 + 20,
    };
    let arrows = f.arrows(rect, usize::MAX);
    assert!(arrows.iter().any(|a| a.los) && arrows.iter().any(|a| !a.los && !a.goal));
    let mut rec = DebugRecorder::default();
    let drawn = f.debug_emit(&mut rec, rect, usize::MAX, 2.5);
    assert_eq!(drawn, arrows.len());
    assert_eq!(
        rec.count_kind(DebugKind::Flow),
        rec.len(),
        "every primitive is a flow arrow"
    );
    let counts = rec.counts();
    assert_eq!(counts.circles, 1, "one goal");
    assert_eq!(counts.lines, 3 * (arrows.len() - 1));
    let cell = g.cell_m();
    let mut prims = rec.prims().iter();
    for arrow in &arrows {
        if arrow.goal {
            assert!(matches!(prims.next(), Some(DebugPrim::Circle { center, .. })
                if *center == [arrow.from[0], 2.5, arrow.from[1]]));
            continue;
        }
        let Some(DebugPrim::Line { a, b, .. }) = prims.next() else {
            panic!("a shaft for {arrow:?}");
        };
        assert_eq!(*a, [arrow.from[0], 2.5, arrow.from[1]]);
        assert_eq!(b[1], 2.5);
        let (dx, dz) = (b[0] - a[0], b[2] - a[2]);
        let len = regia_detsim::math::sqrt(dx * dx + dz * dz);
        assert!((len - 0.8 * cell).abs() < 1e-3, "shaft {len}");
        let steer = f.direction_at(arrow.from).expect("reachable");
        assert!(
            (dx / len - steer[0]).abs() < 1e-4 && (dz / len - steer[1]).abs() < 1e-4,
            "shaft {dx},{dz} vs steering {steer:?}"
        );
        for _ in 0..2 {
            assert!(matches!(prims.next(), Some(DebugPrim::Line { a: tip, .. }) if tip == b));
        }
    }
    assert!(prims.next().is_none());

    // The cap.
    let mut few = DebugRecorder::default();
    assert_eq!(f.debug_emit(&mut few, rect, 7, 0.0), 7);
    // The debug source draws only with FLOW in the switch.
    let mut source = FlowDebug {
        rect,
        y: 2.5,
        ..FlowDebug::new(&f)
    };
    source.set_debug_capture(None);
    let mut none = DebugRecorder::default();
    source.debug_emit(
        &mut none,
        DebugWhat::from_bits(DebugWhat::ALL.bits() & !DebugWhat::FLOW.bits()),
    );
    assert!(none.is_empty());
    let mut all = DebugRecorder::default();
    source.debug_emit(&mut all, DebugWhat::ALL);
    assert_eq!(all, rec);

    // A clustered flow draws only what it has cached.
    let mut flow = ClusterFlow::build(&g, p, &[goal], 1 << 20);
    let mut empty = DebugRecorder::default();
    assert_eq!(flow.debug_emit_cached(&mut empty, rect, usize::MAX, 0.0), 0);
    flow.field_for(&g, goal).expect("goal field");
    let mut local = DebugRecorder::default();
    let n = flow.debug_emit_cached(&mut local, rect, usize::MAX, 0.0);
    assert!(n > 0 && local.count_kind(DebugKind::Flow) == local.len());
    assert_eq!(
        local.counts().circles,
        1,
        "the goal region's field holds the goal"
    );
}

/// The debug arrows and the capped view: inside the field, at most the cap, row-major.
#[test]
fn arrows_and_views_are_capped() {
    let g = flat(32, 32);
    let goal = g.index(16, 16);
    let f = FlowField::build(&g, Profile::HOVER, &[goal], FlowSettings::default());
    let all = f.arrows(CellRect::whole(&g), usize::MAX);
    assert_eq!(all.len(), 32 * 32);
    assert_eq!(all.iter().filter(|a| a.goal).count(), 1);
    let few = f.arrows(CellRect::whole(&g), 10);
    assert_eq!(few.len(), 10);
    assert_eq!(few[0].from, g.center(0));
    assert_eq!(few[0].to, g.center(g.index(1, 1)));
    let v = f.view(
        CellRect {
            x0: 10,
            z0: 10,
            x1: 100,
            z1: 100,
        },
        64,
    );
    assert!(v.truncated);
    assert!(v.rect.area() <= 64 && v.rect.x1 <= 32 && v.rect.z1 <= 32);
    assert_eq!(v.dirs.len(), v.rect.area());
    assert_eq!(v.integration[0], f.integration(g.index(10, 10)).expect("open"));
    // Memory: 5 bytes per cell plus the struct and goals.
    assert!(f.memory_bytes() >= 32 * 32 * BYTES_PER_CELL);
    assert!(f.memory_bytes() < 32 * 32 * BYTES_PER_CELL + 512);
}

// --------------------------------------------------------------------------------- determinism pin

/// Fixtures of the pinned scenario: flat with stamps, rough ground with stamps, the walled map.
fn pin_fixtures() -> Vec<NavGrid> {
    let mut v = seeded_grids();
    v.truncate(2);
    v.push(walled());
    v
}

/// 48 seeded ticks: each builds a full field (random profile, one to three goals, line of sight on
/// or off), samples it at eight points (nearest and blended), and every fourth tick also builds a
/// clustered flow and walks it from four cells.
struct FlowScenario {
    fixtures: Vec<NavGrid>,
    rng: SplitMix64,
    last: u64,
    acc: u64,
    reached: u64,
}

impl FlowScenario {
    fn new() -> Self {
        FlowScenario {
            fixtures: pin_fixtures(),
            rng: SplitMix64::new(0),
            last: 0,
            acc: 0,
            reached: 0,
        }
    }
}

impl DetScenario for FlowScenario {
    fn name(&self) -> &str {
        "flow_fields"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(0x5eed_f10e, 48, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        self.rng = SplitMix64::new(run.seed);
        self.last = 0;
        self.acc = 0;
        self.reached = 0;
    }

    fn step(&mut self, tick: u32) {
        let which = (self.rng.next_u64() % self.fixtures.len() as u64) as usize;
        let grid = &self.fixtures[which];
        let profile = Profile((self.rng.next_u64() % 5) as u8);
        let count = 1 + (self.rng.next_u64() % 3) as usize;
        let goals: Vec<u32> = (0..count).map(|_| random_cell(grid, &mut self.rng)).collect();
        let settings = FlowSettings {
            los_cells: if self.rng.next_u64().is_multiple_of(2) {
                24
            } else {
                0
            },
            blend: self.rng.next_u64().is_multiple_of(2),
            ..FlowSettings::default()
        };
        let field = FlowField::build(grid, profile, &goals, settings);
        let mut h = StateHasher::new();
        h.u64("field", field.bits_hash());
        h.u32("reached", field.stats().reached);
        let hx = grid.width() as f32 * grid.cell_m() * 0.5;
        let hz = grid.height() as f32 * grid.cell_m() * 0.5;
        for _ in 0..8 {
            let pos = [self.rng.signed() * hx, self.rng.signed() * hz];
            match field.direction_at(pos) {
                Some(d) => {
                    h.f32("dx", d[0]);
                    h.f32("dz", d[1]);
                }
                None => h.u32("none", 1),
            }
        }
        if tick.is_multiple_of(4) {
            let mut flow = ClusterFlow::build(grid, profile, &goals, 256);
            for _ in 0..4 {
                let c = random_cell(grid, &mut self.rng);
                h.u32("cluster_value", flow.integration(grid, c).unwrap_or(UNREACHED));
                let mut at = c;
                let mut steps = 0u32;
                while let Some(n) = flow.next_cell(grid, at) {
                    at = n;
                    steps += 1;
                }
                h.u32("cluster_end", at);
                h.u32("cluster_steps", steps);
            }
            let s = flow.stats();
            h.u32("local_builds", s.local_builds);
            h.u32("local_evicted", s.local_evicted);
        }
        self.reached += u64::from(field.stats().reached);
        self.last = h.finish();
        self.acc = (self.acc ^ self.last).wrapping_mul(0x100_0000_01b3);
    }

    fn hash_state(&self, h: &mut StateHasher) {
        h.u64("tick", self.last);
        h.u64("acc", self.acc);
        h.u64("reached", self.reached);
    }
}

/// Two fresh runs and a replay after reset give the same tick hashes.
#[test]
fn flow_scenario_is_deterministic() {
    let mut a = FlowScenario::new();
    let run = a.default_run();
    let first = golden::record(&mut a, &run);
    let second = golden::record(&mut FlowScenario::new(), &run);
    assert_eq!(first.tick_hashes.len(), 48);
    assert_eq!(
        golden::first_divergence(&first.tick_hashes, &second.tick_hashes),
        None
    );
    let again = golden::record(&mut a, &run);
    assert_eq!(again.tick_hashes, first.tick_hashes);
    assert!(a.reached > 0);
}

/// Acceptance: the seeded flow scenario is pinned by a determinism golden
/// (`goldens/flow_fields.detgold`, ADR-8046 format): every target must hash to the committed
/// x86-64 Linux run; `REGIA_BLESS=1` records this target's line. Prints one
/// `regia-det-golden {json}` line.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "test harness: where the pinned golden lives, as regia_detsim::golden::engine_golden_dir; no sim tick reads the environment (ADR-8367, ADR-8877)"
)]
fn flow_bits_match_the_pinned_hash() {
    // det-scan: allow(env_read): test harness: where the pinned golden lives, not sim input
    let dir = match std::env::var_os(golden::GOLDEN_DIR_ENV) {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("goldens"),
    };
    let mut scenario = FlowScenario::new();
    let run = scenario.default_run();
    let trace = golden::record(&mut scenario, &run);
    let report = golden::check_trace(
        &trace,
        &golden::golden_path(&dir, "flow_fields"),
        &golden::current_target(),
        GoldenMode::from_env(),
    );
    println!("{REPORT_PREFIX}{}", report.to_json());
    assert!(report.passed(), "{report}");
}

/// A 256 x 256 canvas with `cluster` cells per cluster side and seeded obstacle discs.
fn clustered_canvas(cluster: usize) -> NavGrid {
    let settings = Arc::new(GridSettings {
        canvas_cells: [256, 256],
        cluster_cells: cluster,
        ..GridSettings::hover_ground_jet()
    });
    let mut g = NavGrid::new(Arc::new(NavStatic::build(None, settings).expect("canvas"))).expect("grid");
    let mut rng = SplitMix64::new(0xc1u64 + cluster as u64);
    let set = stamps(&g, &mut rng, 400);
    g.sync_stamps(set);
    g
}

/// RegionKey follow-up (integ-v06p-sim finding): a grid built with another
/// `GridSettings::cluster_cells` than the default 8 (one-byte labels below 16, two-byte
/// `wide_labels` from 16) gets the same clustered flow guarantees as the default: the same reachable
/// cells as the full field, never below it, within the arrival bound of its own cluster size,
/// arrival cost equal to the reported value, region keys that name the grid's own clusters and
/// labels, and a stamp change through `on_sync` equal to a fresh build. Before the fix the flow
/// used the fixed 8-cell rectangles and the one-byte labels (a panic on a two-byte grid).
#[test]
fn cluster_flow_keeps_its_guarantees_at_every_cluster_size() {
    let p = Profile::HOVER;
    for cluster in [4usize, 16, 32] {
        let mut grid = clustered_canvas(cluster);
        assert_eq!(grid.cluster_cells(), cluster);
        let goal = grid
            .nearest_passable(p, grid.cell_at(150.0, -90.0), 20)
            .expect("goal");
        let full = FlowField::build(&grid, p, &[goal], FlowSettings::default());
        let mut flow = ClusterFlow::build(&grid, p, &[goal], 1 << 20);
        let n = (grid.width() * grid.height()) as u32;
        let slack = 2 * 14 * cluster as u32;
        let mut reached = 0u32;
        for c in 0..n {
            let a = full.integration(c);
            let b = flow.integration(&grid, c);
            assert_eq!(
                a.is_some(),
                b.is_some(),
                "cluster {cluster}: reachability at cell {c}"
            );
            if let (Some(a), Some(b)) = (a, b) {
                assert!(
                    b >= a,
                    "cluster {cluster}, cell {c}: clustered {b} below the full field's {a}"
                );
                assert!(
                    b <= a + a / 8 + slack,
                    "cluster {cluster}, cell {c}: clustered {b} past the bound of {a}"
                );
                reached += 1;
            }
            if let Some(key) = flow.region_of(&grid, c) {
                assert_eq!(key.0 as usize, grid.cluster_of(c), "cluster {cluster}, cell {c}");
                assert_eq!(key.1, grid.region_label(p, c), "cluster {cluster}, cell {c}");
            } else {
                assert!(
                    !grid.passable(p, c),
                    "cluster {cluster}: open cell {c} has no region"
                );
            }
        }
        assert!(reached > n / 2, "cluster {cluster}: {reached} reached");
        let mut rng = SplitMix64::new(11);
        for _ in 0..60 {
            let c = open_cell(&grid, p, &mut rng);
            let Some(v) = flow.integration(&grid, c) else {
                continue;
            };
            let (_, cost, end) = walk(&grid, p, |at| flow.next_cell(&grid, at), c);
            assert_eq!(end, goal, "cluster {cluster}");
            assert_eq!(
                cost, v,
                "cluster {cluster}: arrival cost equals the clustered value"
            );
        }
        // A stamp change: the synced flow equals one built fresh on the changed grid.
        let mut set: Vec<(u32, Stamp)> = stamps(&grid, &mut SplitMix64::new(0xc1u64 + cluster as u64), 400);
        set.truncate(300);
        set.push((9_000, Stamp::from_world(-200.0, 100.0, 30.0)));
        let sync = grid.sync_stamps(set);
        assert!(sync.changed);
        flow.on_sync(&grid, &sync);
        let mut fresh = ClusterFlow::build(&grid, p, &[goal], 1 << 20);
        for c in (0..n).step_by(3) {
            assert_eq!(
                flow.integration(&grid, c),
                fresh.integration(&grid, c),
                "cluster {cluster}: synced cell {c}"
            );
        }
    }
}
