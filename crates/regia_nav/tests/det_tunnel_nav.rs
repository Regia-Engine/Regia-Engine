// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pathing/tests.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 13-32 (`terrain`), 101-105 (`d2`), 592-669 (`tunnel_world`, `ground_pass`), 675-690, 863-963 and 1191-1238,
// mark: as-is (asserts unchanged); 786-834, mark: adapt (asserts kept, read on the planner instead of a stepped
// sim: see each test). Also ir-sim/src/tunnels.rs lines 470-524 (`carve_open_cuts`, the test fixture; the
// compat layer's copy is `#[cfg(test)]` in its own crate), mark: as-is, and ir-core/tests/tunnels_switchbacks.rs
// lines 43-79, mark: adapt (the navmesh half added). All three files are byte-identical between regia-absorb-2
// (e6c84ba6d) and regia-absorb-3.
// Edits: module paths (the compat layer `regia_world_geom::compat`, the grid, layer, search and navmesh
// `regia_nav::det`); the game state is a harness (`TunnelWorld`: the carved terrain, `TunnelsState<NavMesh>`, the
// grid and the tunnel layer built from them the way the source's `nav_prepare` builds them); the grid's size
// class rule is `layer_for` (the source's `grid.tunnel_for`), its settings `GridSettings::hover_ground_jet`; the
// grid's funnel flag is S-01's `lowered`; unit, map and studio names in comments made neutral.

//! The tunnel half of the deterministic navigation on the compat tunnels (plan row W-04): the
//! navmesh bake behind `TunnelNavBake`, the tunnel route proofs on the grid, the layer and the mesh,
//! and the pins.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use regia_detsim::golden::scenarios::SplitMix64;
use regia_detsim::golden::{self, DetRun, DetScenario, GoldenMode, REPORT_PREFIX, StateHasher};
use regia_detsim::math;
use regia_detsim::tick::TickRate;
use regia_nav::det::grid::{GridSettings, NavGrid, NavStatic, Profile};
use regia_nav::det::layer::{self, PRESET_TUNNEL_SIZE_LIMIT, TunnelLayer, layer_for};
use regia_nav::det::navmesh::{
    MESH_LOCATE_DROPS, MeshAgent, MeshPlan, NavMesh, PolyKind, RouteKind, route_for,
};
use regia_nav::det::search::{self, Plan, PlanKind, Scratch};
use regia_nav::det::tunnel_bake::{NavTunnelsState, TunnelNavMeshBake, navmesh_ground};
use regia_world_geom::compat::fixtures::switchbacks;
use regia_world_geom::compat::layout::{Junction, Opening, Passage, PassageKind, TunnelLayout};
use regia_world_geom::compat::mesh::{clamp_altitude_under_ceiling, separate_disc_from_walls_band};
use regia_world_geom::compat::pack::parse_tunnels;
use regia_world_geom::compat::sampler::{FloorSurface, HeightGrid, TerrainSampler};
use regia_world_geom::compat::state::{NoNavBake, TunnelNavBake, TunnelsState, UNIT_COLLISION_RADIUS_M};
use regia_world_geom::compat::terrain_patch::TerrainPatch;

const SIDE: usize = 256;
const WIDTH: f32 = 1280.0;
const BASE: u16 = 16384;

/// The park distance of a go-to order (the source's `GOTO_PARK_M`): a hull this close to its goal has
/// arrived.
const GOTO_PARK_M: f32 = 8.0;

fn settings() -> Arc<GridSettings> {
    Arc::new(GridSettings::hover_ground_jet())
}

/// Heightfield from a function of world (x, z) in metres. 5 m cells.
fn terrain(height: impl Fn(f32, f32) -> f32) -> TerrainSampler {
    let mut heights = vec![0u16; SIDE * SIDE];
    let spacing = WIDTH / SIDE as f32;
    let half = WIDTH * 0.5;
    for z in 0..SIDE {
        for x in 0..SIDE {
            let wx = x as f32 * spacing - half;
            let wz = z as f32 * spacing - half;
            let h = height(wx, wz).clamp(-400.0, 400.0);
            heights[z * SIDE + x] = (BASE as f32 + h * 32.0).round() as u16;
        }
    }
    // Absolute heights: 0 m is BASE counts (a wall through the centre must not set the datum).
    TerrainSampler::new(HeightGrid::square(SIDE, heights).expect("square grid"), WIDTH)
        .with_zero_counts(u32::from(BASE))
}

fn d2(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[2] - b[2];
    math::sqrt(dx * dx + dz * dz)
}

fn dist2(a: [f32; 2], b: [f32; 2]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[1] - b[1];
    math::sqrt(dx * dx + dz * dz)
}

// ---------------------------------------------------------------- the fixture (ir-sim/src/tunnels.rs 470-524)

