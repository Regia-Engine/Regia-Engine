// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pathing/search.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 883-898, mark: as-is (Stage A: unchanged): `fine_cap_and_tick_budget_hold_on_256_and_grow_at_410`.
// Also ir-sim/src/pathing/tests.rs (same tag) lines 137-170, 209-258 and 519-545 (the grid-only planner proofs) and
// 675-690 (the tunnel planner proof, on the layer alone), mark: adapt: asserts unchanged; the fixtures build S-01's
// `HeightCells` (the same 1/32 m quantized heights) and the tunnel world's layout from `regia_world_geom::compat`
// (no game state: the layer is built straight from the sampled layout), `dist2` and the `terrain`/`wall` fixtures
// at lines 13-42 and mod.rs 537-541 come with them; unit names in comments made neutral.

use std::sync::Arc;

use regia_detsim::float::det_clamp;
use regia_detsim::math;
use regia_world_geom::compat::layout::{Junction, Opening, Passage, PassageKind, TunnelLayout};
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::compat::state::MapTunnels;

use super::*;
use crate::grid::{GridSettings, HeightCells, NavStatic};
use crate::layer::{self, PRESET_TUNNEL_SIZE_LIMIT, TunnelLayer, layer_for};

const SIDE: usize = 256;
const WIDTH: f32 = 1280.0;
const BASE: u16 = 16384;
const M_PER_COUNT: f32 = 1.0 / 32.0;

fn settings() -> Arc<GridSettings> {
    Arc::new(GridSettings::hover_ground_jet())
}

/// Raw height counts (1/32 m around `BASE`), as the source fixtures store them.
fn counts(cols: usize, rows: usize, cell_m: f32, height: &dyn Fn(f32, f32) -> f32, round: bool) -> Vec<u16> {
    let (half_x, half_z) = (cols as f32 * cell_m * 0.5, rows as f32 * cell_m * 0.5);
    let mut out = Vec::with_capacity(cols * rows);
    for z in 0..rows {
        for x in 0..cols {
            let wx = x as f32 * cell_m - half_x;
            let wz = z as f32 * cell_m - half_z;
            let raw = BASE as f32 + height(wx, wz) * 32.0;
            out.push(if round { raw.round() as u16 } else { raw as u16 });
        }
    }
    out
}

fn cells_from_counts(cols: usize, rows: usize, cell_m: f32, raw: &[u16]) -> HeightCells {
    let heights = raw.iter().map(|c| *c as f32 * M_PER_COUNT).collect();
    let mut cells = HeightCells::centered(cols, rows, cell_m, heights);
    // Absolute heights: 0 m is BASE counts.
    cells.center_height = BASE as f32 * M_PER_COUNT;
    cells
}

/// Heightfield from a function of world (x, z) in meters. 5 m cells over 1280 m.
fn terrain(height: impl Fn(f32, f32) -> f32) -> HeightCells {
    let raw = counts(
        SIDE,
        SIDE,
        WIDTH / SIDE as f32,
        &|x, z| det_clamp(height(x, z), -400.0, 400.0),
        true,
    );
    cells_from_counts(SIDE, SIDE, WIDTH / SIDE as f32, &raw)
}

/// A 60 m high wall along x = 0 from z = -`half_len` to z = +`half_len`, with an optional gap of
/// `gap` meters centered at z = `gap_z`.
fn wall(half_len: f32, gap: Option<(f32, f32)>) -> HeightCells {
    terrain(move |x, z| {
        let in_wall = x.abs() <= 7.5 && z.abs() <= half_len;
        let in_gap = gap.is_some_and(|(gz, w)| (z - gz).abs() <= w * 0.5);
        if in_wall && !in_gap { 60.0 } else { 0.0 }
    })
}

fn grid_of(ground: &HeightCells) -> NavGrid {
    NavGrid::new(Arc::new(
        NavStatic::build(Some(ground), settings()).expect("ground"),
    ))
    .expect("grid")
}

fn dist2(a: [f32; 2], b: [f32; 2]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[1] - b[1];
    math::sqrt(dx * dx + dz * dz)
}

// ------------------------------------------------------------------ search.rs 883-898

#[test]
fn fine_cap_and_tick_budget_hold_on_256_and_grow_at_410() {
    assert_eq!(fine_expansions_for(256, 256), MAX_FINE_EXPANSIONS);
    assert_eq!(fine_expansions_for(4, 4), MAX_FINE_EXPANSIONS);
    assert_eq!(fine_expansions_for(100, 80), MAX_FINE_EXPANSIONS);
    assert_eq!(fine_expansions_for(410, 410), 19_219);
    assert!(fine_expansions_for(500, 200) > MAX_FINE_EXPANSIONS);
    assert_eq!(path_tick_budget_for(256, 256), PATH_TICK_BUDGET);
    assert_eq!(path_tick_budget_for(8, 8), PATH_TICK_BUDGET);
    assert_eq!(path_tick_budget_for(410, 410), 9609);
}

// ------------------------------------------------------------------ tests.rs, the planner proofs

#[test]
fn the_planner_goes_round_a_wall_and_never_through_it() {
    let grid = grid_of(&wall(200.0, None));
    let mut scratch = Scratch::default();
    let plan = plan(&grid, &mut scratch, Profile::HOVER, [-100.0, 0.0], [100.0, 0.0]).expect("a route");
    assert_eq!(plan.kind, PlanKind::Full);
    for cell in &plan.cells {
        assert!(grid.passable(Profile::HOVER, *cell), "route enters a cliff cell");
    }
    let len: f32 = plan.waypoints.windows(2).map(|w| dist2(w[0], w[1])).sum();
    assert!(len > 350.0, "goes round the 400 m wall, length {len}");
    assert!(plan.fine_expansions < MAX_FINE_EXPANSIONS);
}