/// `terrain` with each listed ramp's open cut (the segments with no rock over them)
/// carved to the ramp floor, on 0.5 m cells, the way a baked mouth patch is.
fn carve_open_cuts<N>(terrain: TerrainSampler, tunnels: &TunnelsState<N>, ramps: &[&str]) -> TerrainSampler {
    let map = tunnels.layout.as_ref().expect("a layout");
    let mut patches = Vec::new();
    for id in ramps {
        let passage = map.sampled.passage(id).unwrap_or_else(|| panic!("{id} passage"));
        let half = passage.width_m * 0.5;
        let cuts: Vec<usize> = (0..passage.samples.len().saturating_sub(1))
            .filter(|i| !passage.covered.get(*i).copied().unwrap_or(true))
            .collect();
        if cuts.is_empty() {
            continue;
        }
        let pts: Vec<[f32; 3]> = cuts
            .iter()
            .flat_map(|i| [passage.samples[*i].p, passage.samples[*i + 1].p])
            .collect();
        let (mut x0, mut x1, mut z0, mut z1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
        for p in &pts {
            x0 = x0.min(p[0]);
            x1 = x1.max(p[0]);
            z0 = z0.min(p[2]);
            z1 = z1.max(p[2]);
        }
        let spacing = 0.5f32;
        let origin = [x0 - half - 1.0, z0 - half - 1.0];
        let cols = ((x1 + half + 1.0 - origin[0]) / spacing).ceil() as usize + 1;
        let rows = ((z1 + half + 1.0 - origin[1]) / spacing).ceil() as usize + 1;
        let mut heights = Vec::with_capacity(cols * rows);
        for r in 0..rows {
            for c in 0..cols {
                let (x, z) = (origin[0] + c as f32 * spacing, origin[1] + r as f32 * spacing);
                // The nearest ramp segment decides: only a cell nearest an open-cut segment is carved,
                // so the covered ramp keeps its rock (and its lid) over it.
                let mut best: Option<(f32, f32, bool)> = None;
                for i in 0..passage.samples.len() - 1 {
                    let (a, b) = (passage.samples[i].p, passage.samples[i + 1].p);
                    let (dx, dz) = (b[0] - a[0], b[2] - a[2]);
                    let len2 = (dx * dx + dz * dz).max(1e-6);
                    let t = (((x - a[0]) * dx + (z - a[2]) * dz) / len2).clamp(0.0, 1.0);
                    let (px, pz) = (a[0] + dx * t, a[2] + dz * t);
                    let d = ((x - px).powi(2) + (z - pz).powi(2)).sqrt();
                    if d <= half && best.is_none_or(|(bd, _, _)| d < bd) {
                        best = Some((
                            d,
                            a[1] + (b[1] - a[1]) * t,
                            !passage.covered.get(i).copied().unwrap_or(true),
                        ));
                    }
                }
                let base = terrain.height_at_world(x, z);
                heights.push(match best {
                    Some((_, y, true)) => y.min(base),
                    _ => base,
                });
            }
        }
        patches.push(TerrainPatch {
            id: format!("{id}_cut"),
            origin,
            spacing,
            cols,
            rows,
            heights,
        });
    }
    terrain.with_patches(patches)
}

// ---------------------------------------------------------------- the tunnel world (pathing/tests.rs 592-669)

/// What the source's stripped game state holds for the tunnel proofs: the terrain the sim stands on,
/// its armed tunnels (with the navmesh), and the grid and tunnel layer `nav_prepare` builds from them.
struct TunnelWorld {
    terrain: TerrainSampler,
    tunnels: NavTunnelsState,
    grid: NavGrid,
    layer: TunnelLayer,
}

fn tunnel_layout() -> TunnelLayout {
    let floor = -40.0;
    TunnelLayout {
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
    }
}

/// An 80 m wall along x = 0 from z = -400 to 400 on flat ground, and a tunnel under it:
/// a tube from x = -120 to 120 at y = -40, ramps up to the surface at x = -190 and 190,
/// and a gallery that leaves the tube at x = -60, loops north and rejoins at x = 60.
fn tunnel_world() -> TunnelWorld {
    let sampler = terrain(|x, z| {
        if x.abs() <= 10.0 && z.abs() <= 400.0 {
            80.0
        } else {
            0.0
        }
    });
    let layout = tunnel_layout();
    // T3 takes the highest surface at or under the body, so the open cut of each ramp is
    // carved into the ground the way a baked mouth patch is.
    let probe = TunnelsState::<()>::from_layout(layout.clone(), Some(&sampler));
    let sampler = carve_open_cuts(sampler, &probe, &["ramp_w", "ramp_e"]);
    let tunnels =
        NavTunnelsState::from_layout_with_nav(layout, Some(&sampler), &TunnelNavMeshBake::default());
    // `nav_prepare`: the grid from the terrain, the tunnel layer from the armed layout, its open cuts
    // closed on the surface.
    let stat = Arc::new(NavStatic::build(Some(&sampler), settings()).expect("ground"));
    let map = tunnels.layout.as_ref().expect("armed layout");
    let (layer, cut) = TunnelLayer::build(map, &stat);
    let grid = NavGrid::with_closed_cells(stat, cut).expect("cut mask");
    TunnelWorld {
        terrain: sampler,
        tunnels,
        grid,
        layer,
    }
}

/// The host's ground pass: floors (galleries included), walls, ceiling, for bodies of `radius`.
fn ground_pass(world: &TunnelWorld, bodies: &mut [[f32; 3]], radius: f32) {
    let terrain = Some(&world.terrain);
    for p in bodies.iter_mut() {
        let floor = world.tunnels.move_floor(terrain, p[0], p[2], p[1]);
        let mut q = [p[0], floor.point[1], p[2]];
        if floor.surface == FloorSurface::TunnelFloor {
            let mesh = world.tunnels.mesh.as_ref().unwrap();
            let [x, z] = separate_disc_from_walls_band(mesh, q[0], q[2], radius, q[1] - 1.0, q[1] + 2.0);
            q[0] = x;
            q[2] = z;
            q[1] = world.tunnels.move_floor(terrain, x, z, floor.point[1]).point[1];
        }
        *p = q;
    }
}

/// The source's mesh request for one hull (pathing/mod.rs 1150-1166, requirements): the hover agent
/// at the hull's radius, the tunnel share, the start and goal dropped through `MESH_LOCATE_DROPS`.
fn hull_mesh_route(mesh: &NavMesh, from: [f32; 3], goal: [f32; 3], pct: u32) -> Option<MeshPlan> {
    let query = MeshAgent::hover(UNIT_COLLISION_RADIUS_M);
    MESH_LOCATE_DROPS.iter().find_map(|dy| {
        mesh.find_path_scaled(
            [from[0], from[1] + dy, from[2]],
            [goal[0], goal[1] + dy, goal[2]],
            &query,
            pct,
        )
    })
}

/// A hull driven along a mesh route: every metre of every leg, grounded by [`ground_pass`] from the
/// waypoint height. Returns the deepest floor and where it ends.
fn drive_route(world: &TunnelWorld, plan: &MeshPlan) -> (f32, [f32; 3]) {
    let mut deepest = 0.0f32;
    let mut at = plan.waypoints[0];
    for w in plan.waypoints.windows(2) {
        let (a, b) = (w[0], w[1]);
        let steps = (d2(a, b).ceil() as usize).max(1);
        for k in 0..=steps {
            let t = k as f32 / steps as f32;
            let mut body = [[
                a[0] + (b[0] - a[0]) * t,
                a[1] + (b[1] - a[1]) * t + 1.0,
                a[2] + (b[2] - a[2]) * t,
            ]];
            ground_pass(world, &mut body, UNIT_COLLISION_RADIUS_M);
            deepest = deepest.min(body[0][1]);
            at = body[0];
        }
    }
    (deepest, at)
}

fn tunnel_polys(mesh: &NavMesh, plan: &MeshPlan) -> usize {
    plan.polys
        .iter()
        .filter(|p| mesh.poly_kind(**p) == Some(PolyKind::Tunnel))
        .count()
}

// ---------------------------------------------------------------- the proofs

#[test]
fn the_planner_takes_the_tunnel_when_it_is_shorter() {
    let world = tunnel_world();
    let grid = &world.grid;
    assert!(!world.layer.is_empty(), "the tunnel layer is built");
    let mut scratch = Scratch::default();
    let hover = layer_for(grid, Some(&world.layer), Profile::HOVER, PRESET_TUNNEL_SIZE_LIMIT);
    let plan = search::plan_layered(
        &grid.clone(),
        hover,
        &mut scratch,
        Profile::HOVER,
        [-250.0, 0.0],
        None,
        [250.0, 0.0],
        None,
        layer::TUNNEL_COST_PCT,
    )
    .unwrap();
    assert_eq!(plan.kind, PlanKind::Full);
    assert!(plan.layers.iter().any(|t| *t), "the route goes underground");
    let len: f32 = plan.waypoints.windows(2).map(|w| dist2(w[0], w[1])).sum();
    assert!(len < 560.0, "through the tunnel, not 900 m round the wall: {len}");
    // A large hull does not fit: it goes round.
    let large = layer_for(
        grid,
        Some(&world.layer),
        Profile::HOVER_LARGE,
        PRESET_TUNNEL_SIZE_LIMIT,
    );
    let big = search::plan_layered(
        &grid.clone(),
        large,
        &mut scratch,
        Profile::HOVER_LARGE,
        [-250.0, 0.0],
        None,
        [250.0, 0.0],
        None,
        layer::TUNNEL_COST_PCT,
    )
    .unwrap();
    assert!(big.layers.iter().all(|t| !*t));
}

/// T6: a hover hull routes from the west mouth to the east mouth on the navmesh corridors
/// (`PolyKind::Tunnel`), not the centreline graph. The route goes down the west ramp, along the tube
/// and up the east ramp. Planner level (the source steps a hull order for 40 s): the hull's mesh
/// request is planned, then driven leg by leg through the ground pass.
#[test]
fn a_hull_routes_mouth_to_mouth_through_the_tube_on_the_navmesh() {
    let world = tunnel_world();
    let navmesh = world
        .tunnels
        .navmesh
        .as_deref()
        .expect("a tunnel map arms its navmesh");
    assert!(navmesh.has_tunnels(), "the bake has tunnel corridors");
    assert!(navmesh.poly_count() > 0);
    // The router puts a hover hull on a tunnel-carrying mesh.
    assert_eq!(route_for(true, true, navmesh.has_tunnels()), RouteKind::Mesh);
    let plan = hull_mesh_route(
        navmesh,
        [-190.0, 0.0, 0.0],
        [190.0, 0.0, 0.0],
        layer::TUNNEL_COST_PCT,
    );
    let mesh_routes = u32::from(plan.is_some());
    let plan = plan.expect("the route was planned on the navmesh");
    let (deepest, at) = drive_route(&world, &plan);
    assert!(
        d2(at, [190.0, 0.0, 0.0]) <= GOTO_PARK_M + 0.5,
        "arrived at the east mouth"
    );
    assert!(deepest < -35.0, "descended into the tube ({deepest})");
    assert!(mesh_routes > 0, "the route was planned on the navmesh");
    assert!(
        tunnel_polys(navmesh, &plan) > 0,
        "the navmesh route used the tunnel corridors"
    );
}

/// T6: a flank hull (tunnel metres cost 20%) routes through the tube even when the surface is
/// closed by the wall. The route is found on the navmesh. Planner level, as above.
#[test]
fn a_flank_hull_routes_through_the_tube_on_the_navmesh_when_the_surface_is_blocked() {
    let world = tunnel_world();
    let navmesh = world.tunnels.navmesh.as_deref().expect("navmesh");
    let plan = hull_mesh_route(
        navmesh,
        [-250.0, 0.0, 0.0],
        [250.0, 0.0, 0.0],
        layer::FLANK_COST_PCT,
    );
    let mesh_routes = u32::from(plan.is_some());
    let plan = plan.expect("the flank route was planned on the navmesh");
    let (deepest, at) = drive_route(&world, &plan);
    assert!(d2(at, [250.0, 0.0, 0.0]) <= GOTO_PARK_M + 0.5, "arrived");
    assert!(deepest < -35.0, "the flank went underground ({deepest})");
    assert!(mesh_routes > 0, "the flank route was planned on the navmesh");
    assert!(tunnel_polys(navmesh, &plan) > 0);
}

#[test]
fn the_ceiling_caps_height_and_rock_hides_radar_past_150_m() {
    let world = tunnel_world();
    let t = &world.tunnels;
    let mesh = t.mesh.as_ref().unwrap();
    // Under the tube's lid (floor -40, ceiling -28): a hull is held 2.5 m under it.
    let y = clamp_altitude_under_ceiling(Some(mesh), -100.0, -20.0, 0.0, UNIT_COLLISION_RADIUS_M);
    assert!(
        (y - (-28.0 - UNIT_COLLISION_RADIUS_M)).abs() < 0.01,
        "clamped to {y}"
    );
    let under = [-100.0, -39.0, 0.0];
    assert!(t.underground(under));
    assert!(
        !t.underground([-100.0, 1.0, 0.0]),
        "the surface above is not underground"
    );
    assert!(
        !t.rock_hidden(under, [-100.0, 1.0, 120.0]),
        "inside 150 m it shows"
    );
    assert!(
        t.rock_hidden(under, [-100.0, 1.0, 200.0]),
        "past 150 m rock hides it"
    );
    assert!(t.underground([100.0, -39.0, 0.0]));
    assert!(
        !t.rock_hidden(under, [100.0, -39.0, 0.0]),
        "both underground, 200 m: normal radar"
    );
    // The open top of a ramp is not underground.
    assert!(!t.underground([-185.0, -1.0, 0.0]));
}

/// A tunnel-mouth funnel baked as a ground patch: 0.5 m cells, a 40 m slope that drops 24 m.
fn funnel_patch(drop_over_run: f32) -> TerrainPatch {
    let (cols, rows) = (121usize, 41usize);
    let mut heights = Vec::with_capacity(cols * rows);
    for _r in 0..rows {
        for c in 0..cols {
            heights.push(-(c as f32 * 0.5 * drop_over_run).min(24.0));
        }
    }
    TerrainPatch {
        id: "funnel".into(),
        origin: [0.0, 0.0],
        spacing: 0.5,
        cols,
        rows,
        heights,
    }
}

#[test]
fn a_walkable_mouth_funnel_is_walkable_in_the_nav_grid_and_a_cliff_one_is_not() {
    let flat = terrain(|_, _| 0.0);
    let plain = NavStatic::build(Some(&flat), settings()).unwrap();
    let key_plain = plain.key;

    let gentle = terrain(|_, _| 0.0).with_patches(vec![funnel_patch(0.6)]);
    let stat = NavStatic::build(Some(&gentle), settings()).unwrap();
    assert_ne!(stat.key, key_plain, "a patched map rebuilds the grid");
    let nav = NavGrid::new(Arc::new(stat)).unwrap();
    let (cx, cz) = nav.cell_coords(30.0, 10.0);
    let cell = nav.cell_at(30.0, 10.0);
    assert!(
        nav.stat.height_cells[cell as usize] < -10.0,
        "the nav height follows the drawn funnel"
    );
    assert!(
        nav.stat.steep[cell as usize] < 0.75,
        "0.6 slope reads as a slope, not a cliff"
    );
    for profile in [Profile::FOOT, Profile::HOVER] {
        assert!(
            nav.passable(profile, cell),
            "{profile:?} stands on the funnel slope"
        );
        assert!(
            nav.can_step(profile, cell, 1, 0),
            "{profile:?} walks down the funnel"
        );
        assert!(nav.can_step(profile, cell, -1, 0), "{profile:?} walks back up");
    }
    let _ = (cx, cz);

    let sheer = terrain(|_, _| 0.0).with_patches(vec![funnel_patch(4.0)]);
    let nav = NavGrid::new(Arc::new(NavStatic::build(Some(&sheer), settings()).unwrap())).unwrap();
    let cell = nav.cell_at(2.0, 10.0);
    assert!(
        !nav.passable(Profile::FOOT, cell),
        "a 4:1 funnel wall is still a cliff on foot"
    );
}

#[test]
fn funnel_cells_are_flagged_so_the_ramp_cut_leaves_the_slope_open() {
    let plain = NavStatic::build(Some(&terrain(|_, _| 0.0)), settings()).unwrap();
    assert!(plain.lowered.iter().all(|f| *f == 0), "no patch, no funnel cells");
    let stat = NavStatic::build(
        Some(&terrain(|_, _| 0.0).with_patches(vec![funnel_patch(0.6)])),
        settings(),
    )
    .unwrap();
    let nav = NavGrid::new(Arc::new(stat)).unwrap();
    let deep = nav.cell_at(40.0, 10.0) as usize;
    let rim = nav.cell_at(-20.0, 10.0) as usize;
    assert_eq!(
        nav.stat.lowered[deep], 1,
        "a cell the patch lowers is a funnel cell"
    );
    assert_eq!(nav.stat.lowered[rim], 0, "ground outside the patch is not");
}

/// T7 collapse: rubble closes its tunnel nodes and the planner routes round
/// the wall on the surface; clearing the rubble reopens the passage. The
/// closed set is the nav end of the host's collapse field.
#[test]
fn a_collapse_closes_its_passage_and_clearing_reopens_it() {
    let world = tunnel_world();
    let route = |world: &TunnelWorld| {
        let grid = &world.grid;
        let mut scratch = Scratch::default();
        search::plan_layered(
            &grid.clone(),
            layer_for(grid, Some(&world.layer), Profile::HOVER, PRESET_TUNNEL_SIZE_LIMIT),
            &mut scratch,
            Profile::HOVER,
            [-250.0, 0.0],
            None,
            [250.0, 0.0],
            None,
            layer::TUNNEL_COST_PCT,
        )
        .unwrap()
    };
    assert!(route(&world).layers.iter().any(|t| *t), "through the tube first");
    let tunnel = &world.layer;
    // Rubble drops the tube: every tube node is under rubble.
    let tube: Vec<u32> = tunnel
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| !n.ramp)
        .map(|(i, _)| i as u32)
        .collect();
    assert!(!tube.is_empty());
    tunnel.set_closed(tube.clone());
    assert_eq!(tunnel.closed(), tube, "whole-list replace, sorted");
    let round = route(&world);
    assert!(
        round.layers.iter().all(|t| !*t),
        "no way underground: round the wall, kind {:?}",
        round.kind
    );
    // Blown clear: the passage is back.
    tunnel.set_closed(Vec::new());
    assert!(tunnel.closed().is_empty());
    assert!(route(&world).layers.iter().any(|t| *t), "through the tube again");
}