#[test]
fn an_unreachable_goal_ends_at_the_nearest_reachable_point() {
    // A ring of cliff round the goal.
    let grid = grid_of(&terrain(|x, z| {
        let r = math::sqrt(x * x + z * z);
        if (40.0..=55.0).contains(&r) { 80.0 } else { 0.0 }
    }));
    let mut scratch = Scratch::default();
    let plan = plan(&grid, &mut scratch, Profile::HOVER, [-300.0, 0.0], [0.0, 0.0]).expect("a partial route");
    assert_eq!(plan.kind, PlanKind::Partial);
    let end = *plan.waypoints.last().expect("waypoints");
    let r = math::sqrt(end[0] * end[0] + end[1] * end[1]);
    assert!(r > 55.0 && r < 75.0, "stops outside the ring, at {r} m");
}

/// On a rectangular map a wall across the whole short (Z) axis cannot be rounded: the planner must
/// return Partial. The same wall with a gap gives a Full route that threads the gap and stays inside
/// the narrow bounds.
#[test]
fn rectangular_map_blocks_and_threads_across_the_short_axis() {
    // 96x24 cells at 5 m -> 480 x 120 m; wall |x| <= 7.5 over all |z| <= 60.
    let (cols, rows) = (96usize, 24usize);
    let cell = 5.0f32;
    let (half_x, half_z) = ((cols as f32 * cell) * 0.5, (rows as f32 * cell) * 0.5);
    let build = |gap: Option<f32>| {
        let height = move |wx: f32, wz: f32| {
            let wall = wx.abs() <= 7.5 && wz.abs() <= half_z && !gap.is_some_and(|g| (wz - g).abs() <= 12.0);
            if wall { 60.0 } else { 0.0 }
        };
        grid_of(&cells_from_counts(
            cols,
            rows,
            cell,
            &counts(cols, rows, cell, &height, false),
        ))
    };
    // No gap: the goal is unreachable; the plan ends near the wall.
    let solid = build(None);
    let mut scratch = Scratch::default();
    let plan1 =
        plan(&solid, &mut scratch, Profile::HOVER, [-200.0, 0.0], [200.0, 0.0]).expect("a partial route");
    assert_eq!(
        plan1.kind,
        PlanKind::Partial,
        "a wall across all Z cannot be rounded"
    );
    // Gap at z = 0: a full route threads it, every cell in bounds.
    let gapped = build(Some(0.0));
    let mut scratch = Scratch::default();
    let plan = plan(&gapped, &mut scratch, Profile::HOVER, [-200.0, 0.0], [200.0, 0.0])
        .expect("a route through the gap");
    assert_eq!(plan.kind, PlanKind::Full);
    let n = gapped.width() * gapped.height();
    for cell_id in &plan.cells {
        assert!(
            (*cell_id as usize) < n,
            "cell {cell_id} outside the {cols}x{rows} grid"
        );
        let (x, z) = gapped.xz(*cell_id);
        assert!(x < cols && z < rows);
    }
    // The route crosses x = 0 through the gap band (cell centers).
    let crosses = plan.cells.iter().any(|&c| {
        let [wx, wz] = gapped.center(c);
        wx.abs() <= 10.0 && wz.abs() <= 15.0
    });
    assert!(crosses, "route never threads the gap: {:?}", plan.waypoints);
    for wp in &plan.waypoints {
        assert!(
            wp[0].abs() <= half_x && wp[1].abs() <= half_z,
            "waypoint {wp:?} off the map"
        );
    }
}

#[test]
fn each_movement_class_has_its_own_slope_limit() {
    // A 40 m shelf for x > 10 between z = -300 and 300; its long face rises 2.0 (63 degrees), its ends ramp 0.5.
    let grid = grid_of(&terrain(|x, z| {
        let face = det_clamp((x + 10.0) / 20.0, 0.0, 1.0);
        let ends = det_clamp((380.0 - z.abs()) / 80.0, 0.0, 1.0);
        40.0 * face * ends
    }));
    let mut scratch = Scratch::default();
    let len = |plan: &Plan| -> f32 { plan.waypoints.windows(2).map(|w| dist2(w[0], w[1])).sum() };
    let from = [-80.0, 0.0];
    let to = [80.0, 0.0];
    let ground = plan(&grid, &mut scratch, Profile::FOOT, from, to).expect("ground route");
    let jet = plan(&grid, &mut scratch, Profile::JET, from, to).expect("jet route");
    let hover = plan(&grid, &mut scratch, Profile::HOVER, from, to).expect("hover route");
    assert_eq!(ground.kind, PlanKind::Full);
    assert!(
        len(&ground) > 600.0,
        "a body on foot walks round by the end ramps: {}",
        len(&ground)
    );
    assert!(len(&jet) < 200.0, "a jetpack goes up the face: {}", len(&jet));
    // The face is 63 degrees. Fans lose grip near 38, so a hover hull takes the
    // end ramps with the bodies on foot instead of cutting the shelf.
    assert!(
        len(&hover) > 600.0,
        "a hover hull walks the ramps, not the 63 degree face: {}",
        len(&hover)
    );
    for cell in &ground.cells {
        assert!(grid.passable(Profile::FOOT, *cell));
    }
}

// ------------------------------------------------------------------ the tunnel world, on the layer alone

/// An 80 m wall along x = 0 from z = -400 to 400 on flat ground, and a tunnel under it: a tube from
/// x = -120 to 120 at y = -40, ramps up to the surface at x = -190 and 190, and a gallery that
/// leaves the tube at x = -60, loops north and rejoins at x = 60 (tests.rs 592-647, requirements:
/// the layout as data; the source's game state, carved open cuts and navmesh are not needed by the
/// layer).
pub(crate) fn tunnel_world() -> (NavGrid, TunnelLayer) {
    let height = |x: f32, z: f32| {
        if x.abs() <= 10.0 && z.abs() <= 400.0 {
            80.0
        } else {
            0.0
        }
    };
    let floor = -40.0;
    let layout = TunnelLayout {
        map_id: "test".into(),
        version: 1,
        sample_step_m: 5.0,
        passages: vec![
            Passage {
                id: "tube".into(),
                kind: PassageKind::Tube,
                width_m: 22.0,
                ceiling_m: 12.0,
                points: vec![[-120.0, floor, 0.0], [0.0, floor, 0.0], [120.0, floor, 0.0]],
            },
            Passage {
                id: "gallery".into(),
                kind: PassageKind::Gallery,
                width_m: 18.0,
                ceiling_m: 10.0,
                points: vec![
                    [-60.0, floor, 0.0],
                    [-40.0, floor, 60.0],
                    [40.0, floor, 60.0],
                    [60.0, floor, 0.0],
                ],
            },
            Passage {
                id: "ramp_w".into(),
                kind: PassageKind::Ramp,
                width_m: 20.0,
                ceiling_m: 12.0,
                points: vec![[-120.0, floor, 0.0], [-190.0, 0.0, 0.0]],
            },
            Passage {
                id: "ramp_e".into(),
                kind: PassageKind::Ramp,
                width_m: 20.0,
                ceiling_m: 12.0,
                points: vec![[120.0, floor, 0.0], [190.0, 0.0, 0.0]],
            },
        ],
        openings: vec![
            Opening {
                id: "w".into(),
                kind: "skylight".into(),
                ramp: "ramp_w".into(),
                top: [-190.0, 0.0, 0.0],
            },
            Opening {
                id: "e".into(),
                kind: "skylight".into(),
                ramp: "ramp_e".into(),
                top: [190.0, 0.0, 0.0],
            },
        ],
        junctions: vec![
            Junction {
                id: "jw".into(),
                at: [-60.0, floor, 0.0],
                passages: vec!["tube".into(), "gallery".into()],
            },
            Junction {
                id: "je".into(),
                at: [60.0, floor, 0.0],
                passages: vec!["tube".into(), "gallery".into()],
            },
        ],
        crawlways: Vec::new(),
    };
    let raw = counts(SIDE, SIDE, WIDTH / SIDE as f32, &height, true);
    let sampler = TerrainSampler::new(HeightGrid::square(SIDE, raw.clone()).expect("square grid"), WIDTH)
        .with_zero_counts(u32::from(BASE));
    let sampled = layout.sample(Some(&sampler));
    let map = MapTunnels { layout, sampled };
    let stat = Arc::new(
        NavStatic::build(
            Some(&cells_from_counts(SIDE, SIDE, WIDTH / SIDE as f32, &raw)),
            settings(),
        )
        .expect("ground"),
    );
    let (tunnels, cut) = TunnelLayer::build(&map, &stat);
    (NavGrid::with_closed_cells(stat, cut).expect("cut mask"), tunnels)
}

#[test]
fn the_planner_takes_the_tunnel_when_it_is_shorter() {
    let (grid, tunnels) = tunnel_world();
    assert!(!tunnels.is_empty(), "the tunnel layer is built");
    let mut scratch = Scratch::default();
    let for_hover = layer_for(&grid, Some(&tunnels), Profile::HOVER, PRESET_TUNNEL_SIZE_LIMIT);
    let plan = plan_layered(
        &grid,
        for_hover,
        &mut scratch,
        Profile::HOVER,
        [-250.0, 0.0],
        None,
        [250.0, 0.0],
        None,
        layer::TUNNEL_COST_PCT,
    )
    .expect("a route");
    assert_eq!(plan.kind, PlanKind::Full);
    assert!(plan.layers.iter().any(|t| *t), "the route goes underground");
    let len: f32 = plan.waypoints.windows(2).map(|w| dist2(w[0], w[1])).sum();
    assert!(len < 560.0, "through the tunnel, not 900 m round the wall: {len}");
    // A large hull does not fit: it goes round.
    let for_large = layer_for(
        &grid,
        Some(&tunnels),
        Profile::HOVER_LARGE,
        PRESET_TUNNEL_SIZE_LIMIT,
    );
    assert!(for_large.is_none());
    let big = plan_layered(
        &grid,
        for_large,
        &mut scratch,
        Profile::HOVER_LARGE,
        [-250.0, 0.0],
        None,
        [250.0, 0.0],
        None,
        layer::TUNNEL_COST_PCT,
    )
    .expect("a route");
    assert!(big.layers.iter().all(|t| !*t));
}

// ------------------------------------------------------------------ new (plan row S-02)

/// The layer's shape on the tunnel world: two portals on open ground just past the ramp tops, the
/// gallery joined to the tube at two junctions, open-cut cells closed beside the ramps, the portal
/// cells themselves open, and the large class kept off it.
#[test]
fn the_tunnel_layer_joins_its_passages_and_opens_only_at_the_portals() {
    let (grid, tunnels) = tunnel_world();
    assert_eq!(tunnels.portals.len(), 2, "{:?}", tunnels.portals);
    for (node, cell) in &tunnels.portals {
        let n = tunnels.nodes[*node as usize];
        assert!(n.ramp, "a portal sits on a ramp top");
        let [x, z] = grid.center(*cell);
        assert!(
            x.abs() > 190.0 && x.abs() < 200.0 && z.abs() <= 5.0,
            "portal cell at {x}, {z}"
        );
        assert!(grid.passable(Profile::HOVER, *cell), "the portal cell stays open");
        assert_eq!(tunnels.portal_cell(*node), Some(*cell));
        assert_eq!(tunnels.portals_at(*cell).collect::<Vec<_>>(), vec![*node]);
    }
    assert!(
        tunnels.junctions.len() >= 2,
        "gallery ends join the tube: {:?}",
        tunnels.junctions
    );
    for row in &tunnels.adj {
        assert!(row.windows(2).all(|w| w[0] < w[1]), "ascending neighbor ids");
        assert!(row.iter().all(|(_, c)| *c >= 1));
    }
    let closed = (0..grid.width() * grid.height())
        .filter(|i| grid.closed[*i] != 0)
        .count();
    assert!(closed > 0, "the ramp cuts close surface cells");
    // A cell over the west ramp's open cut, short of the top: closed.
    let over_cut = grid.cell_at(-175.0, 0.0);
    assert!(!grid.passable(Profile::HOVER, over_cut));
    assert!(!grid.passable(Profile::FOOT, over_cut));
    // Clones carry the closed set; equality reads it.
    tunnels.set_closed(vec![3, 1, 3]);
    let copy = tunnels.clone();
    assert_eq!(copy.closed(), vec![1, 3]);
    assert_eq!(copy, tunnels);
    copy.set_closed(Vec::new());
    assert_ne!(copy, tunnels);
}