// ---------------------------------------------------------------- the bake seam (plan row W-04)

/// The bake is the seam's: `TunnelsState<NavMesh>` holds exactly the mesh `NavMesh::bake` makes from
/// `TunnelNavMeshBake::parts`, the collision mesh, nav overlay and layout do not depend on it, and no
/// terrain bakes no navmesh.
#[test]
fn arming_the_navmesh_bakes_the_parts_and_leaves_the_collision_mesh_alone() {
    let world = tunnel_world();
    let bake = TunnelNavMeshBake::default();
    let map = world.tunnels.layout.as_ref().unwrap();
    let parts = bake
        .parts(&map.sampled, Some(&world.terrain))
        .expect("a terrain gives parts");
    assert_eq!(parts.corridors.len(), 4, "one corridor per passage");
    assert_eq!(parts.extent_m, WIDTH);
    let direct = NavMesh::bake(&parts).expect("the parts bake");
    let armed = world.tunnels.navmesh.as_deref().unwrap();
    assert_eq!(armed.to_bytes(), direct.to_bytes());
    assert_eq!(armed.bake_id(), direct.bake_id());
    let plain = TunnelsState::<()>::from_layout(tunnel_layout(), Some(&world.terrain));
    assert_eq!(plain.mesh, world.tunnels.mesh);
    assert_eq!(plain.nav, world.tunnels.nav);
    assert_eq!(plain.layout, world.tunnels.layout);
    assert!(plain.navmesh.is_none());
    assert!(NoNavBake.bake(&map.sampled, Some(&world.terrain)).is_none());
    assert!(bake.bake(&map.sampled, None).is_none(), "no terrain, no navmesh");
    let unarmed = NavTunnelsState::from_layout_with_nav(tunnel_layout(), None, &bake);
    assert!(unarmed.navmesh.is_none() && unarmed.mesh.is_some());
}