/// `locate` finds a node for a body underground and on a ramp floor, none for one on the surface.
#[test]
fn locate_attaches_underground_and_on_ramps_only() {
    let (_, tunnels) = tunnel_world();
    let in_tube = tunnels.locate([0.0, -40.0, 0.0], 80.0).expect("in the tube");
    assert!(!tunnels.nodes[in_tube as usize].ramp);
    assert_eq!(tunnels.locate([0.0, 0.0, 300.0], 0.0), None, "open ground");
    let ramp = tunnels
        .nodes
        .iter()
        .position(|n| n.ramp && n.y > -10.0)
        .expect("a high ramp node");
    let n = tunnels.nodes[ramp];
    let on = tunnels
        .locate([n.x, n.y + 0.5, n.z], 0.0)
        .expect("on the ramp floor");
    assert!(tunnels.nodes[on as usize].ramp);
    let mut levels = tunnels.clone();
    levels.set_levels(&[(0, -2), (u32::MAX, -3)]);
    assert_eq!(levels.nodes[0].level, -2);
    assert!(levels.nodes[1..].iter().all(|n| n.level == -1));
}

/// A body that starts underground plans out of the tube; a goal underground ends on its node.
#[test]
fn a_tunnel_start_and_goal_plan_through_the_portals() {
    let (grid, tunnels) = tunnel_world();
    let mut scratch = Scratch::default();
    let from = tunnels.locate([0.0, -40.0, 0.0], 80.0).expect("in the tube");
    let out = plan_layered(
        &grid,
        Some(&tunnels),
        &mut scratch,
        Profile::FOOT,
        [0.0, 0.0],
        Some(from),
        [300.0, 100.0],
        None,
        layer::TUNNEL_COST_PCT,
    )
    .expect("a route");
    assert_eq!(out.kind, PlanKind::Full);
    assert!(out.layers[0], "starts underground");
    assert!(!*out.layers.last().expect("layers"), "ends on the surface");
    assert_eq!(out.waypoints.len(), out.layers.len());
    let back = plan_layered(
        &grid,
        Some(&tunnels),
        &mut scratch,
        Profile::FOOT,
        [300.0, 100.0],
        None,
        [0.0, 0.0],
        Some(from),
        layer::FLANK_COST_PCT,
    )
    .expect("a route");
    assert!(*back.layers.last().expect("layers"), "ends on the tube node");
    assert_eq!(
        *back.cells.last().expect("cells"),
        (grid.width() * grid.height()) as u32 + from
    );
    // A tunnel node past the layer is ignored (treated as a surface point).
    let stray = plan_layered(
        &grid,
        Some(&tunnels),
        &mut scratch,
        Profile::FOOT,
        [300.0, 100.0],
        Some(u32::MAX),
        [320.0, 100.0],
        None,
        layer::TUNNEL_COST_PCT,
    )
    .expect("a route");
    assert!(stray.layers.iter().all(|t| !*t));
}

/// The scratch buffers are sized by the graph and reused; a clone starts empty and plans the same.
#[test]
fn scratch_is_reused_and_a_clone_starts_empty() {
    let grid = grid_of(&wall(200.0, Some((0.0, 30.0))));
    let mut scratch = Scratch::default();
    let a = plan(
        &grid,
        &mut scratch,
        Profile::HOVER,
        [-100.0, 50.0],
        [100.0, -50.0],
    )
    .expect("a route");
    let lens = scratch.leak_lens();
    assert!(lens.iter().all(|(_, n)| *n > 0));
    let mut fresh = scratch.clone();
    assert!(fresh.leak_lens().iter().all(|(_, n)| *n == 0));
    let b = plan(&grid, &mut fresh, Profile::HOVER, [-100.0, 50.0], [100.0, -50.0]).expect("a route");
    let c = plan(
        &grid,
        &mut scratch,
        Profile::HOVER,
        [-100.0, 50.0],
        [100.0, -50.0],
    )
    .expect("a route");
    assert_eq!(a.waypoints, b.waypoints);
    assert_eq!(a.cells, c.cells);
    assert_eq!(scratch.leak_lens(), lens, "same graph, same lengths");
}

/// A goal under an obstacle disc snaps to the nearest open cell; a start under one escapes first.
#[test]
fn a_blocked_goal_snaps_and_a_blocked_start_escapes() {
    let mut grid = NavGrid::new(Arc::new(NavStatic::build(None, settings()).expect("canvas"))).expect("grid");
    grid.sync_stamps(vec![(1, crate::grid::Stamp::from_world(0.0, 0.0, 12.0))]);
    assert!(!grid.passable(Profile::HOVER, grid.cell_at(0.0, 0.0)));
    let mut scratch = Scratch::default();
    let out = plan(&grid, &mut scratch, Profile::HOVER, [0.0, 0.0], [-100.0, 0.0]).expect("a route");
    assert_eq!(out.kind, PlanKind::Full);
    assert!(!out.snapped);
    assert!(
        out.waypoints.len() >= 3,
        "start, the escape cell, the goal: {:?}",
        out.waypoints
    );
    assert_eq!(out.waypoints[0], [0.0, 0.0]);
    let escape = grid.cell_at(out.waypoints[1][0], out.waypoints[1][1]);
    assert!(
        grid.passable(Profile::HOVER, escape),
        "the second waypoint is the open escape cell"
    );
    assert_eq!(
        *out.waypoints.last().expect("waypoints"),
        [-100.0, 0.0],
        "an exact goal"
    );
    let to_disc = plan(&grid, &mut scratch, Profile::HOVER, [-100.0, 0.0], [0.0, 0.0]).expect("a route");
    assert!(to_disc.snapped);
    let end = *to_disc.waypoints.last().expect("waypoints");
    assert!(
        grid.passable(Profile::HOVER, grid.cell_at(end[0], end[1])),
        "ends beside the disc, not in it: {end:?}"
    );
}

/// Serpentine walls: 30 walls 60 m high across the map, 40 m apart, each with its gap at the other
/// end, so the route snakes through every lane (about 38 km of cells).
fn maze() -> NavGrid {
    grid_of(&terrain(|x, z| {
        for k in 0..30 {
            let wx = -600.0 + 40.0 * k as f32;
            let gap = if k % 2 == 0 { z > 600.0 } else { z < -600.0 };
            if (x - wx).abs() <= 7.5 && !gap {
                return 60.0;
            }
        }
        0.0
    }))
}

/// A route past the cell search's expansion cap falls back to the coarse route: straight legs between
/// region centers, every one on open ground, the whole cap spent and nothing more.
#[test]
fn a_maze_past_the_expansion_cap_falls_back_to_the_coarse_route() {
    let grid = maze();
    let mut scratch = Scratch::default();
    let out = plan(&grid, &mut scratch, Profile::HOVER, [-620.0, 0.0], [620.0, 0.0]).expect("a route");
    assert_eq!(
        out.kind,
        PlanKind::Coarse,
        "fine {} coarse {}",
        out.fine_expansions,
        out.coarse_expansions
    );
    assert_eq!(
        out.fine_expansions, MAX_FINE_EXPANSIONS,
        "the whole cap, then no second corridor"
    );
    assert!(out.cells.is_empty(), "no cell path");
    assert_eq!(out.work, out.fine_expansions + out.coarse_expansions * 4 + 16);
    assert_eq!(out.waypoints.first(), Some(&[-620.0, 0.0]));
    assert_eq!(out.waypoints.len(), out.layers.len());
    assert!(
        out.waypoints.len() > 60,
        "a leg per region along the snake: {}",
        out.waypoints.len()
    );
    for w in &out.waypoints[1..] {
        assert!(
            grid.passable(Profile::HOVER, grid.cell_at(w[0], w[1])),
            "waypoint {w:?} on a wall"
        );
    }
    let last = *out.waypoints.last().expect("waypoints");
    assert_eq!(
        grid.cell_at(last[0], last[1]),
        grid.cell_at(620.0, 0.0),
        "ends on the goal cell"
    );
    // The same request in a fresh scratch gives the same bits.
    let again = plan(
        &grid,
        &mut Scratch::default(),
        Profile::HOVER,
        [-620.0, 0.0],
        [620.0, 0.0],
    )
    .expect("a route");
    assert_eq!(plan_digest(Some(&again)), plan_digest(Some(&out)));
}

// ------------------------------------------------------------------ acceptance: determinism and the pin

use regia_detsim::golden::scenarios::SplitMix64;
use regia_detsim::golden::{self, DetRun, DetScenario, GoldenMode, REPORT_PREFIX, StateHasher};
use regia_detsim::tick::TickRate;

use crate::grid::Stamp;

/// The grids the seeded plans run on: S-01's fixtures (flat canvas, a wall with a gap and two
/// building stamps, a cliff ring, the slope shelf, a narrow rectangular map), the tunnel world and a
/// serpentine maze that runs the cell search past its cap (coarse plans).
struct Fixture {
    grid: NavGrid,
    layer: Option<TunnelLayer>,
}

/// Index of the maze in [`fixtures`].
const MAZE: usize = 6;

fn fixtures() -> Vec<Fixture> {
    let flat = NavGrid::new(Arc::new(NavStatic::build(None, settings()).expect("canvas"))).expect("grid");
    let mut walled = grid_of(&wall(300.0, Some((40.0, 40.0))));
    walled.sync_stamps(vec![
        (60, Stamp::from_world(-60.0, 30.0, 9.0)),
        (61, Stamp::from_world(0.0, 70.0, 12.0)),
    ]);
    let ring = grid_of(&terrain(|x, z| {
        let r = math::sqrt(x * x + z * z);
        if (40.0..=55.0).contains(&r) { 80.0 } else { 0.0 }
    }));
    let shelf = grid_of(&terrain(|x, z| {
        let face = det_clamp((x + 10.0) / 20.0, 0.0, 1.0);
        let ends = det_clamp((380.0 - z.abs()) / 80.0, 0.0, 1.0);
        40.0 * face * ends
    }));
    let (cols, rows) = (96usize, 24usize);
    let narrow_height = |wx: f32, wz: f32| {
        if wx.abs() <= 7.5 && wz.abs() > 12.0 {
            60.0
        } else {
            0.0
        }
    };
    let narrow = grid_of(&cells_from_counts(
        cols,
        rows,
        5.0,
        &counts(cols, rows, 5.0, &narrow_height, false),
    ));
    let (tgrid, tunnels) = tunnel_world();
    let maze = maze();
    vec![
        Fixture {
            grid: flat,
            layer: None,
        },
        Fixture {
            grid: walled,
            layer: None,
        },
        Fixture {
            grid: ring,
            layer: None,
        },
        Fixture {
            grid: shelf,
            layer: None,
        },
        Fixture {
            grid: narrow,
            layer: None,
        },
        Fixture {
            grid: tgrid,
            layer: Some(tunnels),
        },
        Fixture {
            grid: maze,
            layer: None,
        },
    ]
}