/// The ground raster is the terrain's base heights (never the carved patches) with the hover cliff:
/// the wall's faces are not walkable, the plain and the carved ramp cut are, and a stricter cliff
/// setting walks less.
#[test]
fn the_ground_raster_walks_the_plain_and_refuses_the_wall() {
    let world = tunnel_world();
    let t = &world.terrain;
    let raster = navmesh_ground(t, TunnelNavMeshBake::default().hover_cliff);
    assert_eq!((raster.cols, raster.rows), (SIDE, SIDE));
    assert_eq!(raster.step_m, WIDTH / SIDE as f32);
    assert_eq!((raster.origin_x, raster.origin_z), (-WIDTH * 0.5, -WIDTH * 0.5));
    let cell = |x: f32, z: f32| {
        let c = ((x - raster.origin_x) / raster.step_m) as usize;
        let r = ((z - raster.origin_z) / raster.step_m) as usize;
        r * raster.cols + c
    };
    assert!(raster.walk[cell(-300.0, 100.0)], "the plain is walkable");
    assert!(!raster.walk[cell(-10.0, 100.0)], "the wall's face is a cliff");
    let ramp = cell(-175.0, 0.0);
    assert!(
        raster.walk[ramp] && raster.height[ramp] == 0.0,
        "the base height, not the carved cut"
    );
    let walk = |r: &regia_nav::det::navmesh::GroundRaster| r.walk.iter().filter(|w| **w).count();
    // The wall's top is flat (walkable); only its faces are cliffs.
    let face_cells = SIDE * SIDE - walk(&raster);
    assert!(face_cells > 0);
    assert!(walk(&navmesh_ground(t, 0.0)) <= walk(&raster));
}