/// FNV-1a 64 of everything a plan reports, floats by their canonical bits.
fn plan_digest(plan: Option<&Plan>) -> u64 {
    let mut h = StateHasher::new();
    let Some(p) = plan else {
        h.u32("none", 1);
        return h.finish();
    };
    h.u32("kind", p.kind as u32);
    h.bool("snapped", p.snapped);
    h.u32("work", p.work);
    h.u32("fine", p.fine_expansions);
    h.u32("coarse", p.coarse_expansions);
    h.u32("cells", p.cells.len() as u32);
    for c in &p.cells {
        h.u32("cell", *c);
    }
    h.u32("waypoints", p.waypoints.len() as u32);
    for (w, l) in p.waypoints.iter().zip(&p.layers) {
        h.f32("x", w[0]);
        h.f32("z", w[1]);
        h.bool("under", *l);
    }
    h.finish()
}

/// 256 seeded `plan` / `plan_layered` calls, one per tick, over [`fixtures`] with one shared
/// [`Scratch`]: random profiles, endpoints (some off the grid or in a cliff), tunnel starts and goals,
/// tunnel shares (the clamp included), and the tube closed and reopened on the tunnel world.
struct SearchScenario {
    fixtures: Vec<Fixture>,
    scratch: Scratch,
    rng: SplitMix64,
    last: u64,
    acc: u64,
    kinds: [u32; 4],
    underground: u32,
}

impl SearchScenario {
    fn new() -> Self {
        SearchScenario {
            fixtures: fixtures(),
            scratch: Scratch::default(),
            rng: SplitMix64::new(0),
            last: 0,
            acc: 0,
            kinds: [0; 4],
            underground: 0,
        }
    }

    fn point(&mut self, grid: &NavGrid) -> [f32; 2] {
        // Up to 5 % past the edge, so some points clamp onto the border.
        let hx = grid.width() as f32 * grid.cell_m() * 0.525;
        let hz = grid.height() as f32 * grid.cell_m() * 0.525;
        [self.rng.signed() * hx, self.rng.signed() * hz]
    }
}

impl DetScenario for SearchScenario {
    fn name(&self) -> &str {
        "search_plans"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(0x5eed_5302, 256, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        self.rng = SplitMix64::new(run.seed);
        self.scratch = Scratch::default();
        self.last = 0;
        self.acc = 0;
        self.kinds = [0; 4];
        self.underground = 0;
        for f in &self.fixtures {
            if let Some(l) = &f.layer {
                l.set_closed(Vec::new());
            }
        }
    }

    fn step(&mut self, tick: u32) {
        let which = (self.rng.next_u64() % self.fixtures.len() as u64) as usize;
        let profile = Profile((self.rng.next_u64() % 5) as u8);
        let grid = self.fixtures[which].grid.clone();
        let mut from = self.point(&grid);
        let mut goal = self.point(&grid);
        if which == MAZE {
            // Opposite ends of the snake, so the cell search runs past its cap (coarse plans).
            from[0] = -625.0 + 10.0 * self.rng.signed();
            goal[0] = 625.0 + 10.0 * self.rng.signed();
        }
        let pct = [TUNNEL_COST_PCT, layer::FLANK_COST_PCT, 5, 100][(self.rng.next_u64() % 4) as usize];
        let pick = self.rng.next_u64();
        let plan = match &self.fixtures[which].layer {
            Some(tunnels) => {
                // The tube closes for ticks 64-127 and 192-255.
                let tube: Vec<u32> = if (tick / 64) % 2 == 1 {
                    tunnels
                        .nodes
                        .iter()
                        .enumerate()
                        .filter(|(_, n)| !n.ramp)
                        .map(|(i, _)| i as u32)
                        .collect()
                } else {
                    Vec::new()
                };
                tunnels.set_closed(tube);
                let m = tunnels.len() as u64;
                let from_tunnel = pick.is_multiple_of(4).then_some(((pick >> 8) % (m + 2)) as u32);
                let goal_tunnel = (pick % 5 == 1).then_some(((pick >> 24) % m) as u32);
                let layer = layer_for(&grid, Some(tunnels), profile, PRESET_TUNNEL_SIZE_LIMIT);
                plan_layered(
                    &grid,
                    layer,
                    &mut self.scratch,
                    profile,
                    from,
                    from_tunnel,
                    goal,
                    goal_tunnel,
                    pct,
                )
            }
            None if pick.is_multiple_of(2) => plan(&grid, &mut self.scratch, profile, from, goal),
            None => plan_layered(
                &grid,
                None,
                &mut self.scratch,
                profile,
                from,
                None,
                goal,
                None,
                pct,
            ),
        };
        if let Some(p) = &plan {
            self.kinds[p.kind as usize] += 1;
            self.underground += u32::from(p.layers.iter().any(|t| *t));
        } else {
            self.kinds[3] += 1;
        }
        self.last = plan_digest(plan.as_ref());
        self.acc = (self.acc ^ self.last).wrapping_mul(0x100_0000_01b3);
    }

    fn hash_state(&self, h: &mut StateHasher) {
        h.u64("plan", self.last);
        h.u64("acc", self.acc);
        for k in self.kinds {
            h.u32("kind_count", k);
        }
        h.u32("underground", self.underground);
    }
}

/// DO27's search determinism (`two_runs_give_the_same_hash`, tests.rs 429-457, requirements): the
/// seeded plans give the same hash per tick on two fresh runs, and the run covers full, partial and
/// underground plans.
#[test]
fn two_runs_give_the_same_hash() {
    let mut a = SearchScenario::new();
    let run = a.default_run();
    let first = golden::record(&mut a, &run);
    let second = golden::record(&mut SearchScenario::new(), &run);
    assert_eq!(first.tick_hashes.len(), 256);
    assert_eq!(
        golden::first_divergence(&first.tick_hashes, &second.tick_hashes),
        None
    );
    assert_eq!(first.run_hash(), second.run_hash());
    // The same scenario object replayed after a reset (warm scratch, reopened tube) agrees too.
    let again = golden::record(&mut a, &run);
    assert_eq!(again.tick_hashes, first.tick_hashes);
    let [full, partial, coarse, none] = a.kinds;
    println!(
        "kinds full {full} partial {partial} coarse {coarse} none {none} underground {}",
        a.underground
    );
    assert!(full > 100, "most plans are full: {:?}", a.kinds);
    assert!(
        partial > 0,
        "the ring and the blocked tube give partial plans: {:?}",
        a.kinds
    );
    assert!(a.underground > 0, "some plans go underground");
    assert!(coarse > 0, "the maze gives coarse plans: {:?}", a.kinds);
}

/// The requirement of DO27's `a_collapse_closes_its_passage_and_clearing_reopens_it` (tests.rs
/// 1195-1238) on the layer alone: open, the plan goes underground; every tube node closed, it goes
/// round the wall; cleared, it is back in the tube (and bit for bit the first plan).
#[test]
fn plan_layered_takes_the_tube_and_routes_round_when_closed() {
    let (grid, tunnels) = tunnel_world();
    let route = |tunnels: &TunnelLayer| {
        let mut scratch = Scratch::default();
        plan_layered(
            &grid,
            Some(tunnels),
            &mut scratch,
            Profile::HOVER,
            [-250.0, 0.0],
            None,
            [250.0, 0.0],
            None,
            layer::TUNNEL_COST_PCT,
        )
        .expect("a route")
    };
    let open = route(&tunnels);
    assert!(open.layers.iter().any(|t| *t), "through the tube first");
    let tube: Vec<u32> = tunnels
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| !n.ramp)
        .map(|(i, _)| i as u32)
        .collect();
    assert!(!tube.is_empty());
    // Replaced whole, sorted: hand it in reverse.
    tunnels.set_closed(tube.iter().rev().copied().collect());
    assert_eq!(tunnels.closed(), tube, "whole-list replace, sorted");
    assert!(tube.iter().all(|t| tunnels.is_closed(*t)));
    let round = route(&tunnels);
    assert!(
        round.layers.iter().all(|t| !*t),
        "no way underground: round the wall, kind {:?}",
        round.kind
    );
    let len = |p: &Plan| -> f32 { p.waypoints.windows(2).map(|w| dist2(w[0], w[1])).sum() };
    assert!(
        len(&round) > len(&open),
        "round the wall is longer: {} vs {}",
        len(&round),
        len(&open)
    );
    // Blown clear: the passage is back.
    tunnels.set_closed(Vec::new());
    assert!(tunnels.closed().is_empty());
    let back = route(&tunnels);
    assert!(back.layers.iter().any(|t| *t), "through the tube again");
    assert_eq!(
        plan_digest(Some(&back)),
        plan_digest(Some(&open)),
        "the same plan as before the closing"
    );
}

/// Item 3 of plan row S-02: the seeded plans pinned by a determinism golden
/// (`goldens/search_plans.detgold`, ADR-8046 format). Every target must hash to the committed
/// x86-64 Linux run; `REGIA_BLESS=1` records this target's line. Prints one
/// `regia-det-golden {json}` line.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "test harness: where the pinned golden lives, as regia_detsim::golden::engine_golden_dir; no sim tick reads the environment (ADR-8367, ADR-8877)"
)]
fn search_plans_pinned_on_every_arch() {
    // det-scan: allow(env_read): test harness: where the pinned golden lives, not sim input
    let dir = match std::env::var_os(golden::GOLDEN_DIR_ENV) {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("goldens"),
    };
    let mut scenario = SearchScenario::new();
    let run = scenario.default_run();
    let trace = golden::record(&mut scenario, &run);
    let report = golden::check_trace(
        &trace,
        &golden::golden_path(&dir, "search_plans"),
        &golden::current_target(),
        GoldenMode::from_env(),
    );
    println!("{REPORT_PREFIX}{}", report.to_json());
    assert!(report.passed(), "{report}");
}

// ------------------------------------------------------------------ scale (ADR-8295)

/// ADR-8295: the search at the default cluster keeps S-02's pins: the fixtures run at 8 x 8 with
/// one-byte labels, two runs agree, and the run hashes to the committed `search_plans` golden.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "test harness: where the pinned golden lives, as regia_detsim::golden::engine_golden_dir; no sim tick reads the environment (ADR-8367, ADR-8877)"
)]
fn default_cluster_keeps_the_search_pins() {
    assert_eq!(settings().cluster_cells, 8);
    for f in fixtures() {
        assert_eq!(f.grid.cluster_cells(), 8);
        assert_eq!(f.grid.max_regions(), 64);
        assert!(f.grid.wide_labels.is_empty());
    }
    let mut a = SearchScenario::new();
    let run = a.default_run();
    let first = golden::record(&mut a, &run);
    let second = golden::record(&mut SearchScenario::new(), &run);
    assert_eq!(first.tick_hashes, second.tick_hashes);
    // The det-golden legs run this binary away from the source tree (ADR-8838).
    // det-scan: allow(env_read): test harness: where the pinned golden lives, not sim input
    let dir = match std::env::var_os(golden::GOLDEN_DIR_ENV) {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("goldens"),
    };
    let report = golden::check_trace(
        &first,
        &golden::golden_path(&dir, "search_plans"),
        &golden::current_target(),
        GoldenMode::Check,
    );
    assert!(report.passed(), "{report}");
}