// ---------------------------------------------------------------- acceptance 1: the Switchbacks one-bake pin

/// The canyon floor, flat at -256 m, as the sim heightfield sees it.
fn canyon() -> TerrainSampler {
    TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).expect("grid"), 1280.0).with_zero_counts(16384)
}

/// T1 "one bake": the baked file the runtime loads (`do27-tunnels/1`) rebuilds the sim's collision
/// mesh, nav overlay and tunnel navmesh byte-identically to the splines, so the replay checksums
/// cannot move when the runtime arms its tunnels from the baked file instead of re-sampling the
/// splines. Here with the navmesh half (`TunnelsState<NavMesh>`, bytes and bake id).
#[test]
fn switchbacks_baked_file_builds_the_byte_identical_sim() {
    let terrain = canyon();
    let bake = TunnelNavMeshBake::default();
    let text = switchbacks().export_json(Some(&terrain));
    let baked = parse_tunnels(&text).expect("the baked file parses");
    let from_spline = NavTunnelsState::for_layout_with_nav(switchbacks(), Some(&terrain), &bake);
    let from_baked = NavTunnelsState::from_baked_with_nav(&baked, &terrain, &bake);
    assert_eq!(
        from_baked.mesh, from_spline.mesh,
        "the baked file rebuilds the same collision mesh (byte-identical triangles)"
    );
    assert_eq!(
        from_baked.nav, from_spline.nav,
        "the baked file rebuilds the same nav overlay"
    );
    assert_eq!(
        from_baked.layout, from_spline.layout,
        "the baked file carries the same description and sampled centrelines"
    );
    assert_eq!(
        baked.triangle_count.map(|n| n as usize),
        Some(from_spline.mesh.as_ref().expect("mesh").triangles.len()),
        "triangle_count in the file agrees with the sim mesh"
    );
    // The navmesh half (plan row W-04).
    let spline_mesh = from_spline
        .navmesh
        .as_deref()
        .expect("the splines bake a navmesh");
    let baked_mesh = from_baked
        .navmesh
        .as_deref()
        .expect("the baked file bakes a navmesh");
    assert!(
        spline_mesh.has_tunnels(),
        "the Switchbacks navmesh carries its tunnel corridors"
    );
    assert_eq!(
        baked_mesh.to_bytes(),
        spline_mesh.to_bytes(),
        "the baked file rebuilds the same tunnel navmesh (byte-identical)"
    );
    assert_eq!(baked_mesh.bake_id(), spline_mesh.bake_id(), "the same bake id");
}