/// A fixture grid rebuilt at another cluster size (the same ground, stamps and closed cells).
fn at_cluster(grid: &NavGrid, k: usize) -> NavGrid {
    let s = Arc::new(GridSettings {
        cluster_cells: k,
        ..grid.settings().clone()
    });
    let st = &grid.stat;
    let stat = NavStatic {
        width: st.width,
        height: st.height,
        cell_m: st.cell_m,
        origin_x: st.origin_x,
        origin_z: st.origin_z,
        cost: st.cost.clone(),
        edges: st.edges.clone(),
        steep: st.steep.clone(),
        height_cells: st.height_cells.clone(),
        lowered: st.lowered.clone(),
        key: st.key,
        settings: s,
    };
    let mut g = NavGrid::with_closed_cells(Arc::new(stat), grid.closed.to_vec()).expect("grid");
    g.sync_stamps(grid.stamps().to_vec());
    g
}

/// ADR-8295: a search that reads its region neighbors from a [`RegionGraph`] gives the bits of the
/// border scan, on every surface fixture and at clusters of 8, 16 and 32; the route cache with one
/// request per key changes nothing either.
#[test]
fn region_graph_search_equals_the_border_scan_search() {
    let mut rng = SplitMix64::new(0x5eed_8295);
    let mut compared = 0u32;
    for (which, f) in fixtures().into_iter().enumerate() {
        if f.layer.is_some() {
            continue;
        }
        for k in [8usize, 16, 32] {
            let grid = if k == 8 {
                f.grid.clone()
            } else {
                at_cluster(&f.grid, k)
            };
            assert_eq!(grid.cluster_cells(), k);
            let graph = RegionGraph::build(&grid);
            let (mut s1, mut s2) = (Scratch::default(), Scratch::default());
            for _ in 0..24 {
                let hx = grid.width() as f32 * grid.cell_m() * 0.5;
                let hz = grid.height() as f32 * grid.cell_m() * 0.5;
                let from = [rng.signed() * hx, rng.signed() * hz];
                let goal = [rng.signed() * hx, rng.signed() * hz];
                let profile = Profile((rng.next_u64() % 5) as u8);
                let direct = plan(&grid, &mut s1, profile, from, goal);
                let args = PlanArgs {
                    layer: None,
                    profile,
                    from,
                    from_tunnel: None,
                    goal,
                    goal_tunnel: None,
                    tunnel_pct: layer::TUNNEL_COST_PCT,
                };
                let mut cache = RouteCache::new();
                let tabled = plan_core(&grid, Some(&graph), &mut s2, &args, Some(&mut cache));
                assert!(tabled.as_ref().is_none_or(|(_, shared)| !shared));
                assert_eq!(
                    plan_digest(direct.as_ref()),
                    plan_digest(tabled.as_ref().map(|(p, _)| p)),
                    "fixture {which} cluster {k}: {from:?} -> {goal:?}"
                );
                compared += 1;
            }
        }
    }
    assert!(compared >= 6 * 3 * 24);
}

/// Fine-step cost of a plan's cell path (the search's own rule: 10 straight, 14 diagonal, times
/// the cell cost over `COST_FLAT`).
fn cell_path_cost(grid: &NavGrid, profile: Profile, cells: &[u32]) -> u32 {
    cells
        .windows(2)
        .map(|w| {
            let (a, b) = (w[0] as usize, w[1] as usize);
            let (ax, az) = (a % grid.width(), a / grid.width());
            let (bx, bz) = (b % grid.width(), b / grid.width());
            let base = if ax != bx && az != bz { 14 } else { 10 };
            base * grid.cost(profile, w[1]) / COST_FLAT
        })
        .sum()
}

/// KI-WD-031 (ADR-10070): with an area cheaper than flat ground the cell heuristic scales itself, so
/// the search finds the cheaper route along a road 7 rows off the straight line (7 diagonals down,
/// 31 road steps at half cost, 7 diagonals up: at most 351) instead of the straight line (450).
#[test]
fn a_cheap_area_keeps_the_cell_heuristic_admissible() {
    use crate::grid::{AreaDef, PaintRegion, ProfileMask};

    let mut s = GridSettings::three_class();
    s.canvas_cells = [64, 64];
    s.cell_m = 5.0;
    s.areas.push(AreaDef {
        name: "road".into(),
        cost_pct: 50,
        blocked_for: ProfileMask::NONE,
    });
    let stat = NavStatic::build(None, Arc::new(s)).expect("valid settings");
    let mut grid = NavGrid::new(Arc::new(stat)).expect("grid");
    let road = grid.settings().area_id("road").expect("road");
    grid.paint_area(
        &PaintRegion::CellRect {
            min: [0, 17],
            max: [63, 17],
        },
        road,
    )
    .expect("paint");
    let profile = Profile::LOW_CLIMB_MEDIUM;
    let from = grid.center(grid.index(5, 24));
    let goal = grid.center(grid.index(50, 24));
    let mut scratch = Scratch::default();
    let plan = plan(&grid, &mut scratch, profile, from, goal).expect("a plan");
    assert_eq!(plan.kind, PlanKind::Full);
    let cost = cell_path_cost(&grid, profile, &plan.cells);
    let on_road = plan.cells.iter().filter(|c| grid.area_of(**c) == road).count();
    assert!(
        cost <= 351 && on_road >= 20,
        "the cheapest route runs along the road: cost {cost} (straight line 450), {on_road} road cells"
    );

    // Without cheap areas the scale is off, so the default grids keep their plan bits.
    let plain = NavGrid::new(Arc::new(
        NavStatic::build(None, settings()).expect("valid settings"),
    ))
    .expect("grid");
    assert_eq!(area_diag(&plain), None);
    assert_eq!(area_diag(&grid), Some(7));
}