/// FNV-1a 64 of a byte string.
fn fnv(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// The Switchbacks tunnel navmesh's bake id, wire length and FNV-1a 64 of its wire bytes on the canyon
/// floor, recorded on x86-64 Linux (the build server, Rust 1.99). Every target must reproduce them.
const SWITCHBACKS_NAVMESH_BAKE_ID: u64 = 0xbae1_74da_539e_bf71;
/// See [`SWITCHBACKS_NAVMESH_BAKE_ID`].
const SWITCHBACKS_NAVMESH_LEN: usize = 65_181;
/// See [`SWITCHBACKS_NAVMESH_BAKE_ID`].
const SWITCHBACKS_NAVMESH_FNV: u64 = 0xb9ab_f6c8_fdd9_5dc5;

/// Acceptance 4 of plan row W-04: the Switchbacks tunnel navmesh is pinned (bake id and bytes), so a
/// change to the bake, the corridor sampling or the wire on any target fails here.
#[test]
fn switchbacks_navmesh_bake_id_is_pinned() {
    let tunnels =
        NavTunnelsState::for_layout_with_nav(switchbacks(), Some(&canyon()), &TunnelNavMeshBake::default());
    let mesh = tunnels.navmesh.as_deref().expect("navmesh");
    let bytes = mesh.to_bytes();
    let got = (mesh.bake_id(), bytes.len(), fnv(&bytes));
    println!(
        "switchbacks navmesh: bake_id {:#018x} len {} fnv {:#018x} ({})",
        got.0,
        got.1,
        got.2,
        mesh.describe()
    );
    assert_eq!(
        got,
        (
            SWITCHBACKS_NAVMESH_BAKE_ID,
            SWITCHBACKS_NAVMESH_LEN,
            SWITCHBACKS_NAVMESH_FNV
        ),
        "the Switchbacks tunnel navmesh moved"
    );
    let loaded = NavMesh::from_bytes(&bytes).expect("the cooked bytes load");
    assert_eq!(loaded.bake_id(), mesh.bake_id());
}

// ---------------------------------------------------------------- acceptance 2: the tunnel plans pin

/// FNV-1a 64 of a grid plan, floats by their canonical bits.
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

/// FNV-1a 64 of a mesh plan, floats by their canonical bits.
fn mesh_digest(plan: Option<&MeshPlan>) -> u64 {
    let mut h = StateHasher::new();
    let Some(p) = plan else {
        h.u32("none", 1);
        return h.finish();
    };
    h.u32("expansions", p.expansions);
    h.f32("length", p.length);
    h.u32("waypoints", p.waypoints.len() as u32);
    for (w, poly) in p.waypoints.iter().zip(&p.polys) {
        h.f32("x", w[0]);
        h.f32("y", w[1]);
        h.f32("z", w[2]);
        h.u32("poly", *poly);
    }
    h.finish()
}

/// 128 seeded plans on the tunnel world, one per tick: even ticks a `plan_layered` grid plan (hover,
/// large hover, foot and walker profiles, tunnel starts and goals, the tube closed for ticks 32-63),
/// odd ticks a navmesh `find_path_scaled` plan for a hull or a pilot (tunnel shares 75 and 20, and a
/// plain `find_path`). Endpoints are drawn near the mouths, on the plain and down in the tube.
struct TunnelPlans {
    world: TunnelWorld,
    scratch: Scratch,
    rng: SplitMix64,
    last: u64,
    acc: u64,
    grid_underground: u32,
    grid_full: u32,
    mesh_found: u32,
    mesh_tunnel: u32,
    mesh_missed: u32,
    pilots: u32,
}

impl TunnelPlans {
    fn new() -> Self {
        TunnelPlans {
            world: tunnel_world(),
            scratch: Scratch::default(),
            rng: SplitMix64::new(0),
            last: 0,
            acc: 0,
            grid_underground: 0,
            grid_full: 0,
            mesh_found: 0,
            mesh_tunnel: 0,
            mesh_missed: 0,
            pilots: 0,
        }
    }

    /// A point: west or east of the wall on the plain, by a mouth, or in the tube.
    fn point(&mut self) -> [f32; 3] {
        let side = if self.rng.next_u64().is_multiple_of(2) {
            -1.0
        } else {
            1.0
        };
        match self.rng.next_u64() % 4 {
            0 => [
                side * (190.0 + 10.0 * self.rng.signed()),
                0.0,
                6.0 * self.rng.signed(),
            ],
            1 => [100.0 * self.rng.signed(), -40.0, 6.0 * self.rng.signed()],
            _ => [
                side * (220.0 + 150.0 * self.rng.unit()),
                0.0,
                300.0 * self.rng.signed(),
            ],
        }
    }
}

impl DetScenario for TunnelPlans {
    fn name(&self) -> &str {
        "tunnel_plans"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(0x5eed_0a04, 128, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        self.rng = SplitMix64::new(run.seed);
        self.scratch = Scratch::default();
        self.last = 0;
        self.acc = 0;
        self.grid_underground = 0;
        self.grid_full = 0;
        self.mesh_found = 0;
        self.mesh_tunnel = 0;
        self.mesh_missed = 0;
        self.pilots = 0;
        self.world.layer.set_closed(Vec::new());
    }

    fn step(&mut self, tick: u32) {
        let from = self.point();
        let goal = self.point();
        if tick.is_multiple_of(2) {
            let profile = [Profile::HOVER, Profile::HOVER_LARGE, Profile::FOOT, Profile(3)]
                [(self.rng.next_u64() % 4) as usize];
            let tube: Vec<u32> = if (tick / 32) % 4 == 1 {
                self.world
                    .layer
                    .nodes
                    .iter()
                    .enumerate()
                    .filter(|(_, n)| !n.ramp)
                    .map(|(i, _)| i as u32)
                    .collect()
            } else {
                Vec::new()
            };
            self.world.layer.set_closed(tube);
            let pick = self.rng.next_u64();
            let m = self.world.layer.len() as u64;
            let from_tunnel = pick.is_multiple_of(5).then_some(((pick >> 8) % m) as u32);
            let goal_tunnel = (pick % 7 == 1).then_some(((pick >> 24) % m) as u32);
            let pct = [layer::TUNNEL_COST_PCT, layer::FLANK_COST_PCT][(pick >> 40) as usize % 2];
            let grid = &self.world.grid;
            let layer = layer_for(grid, Some(&self.world.layer), profile, PRESET_TUNNEL_SIZE_LIMIT);
            let plan = search::plan_layered(
                grid,
                layer,
                &mut self.scratch,
                profile,
                [from[0], from[2]],
                from_tunnel,
                [goal[0], goal[2]],
                goal_tunnel,
                pct,
            );
            if let Some(p) = &plan {
                self.grid_full += u32::from(p.kind == PlanKind::Full);
                self.grid_underground += u32::from(p.layers.iter().any(|t| *t));
            }
            self.last = plan_digest(plan.as_ref());
        } else {
            let mesh = self.world.tunnels.navmesh.as_deref().expect("navmesh");
            let pilot = self.rng.next_u64().is_multiple_of(2);
            let agent = if pilot {
                self.pilots += 1;
                MeshAgent::pilot()
            } else {
                MeshAgent::hover(UNIT_COLLISION_RADIUS_M)
            };
            let plan = match self.rng.next_u64() % 3 {
                0 => mesh.find_path(from, goal, &agent),
                1 => mesh.find_path_scaled(from, goal, &agent, layer::TUNNEL_COST_PCT),
                _ => mesh.find_path_scaled(from, goal, &agent, layer::FLANK_COST_PCT),
            };
            match &plan {
                Some(p) => {
                    self.mesh_found += 1;
                    self.mesh_tunnel += u32::from(tunnel_polys(mesh, p) > 0);
                }
                None => self.mesh_missed += 1,
            }
            self.last = mesh_digest(plan.as_ref());
        }
        self.acc = (self.acc ^ self.last).wrapping_mul(0x100_0000_01b3);
    }

    fn hash_state(&self, h: &mut StateHasher) {
        h.u64("plan", self.last);
        h.u64("acc", self.acc);
        h.u32("grid_full", self.grid_full);
        h.u32("grid_underground", self.grid_underground);
        h.u32("mesh_found", self.mesh_found);
        h.u32("mesh_tunnel", self.mesh_tunnel);
        h.u32("mesh_missed", self.mesh_missed);
    }
}

/// Acceptance 2 of plan row W-04 (the Regia twin of the source's `tunnel_runs_give_the_same_hash`,
/// which steps the whole game): 64 grid and 64 navmesh plans on the tunnel world give the same hash
/// per tick on two fresh runs and after a reset, and the run covers underground grid plans, mesh plans
/// through the tunnel corridors for hulls and pilots, and mesh misses.
#[test]
fn tunnel_plans_give_the_same_hash() {
    let mut a = TunnelPlans::new();
    let run = a.default_run();
    let first = golden::record(&mut a, &run);
    let second = golden::record(&mut TunnelPlans::new(), &run);
    assert_eq!(first.tick_hashes.len(), 128);
    assert_eq!(
        golden::first_divergence(&first.tick_hashes, &second.tick_hashes),
        None
    );
    assert_eq!(first.run_hash(), second.run_hash());
    let again = golden::record(&mut a, &run);
    assert_eq!(again.tick_hashes, first.tick_hashes);
    println!(
        "grid full {} underground {}; mesh found {} tunnel {} missed {}; pilots {}",
        a.grid_full, a.grid_underground, a.mesh_found, a.mesh_tunnel, a.mesh_missed, a.pilots
    );
    assert!(a.grid_full > 32, "most grid plans are full");
    assert!(a.grid_underground > 0, "some grid plans go underground");
    assert!(a.mesh_tunnel > 0, "some mesh plans use the tunnel corridors");
    assert!(
        a.mesh_found > a.mesh_tunnel,
        "some mesh plans stay on the surface"
    );
    assert!(a.pilots > 0 && a.pilots < 64, "hulls and pilots both plan");
}

/// Acceptance 2 of plan row W-04: the tunnel plans pinned by a determinism golden
/// (`goldens/tunnel_plans.detgold`, ADR-8046 format). Every target must hash to the committed x86-64
/// Linux run; `REGIA_BLESS=1` records this target's line. Prints one `regia-det-golden {json}` line.
#[test]
fn tunnel_plans_pinned_on_every_arch() {
    let dir = match std::env::var_os(golden::GOLDEN_DIR_ENV) {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("goldens"),
    };
    let mut scenario = TunnelPlans::new();
    let run = scenario.default_run();
    let trace = golden::record(&mut scenario, &run);
    let report = golden::check_trace(
        &trace,
        &golden::golden_path(&dir, "tunnel_plans"),
        &golden::current_target(),
        GoldenMode::from_env(),
    );
    println!("{REPORT_PREFIX}{}", report.to_json());
    assert!(report.passed(), "{report}");
}

// New test, not from the source (KI-WD-030, ADR-10070).
/// The tunnel layer's mouths and junctions register as crowd chokes from the layer itself: each
/// portal's ramp top and each junction node becomes one choke, a second call adds nothing, and a
/// group whose leg passes a mouth (and no other choke) files into lanes, which it does not do
/// without the registration.
#[test]
fn tunnel_mouths_and_junctions_register_as_crowd_chokes() {
    use regia_nav::det::agent::DetAgents;

    let world = tunnel_world();
    let layer = &world.layer;
    assert_eq!(layer.portals.len(), 2, "one portal per opening");
    assert!(!layer.junctions.is_empty(), "the gallery and ramps join the tube");
    // The march: north across the west mouth's ramp top (-190, 0), more than CHOKE_RADIUS_M from
    // every junction (they sit at |x| <= 120).
    let members = [
        (1u32, [-190.0f32, -150.0f32]),
        (2, [-186.0, -150.0]),
        (3, [-194.0, -150.0]),
    ];
    let anchor = [-190.0, 150.0];

    let mut plain = DetAgents::default();
    plain.crowd_refresh(7, &members, anchor, 0);
    assert!(plain.crowd.contains(7), "the group holds a flow");
    assert!(
        !plain.crowd.lane_mode(7),
        "without registered chokes the mouth is not a choke"
    );

    let mut agents = DetAgents::default();
    let added = agents.register_tunnel_chokes(layer);
    assert_eq!(agents.crowd.choke_count(), added);
    assert!(
        added >= 3,
        "two mouths and at least one junction register: {added}"
    );
    assert_eq!(
        agents.register_tunnel_chokes(layer),
        0,
        "a second call adds nothing"
    );
    let mouth_tops: Vec<[f32; 2]> = layer
        .portals
        .iter()
        .map(|(node, _)| {
            let n = layer.nodes[*node as usize];
            [n.x, n.z]
        })
        .collect();
    for top in &mouth_tops {
        assert!(
            dist2(*top, [-190.0, 0.0]) < 2.0 || dist2(*top, [190.0, 0.0]) < 2.0,
            "a mouth's choke sits at its ramp top: {top:?}"
        );
    }
    for at in mouth_tops.iter().copied().chain(layer.junctions.iter().map(|j| {
        let n = layer.nodes[*j as usize];
        [n.x, n.z]
    })) {
        assert!(
            agents.crowd.choke_near(None, at, at),
            "every mouth and junction is a choke: {at:?}"
        );
    }
    agents.crowd_refresh(7, &members, anchor, 0);
    assert!(
        agents.crowd.lane_mode(7),
        "a group passing a registered mouth files into lanes"
    );
    assert_eq!(agents.report().crowd.chokes, added, "nav.det_agents reports them");
}
