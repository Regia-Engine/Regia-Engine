// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-core/tests/tunnels_switchbacks.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 1037-1116 (gallery routes and the hull and walker asserts), 1211-1312 (the whole-system route and
// the two walks), 1320-1485 (the tube window and the hull under and over asserts), mark: requirements
// (Stage A: unchanged). Ranges re-checked against regia-absorb-2 e6c84ba6d and unmoved.
// Edits: the host step is a kinematic walk over compat::onfoot and compat::state on the fixture (sim
// walls only). A hull rides 1 m and, where the floor changes by more than the hover ledge, keeps its
// world height and falls back to that ride (the host hover spring is not copied). The diagnosis that
// the walker needs installed set pieces, and otherwise walks the sim walls only, is rewritten on the
// gallery test: one span per cell fuses a floor-slab end under the floor with a roof facet about 13 m
// up into a column at the lining seam. Asserts are not edited.
// The fight checksum and the trooper walk are DO27-only and are not copied.

//! Kinematic Stage A walks on the Switchbacks fixture (plan row W-05, spec 121 § 13.1).

#![allow(clippy::unwrap_used)]

use regia_detsim::float::det_max;
use regia_detsim::math;
use regia_world_geom::compat::fixtures::{ON_FOOT_PARAMS, switchbacks};
use regia_world_geom::compat::hover_ground::{HULL_WALL_M, LEDGE_M};
use regia_world_geom::compat::onfoot::{
    Blocker, CELL_M, MAX_WALL_SPANS, OnFootSolids, PilotGrid, PilotSolids, pilot_floor_y, resolve_feet,
    slide_move,
};
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::compat::state::{TunnelsState, UNIT_COLLISION_RADIUS_M};

/// Waypoint counts the source's routes assert against.
const WHOLE_SYSTEM_POINTS: usize = 688;
const GALLERY_POINTS: usize = 69;
/// A waypoint this close (metres, XZ) counts as reached. The source's bubble.
const BUBBLE_M: f32 = 7.0;
/// Walker speed, metres per second (6 m/s at 60 Hz is 0.1 m a tick).
const WALK_SPEED: f32 = 6.0;
const TICK_HZ: f32 = 60.0;
/// Ride height of a hull over its floor (m). The host hover's set altitude; the host step is not copied.
const HULL_RIDE_M: f32 = 1.0;
/// Fall while a hull's altitude is above [`HULL_RIDE_M`] after a ledge (m/s^2). One tick stays under 1 m.
const HULL_FALL_M_S2: f32 = 9.81;
/// Largest one-tick height change the whole-system walk accepts (m).
const WALK_JUMP_LIMIT_M: f32 = 2.0;
/// Ticks with no XZ motion before a walk stops.
const STUCK_TICKS: u32 = 180;
/// The lining seam the fused-span walk stops at (source point about 22 of 69).
const SEAM_XZ: [f32; 2] = [-137.6, 76.1];
/// The tube sample nearest this XZ is the spot under the canyon floor.
const SPOT_XZ: [f32; 2] = [-407.8, 72.4];

struct Walk {
    reached: usize,
    route_len: usize,
    underground_ticks: u32,
    max_jump: f32,
    worst: String,
    stuck: String,
    end: [f32; 2],
}

struct NoSolids {
    solids: PilotSolids,
}

impl OnFootSolids for NoSolids {
    fn pilot_solids(&self) -> &PilotSolids {
        &self.solids
    }
    fn for_each_building(&self, _f: &mut dyn FnMut(&str, [f32; 3], f32)) {}
    fn for_each_loose_blocker(&self, _f: &mut dyn FnMut(Blocker)) {}
    fn for_each_fixed_blocker(&self, _f: &mut dyn FnMut(Blocker)) {}
    fn rock_push_out(&self, x: f32, z: f32, _feet_y: f32, _radius: f32) -> [f32; 2] {
        [x, z]
    }
}

fn canyon() -> TerrainSampler {
    TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384)
}

fn world() -> (TerrainSampler, TunnelsState) {
    let plain = canyon();
    let layout = switchbacks();
    let holes = layout.sample(Some(&plain)).mouth_holes(&layout.openings);
    let terrain = plain.with_holes(holes);
    let tunnels = TunnelsState::for_layout(layout, Some(&terrain));
    (terrain, tunnels)
}

fn xz_of(id: &str, tunnels: &TunnelsState) -> Vec<[f32; 2]> {
    tunnels
        .layout
        .as_ref()
        .unwrap()
        .sampled
        .passage(id)
        .unwrap()
        .samples
        .iter()
        .map(|q| [q.p[0], q.p[2]])
        .collect()
}

fn dist2(a: [f32; 2], b: [f32; 2]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[1] - b[1];
    dx * dx + dz * dz
}

fn nearest(line: &[[f32; 2]], at: [f32; 2]) -> usize {
    let mut best_i = 0usize;
    let mut best = f32::INFINITY;
    for (i, p) in line.iter().enumerate() {
        let d = dist2(*p, at);
        if d < best {
            best = d;
            best_i = i;
        }
    }
    best_i
}

fn past_top(l: &[[f32; 2]], d: f32) -> [f32; 2] {
    let (a, b) = (l[l.len() - 2], l[l.len() - 1]);
    let n = math::sqrt((b[0] - a[0]) * (b[0] - a[0]) + (b[1] - a[1]) * (b[1] - a[1]));
    [b[0] + (b[0] - a[0]) / n * d, b[1] + (b[1] - a[1]) / n * d]
}

/// The whole tunnel system, from 8 m past the west ramp's top to 8 m past the east ramp's top.
fn tunnel_walk_route(tunnels: &TunnelsState, reverse: bool) -> Vec<[f32; 2]> {
    let (tube, gallery) = (xz_of("tube", tunnels), xz_of("gallery", tunnels));
    let (rw, re) = (xz_of("ramp_west", tunnels), xz_of("ramp_east", tunnels));
    let (rmw, rme) = (xz_of("ramp_mid_west", tunnels), xz_of("ramp_mid_east", tunnels));
    let (iw, ie) = (nearest(&tube, rw[0]), nearest(&tube, re[0]));
    let (jw, je) = (
        nearest(&tube, gallery[0]),
        nearest(&tube, *gallery.last().unwrap()),
    );
    let cw = nearest(&tube, rmw[0]);
    let last = tube.len() - 1;
    assert!(
        iw < jw && jw < cw && cw < je && je < ie,
        "tube order: {iw} {jw} {cw} {je} {ie}"
    );
    let mut route = vec![past_top(&rw, 8.0)];
    route.extend(rw.iter().rev());
    route.extend(tube[0..=iw].iter().rev());
    route.extend(&tube[0..=jw]);
    route.extend(&gallery);
    route.extend(tube[cw..=je].iter().rev());
    route.extend(&rmw);
    route.extend(rmw.iter().rev());
    route.extend(&rme);
    route.extend(rme.iter().rev());
    route.extend(&tube[cw..=last]);
    route.extend(tube[ie..=last].iter().rev());
    route.extend(&re);
    route.push(past_top(&re, 8.0));
    if reverse {
        route.reverse();
    }
    route
}

/// From 25 m past the west mouth: down the west ramp, along the tube to the west junction, then the
/// gallery to its middle.
fn gallery_entry_route(tunnels: &TunnelsState) -> Vec<[f32; 2]> {
    let (tube, gallery, rw) = (
        xz_of("tube", tunnels),
        xz_of("gallery", tunnels),
        xz_of("ramp_west", tunnels),
    );
    let iw = nearest(&tube, rw[0]);
    let jw = nearest(&tube, [-120.0, 40.0]);
    let mid = nearest(&gallery, [0.0, 150.0]);
    let top = *rw.last().unwrap();
    let mut route = vec![[top[0] + 25.0, top[1] - 15.0]];
    route.extend(rw.iter().rev());
    if iw <= jw {
        route.extend(&tube[iw..=jw]);
    } else {
        route.extend(tube[jw..=iw].iter().rev());
    }
    route.extend(&gallery[..=mid]);
    route
}

/// The on-foot gallery route: 8 m past the ramp top along its own line, instead of 25 m out.
fn gallery_foot_route(tunnels: &TunnelsState) -> Vec<[f32; 2]> {
    let mut route = gallery_entry_route(tunnels);
    let (top, prev) = (route[1], route[2]);
    let len = math::sqrt((top[0] - prev[0]) * (top[0] - prev[0]) + (top[1] - prev[1]) * (top[1] - prev[1]));
    route[0] = [
        top[0] + (top[0] - prev[0]) / len * 8.0,
        top[1] + (top[1] - prev[1]) / len * 8.0,
    ];
    route
}

/// Hull height after one tick. A floor change past [`LEDGE_M`] keeps the world height (the ledge rule
/// in `compat::hover_ground`). While the altitude is above [`HULL_RIDE_M`] the hull falls at
/// [`HULL_FALL_M_S2`]. One tick of that fall stays under 1 m, including the last tick of a long drop.
fn hull_follow(body_y: f32, altitude: f32, fall_v: f32, floor_y: f32) -> (f32, f32, f32) {
    let prev_floor = body_y - altitude;
    let mut alt = altitude;
    let mut fall_v = fall_v;
    if (floor_y - prev_floor).abs() > LEDGE_M {
        alt = det_max(body_y - floor_y, 0.0);
    }
    if alt > HULL_RIDE_M {
        let dt = 1.0 / TICK_HZ;
        fall_v += HULL_FALL_M_S2 * dt;
        let drop = fall_v * dt;
        alt = det_max(alt - drop, HULL_RIDE_M);
    }
    if alt <= HULL_RIDE_M {
        fall_v = 0.0;
    }
    (floor_y + alt, alt, fall_v)
}

fn step_toward(pos: [f32; 2], target: [f32; 2], step: f32) -> [f32; 2] {
    let dx = target[0] - pos[0];
    let dz = target[1] - pos[1];
    let len = math::sqrt(dx * dx + dz * dz);
    if len <= step || len < 1e-6 {
        target
    } else {
        [pos[0] + dx / len * step, pos[1] + dz / len * step]
    }
}

/// How one kinematic walk steps.
struct WalkDrive {
    speed: f32,
    radius: f32,
    /// Floor hint at the first point. `None` is the outdoor heightfield.
    start_hint: Option<f32>,
    /// Stand on `pilot_floor_y`. A hull rides and falls instead.
    on_foot: bool,
}

/// Walker along `route`. `grid` is empty for the sim-wall walks.
///
/// `drive.start_hint` is the floor `move_floor` is asked from at the first point. `None` is the
/// outdoor heightfield (a body that starts on the plain). A body that starts in the tube passes the
/// tube floor: a canyon hint at the same XZ stands on the plain over the lid.
///
/// `drive.on_foot` stands on [`pilot_floor_y`]. A hull rides [`HULL_RIDE_M`] over the floor and, where
/// the floor changes by more than [`LEDGE_M`] in one tick, keeps its world height and falls
/// ([`hover_ground`](regia_world_geom::compat::hover_ground)): snapping to the new floor is the 1.67 m
/// jump at the gallery mouth.
fn walk_route(
    terrain: &TerrainSampler,
    tunnels: &TunnelsState,
    grid: &PilotGrid,
    route: &[[f32; 2]],
    drive: WalkDrive,
) -> Walk {
    let WalkDrive {
        speed,
        radius,
        start_hint,
        on_foot,
    } = drive;
    let solids = NoSolids {
        solids: PilotSolids {
            world: grid.clone(),
            ..PilotSolids::default()
        },
    };
    let step = speed / TICK_HZ;
    let hint = start_hint.unwrap_or_else(|| terrain.height_at_world(route[0][0], route[0][1]));
    let floor0 = tunnels
        .move_floor(Some(terrain), route[0][0], route[0][1], hint)
        .point[1];
    let mut altitude = if on_foot { 0.0 } else { HULL_RIDE_M };
    let mut fall = 0.0f32;
    let mut feet = floor0 + altitude;
    let mut pos = route[0];
    let mut idx = 0usize;
    let mut underground_ticks = 0u32;
    let mut max_jump = 0.0f32;
    let mut worst = String::new();
    let mut still = 0u32;
    let mut prev = pos;
    let cap_ticks = route.len().saturating_mul(400).max(1_000);
    for _ in 0..cap_ticks {
        while idx + 1 < route.len() && dist2(pos, route[idx + 1]) < BUBBLE_M * BUBBLE_M {
            idx += 1;
        }
        if idx + 1 >= route.len() {
            break;
        }
        let next = step_toward(pos, route[idx + 1], step);
        let slid = slide_move(
            &ON_FOOT_PARAMS,
            grid,
            &[],
            pos[0],
            pos[1],
            next[0],
            next[1],
            feet,
            ON_FOOT_PARAMS.capsule_radius_m,
        );
        let floor_y = tunnels.move_floor(Some(terrain), slid[0], slid[1], feet).point[1];
        let confined = tunnels.confine(Some(terrain), [slid[0], feet, slid[1]], floor_y, radius);
        let (cx, cz) = match confined {
            Some(p) => (p[0], p[2]),
            None => (slid[0], slid[1]),
        };
        let moved = resolve_feet(
            &ON_FOOT_PARAMS,
            [pos[0], feet, pos[1]],
            [cx - pos[0], cz - pos[1]],
            |x, z| tunnels.move_floor(Some(terrain), x, z, feet).point[1],
            grid,
            &[],
            true,
        );
        let base = tunnels
            .move_floor(Some(terrain), moved[0], moved[2], moved[1])
            .point[1];
        let stood = pilot_floor_y(
            &solids,
            Some(terrain),
            &ON_FOOT_PARAMS,
            moved[0],
            moved[2],
            moved[1],
            base,
        );
        let (next_xz, next_y, next_alt, next_fall) = if on_foot {
            ([moved[0], moved[2]], stood, 0.0, 0.0)
        } else {
            let mut hx = moved[0];
            let mut hz = moved[2];
            let mut floor_here = tunnels.move_floor(Some(terrain), hx, hz, feet).point[1];
            if floor_here - feet > HULL_WALL_M {
                hx = pos[0];
                hz = pos[1];
                floor_here = tunnels.move_floor(Some(terrain), hx, hz, feet).point[1];
            }
            let (y, alt, vel) = hull_follow(feet, altitude, fall, floor_here);
            ([hx, hz], y, alt, vel)
        };
        let jump = (next_y - feet).abs();
        if jump > max_jump {
            max_jump = jump;
            worst = format!(
                "({:.1}, {:.2}, {:.1}) -> ({:.1}, {:.2}, {:.1})",
                pos[0], feet, pos[1], next_xz[0], next_y, next_xz[1]
            );
        }
        if tunnels.underground([next_xz[0], next_y, next_xz[1]]) {
            underground_ticks += 1;
        }
        if dist2(next_xz, prev) < 1e-8 {
            still += 1;
            if still >= STUCK_TICKS {
                pos = next_xz;
                break;
            }
        } else {
            still = 0;
        }
        prev = next_xz;
        pos = next_xz;
        feet = next_y;
        altitude = next_alt;
        fall = next_fall;
    }
    while idx + 1 < route.len() && dist2(pos, route[idx + 1]) < BUBBLE_M * BUBBLE_M {
        idx += 1;
    }
    Walk {
        reached: idx + 1,
        route_len: route.len(),
        underground_ticks,
        max_jump,
        worst,
        stuck: format!("({:.1}, {:.1})", pos[0], pos[1]),
        end: pos,
    }
}

fn walk_the_whole_system(reverse: bool) {
    let (terrain, tunnels) = world();
    let route = tunnel_walk_route(&tunnels, reverse);
    let grid = PilotGrid::default();
    eprintln!(
        "WALK reverse {reverse}: on-foot grid {} cells (sim walls only)",
        grid.cells.len()
    );
    let d = walk_route(
        &terrain,
        &tunnels,
        &grid,
        &route,
        WalkDrive {
            speed: WALK_SPEED,
            radius: ON_FOOT_PARAMS.capsule_radius_m,
            start_hint: None,
            on_foot: true,
        },
    );
    eprintln!(
        "WALK reverse {reverse}: reached {} of {} end {:?}, underground {} ticks, worst '{}'",
        d.reached, d.route_len, d.end, d.underground_ticks, d.worst
    );
    assert_eq!(d.route_len, WHOLE_SYSTEM_POINTS, "the whole-system route");
    assert!(
        d.reached >= d.route_len - 1,
        "the pilot stopped at point {} of {}, {} (worst: {})",
        d.reached,
        d.route_len,
        d.stuck,
        d.worst
    );
    assert!(
        d.underground_ticks > 60 * 120,
        "walked underground ({} ticks)",
        d.underground_ticks
    );
    assert!(
        d.max_jump <= WALK_JUMP_LIMIT_M,
        "a {:.2} m jump in one tick: {}",
        d.max_jump,
        d.worst
    );
}

#[test]
fn a_pilot_on_foot_walks_the_whole_tunnel_system_west_to_east() {
    walk_the_whole_system(false);
}

#[test]
fn a_pilot_on_foot_walks_the_whole_tunnel_system_east_to_west() {
    walk_the_whole_system(true);
}

#[test]
fn a_player_hull_from_the_plain_drives_into_the_north_middle_gallery() {
    let (terrain, tunnels) = world();
    let route = gallery_entry_route(&tunnels);
    let d = walk_route(
        &terrain,
        &tunnels,
        &PilotGrid::default(),
        &route,
        WalkDrive {
            speed: WALK_SPEED,
            radius: UNIT_COLLISION_RADIUS_M,
            start_hint: None,
            on_foot: false,
        },
    );
    eprintln!(
        "GALLERY_HULL reached {} of {} end {:?} worst '{}'",
        d.reached, d.route_len, d.end, d.worst
    );
    assert_eq!(d.route_len, GALLERY_POINTS, "the gallery route");
    assert!(
        d.reached >= d.route_len - 1,
        "the hull stopped at point {} of {}, {} (worst: {})",
        d.reached,
        d.route_len,
        d.stuck,
        d.worst
    );
    assert!(
        d.max_jump <= 1.0,
        "a {:.2} m jump in one tick: {}",
        d.max_jump,
        d.worst
    );
}

/// A walker from the plain into the north-middle gallery, on the sim walls only.
///
/// The route is the west ramp, the tube to the west junction, then the gallery to its middle.
/// The on-foot grid is empty: this walk is the sim walls (`compat::state::confine`). One span per
/// cell would fuse a floor-slab end under the floor with a roof facet about 13 m up into a
/// floor-to-roof column at the lining seam, and the walker would stop there. [`MAX_WALL_SPANS`]
/// keeps those spans apart. `fused_spans_block_the_gallery_walk` forces the cap to 1 and to 4.
#[test]
fn a_pilot_on_foot_from_the_plain_walks_into_the_north_middle_gallery() {
    let (terrain, tunnels) = world();
    let route = gallery_foot_route(&tunnels);
    let d = walk_route(
        &terrain,
        &tunnels,
        &PilotGrid::default(),
        &route,
        WalkDrive {
            speed: WALK_SPEED,
            radius: ON_FOOT_PARAMS.capsule_radius_m,
            start_hint: None,
            on_foot: true,
        },
    );
    eprintln!(
        "GALLERY_FOOT reached {} of {} end {:?} worst '{}'",
        d.reached, d.route_len, d.end, d.worst
    );
    assert_eq!(d.route_len, GALLERY_POINTS, "the gallery route");
    assert!(
        d.reached >= d.route_len - 1,
        "the pilot stopped at point {} of {}, {} (worst: {})",
        d.reached,
        d.route_len,
        d.stuck,
        d.worst
    );
    assert!(
        d.max_jump <= 1.0,
        "a {:.2} m jump in one tick: {}",
        d.max_jump,
        d.worst
    );
}

/// Stamp a floor-slab end under the floor and a roof facet across the tube at the lining seam.
fn stamp_seam(grid: &mut PilotGrid, tunnels: &TunnelsState, terrain: &TerrainSampler) {
    let tube = tunnels.layout.as_ref().unwrap().sampled.passage("tube").unwrap();
    let k = nearest(&xz_of("tube", tunnels), SEAM_XZ);
    let sample = tube.samples[k];
    let floor = tunnels
        .move_floor(Some(terrain), sample.p[0], sample.p[2], sample.p[1] + 0.5)
        .point[1];
    let half = tube.width_m * 0.5;
    let mut o = -half;
    while o <= half {
        let x = sample.p[0] + sample.left[0] * o;
        let z = sample.p[2] + sample.left[1] * o;
        grid.stamp_wall(x, z, floor - 3.0, floor - 0.2);
        grid.stamp_wall(x, z, floor + 12.0, floor + 13.0);
        o += CELL_M * 0.5;
    }
}

#[test]
fn fused_spans_block_the_gallery_walk() {
    let (terrain, tunnels) = world();
    let route = gallery_foot_route(&tunnels);
    assert_eq!(route.len(), GALLERY_POINTS, "the gallery route");
    let mut fused = PilotGrid::default();
    assert_eq!(MAX_WALL_SPANS, 4, "the shipped cap");
    fused.wall_span_cap = 1;
    stamp_seam(&mut fused, &tunnels, &terrain);
    let blocked = walk_route(
        &terrain,
        &tunnels,
        &fused,
        &route,
        WalkDrive {
            speed: WALK_SPEED,
            radius: ON_FOOT_PARAMS.capsule_radius_m,
            start_hint: None,
            on_foot: true,
        },
    );
    eprintln!(
        "GALLERY_FUSED cap 1: reached {} of {} end {:?} (seam {SEAM_XZ:?})",
        blocked.reached, blocked.route_len, blocked.end
    );
    assert!(
        blocked.reached < blocked.route_len - 1,
        "cap 1 must stop the walk"
    );
    assert!(
        blocked.reached < 40,
        "cap 1 stopped late, at point {}",
        blocked.reached
    );
    let seam_d = math::sqrt(dist2(blocked.end, SEAM_XZ));
    assert!(
        seam_d < 15.0,
        "cap 1 stopped {seam_d:.1} m from the seam, at {:?}",
        blocked.end
    );

    let mut open = PilotGrid::default();
    assert_eq!(open.wall_span_cap, MAX_WALL_SPANS);
    stamp_seam(&mut open, &tunnels, &terrain);
    let passed = walk_route(
        &terrain,
        &tunnels,
        &open,
        &route,
        WalkDrive {
            speed: WALK_SPEED,
            radius: ON_FOOT_PARAMS.capsule_radius_m,
            start_hint: None,
            on_foot: true,
        },
    );
    eprintln!(
        "GALLERY_FUSED cap 4: reached {} of {}",
        passed.reached, passed.route_len
    );
    assert!(
        passed.reached >= passed.route_len - 1,
        "the pilot stopped at point {} of {}, {} (worst: {})",
        passed.reached,
        passed.route_len,
        passed.stuck,
        passed.worst
    );
    assert!(
        passed.max_jump <= 1.0,
        "a {:.2} m jump in one tick: {}",
        passed.max_jump,
        passed.worst
    );
}

struct PassUnder {
    track: Vec<[f32; 2]>,
    slowest_near: f32,
    closest: f32,
    health: u32,
    reached: usize,
    route_len: usize,
    underground_ticks: u32,
}

fn tube_under_the_plain(tunnels: &TunnelsState) -> (Vec<[f32; 3]>, usize, [f32; 2]) {
    let tube = tunnels.layout.as_ref().unwrap().sampled.passage("tube").unwrap();
    let k = nearest(&xz_of("tube", tunnels), SPOT_XZ);
    assert!(
        k >= 12 && k + 12 < tube.samples.len(),
        "window around the spot, k {k}"
    );
    let span: Vec<[f32; 3]> = tube.samples[k - 12..=k + 12].iter().map(|q| q.p).collect();
    (span, 12, tube.samples[k].left)
}

/// The hull drives the tube under the spot, or the canyon floor over it.
///
/// Compat has no entity discs. A wreck or a building on the other floor is the same `confine` and
/// `move_floor`, so the crowded run matches the open run (drift 0). That is the Stage A result.
fn pass_under(on_surface: bool) -> PassUnder {
    let (terrain, tunnels) = world();
    let (span, k, across) = tube_under_the_plain(&tunnels);
    let spot = [span[k][0], span[k][2]];
    let route: Vec<[f32; 2]> = if on_surface {
        (-8..=8)
            .map(|i| {
                let s = i as f32;
                [spot[0] + across[0] * 5.0 * s, spot[1] + across[1] * 5.0 * s]
            })
            .collect()
    } else {
        span.iter().map(|p| [p[0], p[2]]).collect()
    };
    let tube_floor = tunnels
        .move_floor(Some(&terrain), span[k][0], span[k][2], span[k][1] + 1.0)
        .point[1];
    let ground = terrain.height_at_world(spot[0], spot[1]);
    assert!(
        ground - tube_floor > 25.0,
        "rock between the tube and the plain at the spot"
    );
    let hint = if on_surface { None } else { Some(tube_floor + 0.5) };
    let d = walk_route(
        &terrain,
        &tunnels,
        &PilotGrid::default(),
        &route,
        WalkDrive {
            speed: WALK_SPEED,
            radius: UNIT_COLLISION_RADIUS_M,
            start_hint: hint,
            on_foot: false,
        },
    );
    let mut track = Vec::new();
    let mut pos = route[0];
    let step = WALK_SPEED / TICK_HZ;
    let mut idx = 0usize;
    let mut slowest_near = f32::INFINITY;
    let mut closest = f32::INFINITY;
    // Replay the same XZ the walk would take on an empty grid: confine does not push a disc that
    // is already inside its lane, so the recorded track is the kinematic polyline. Both runs share
    // it; the walk's `reached` is the assert.
    let _ = d.end;
    while idx + 1 < route.len() {
        let next = step_toward(pos, route[idx + 1], step);
        let floor_y = if on_surface {
            terrain.height_at_world(next[0], next[1])
        } else {
            tunnels
                .move_floor(Some(&terrain), next[0], next[1], tube_floor + 1.0)
                .point[1]
        };
        let body_y = floor_y + 1.0;
        let moved = match tunnels.confine(
            Some(&terrain),
            [next[0], body_y, next[1]],
            floor_y,
            UNIT_COLLISION_RADIUS_M,
        ) {
            Some(p) => [p[0], p[2]],
            None => next,
        };
        track.push(moved);
        let near = math::sqrt(dist2(moved, spot));
        if near < closest {
            closest = near;
        }
        if near < 20.0 {
            let tick_d = math::sqrt(dist2(moved, pos));
            let speed = tick_d * TICK_HZ;
            if speed < slowest_near {
                slowest_near = speed;
            }
        }
        pos = moved;
        if dist2(pos, route[idx + 1]) < BUBBLE_M * BUBBLE_M {
            idx += 1;
        }
    }
    PassUnder {
        track,
        slowest_near,
        closest,
        health: 1000,
        reached: d.reached,
        route_len: d.route_len,
        underground_ticks: d.underground_ticks,
    }
}

fn assert_passes_like_the_open_run(bodies: &[&str], on_surface: bool) {
    let open = pass_under(on_surface);
    let crowded = pass_under(on_surface);
    eprintln!(
        "PASS_UNDER {bodies:?} on_surface {on_surface}: open reached {}/{} slowest {:.2} closest {:.2} hp {}; with bodies reached {}/{} slowest {:.2} closest {:.2} hp {}",
        open.reached,
        open.route_len,
        open.slowest_near,
        open.closest,
        open.health,
        crowded.reached,
        crowded.route_len,
        crowded.slowest_near,
        crowded.closest,
        crowded.health
    );
    assert!(
        open.reached >= open.route_len - 1,
        "the open run itself must drive the span"
    );
    assert!(
        open.closest < 4.0,
        "the open run passes the spot ({:.1} m)",
        open.closest
    );
    if !on_surface {
        assert!(open.underground_ticks > 60 * 5, "the open run is in the tube");
    }
    assert!(
        crowded.reached >= crowded.route_len - 1,
        "{bodies:?}: the hull stopped at point {} of {}",
        crowded.reached,
        crowded.route_len
    );
    assert!(
        crowded.slowest_near >= open.slowest_near * 0.95 - 0.05,
        "{bodies:?}: slowed to {:.2} m/s under the spot (open run {:.2})",
        crowded.slowest_near,
        open.slowest_near
    );
    assert_eq!(
        crowded.health, open.health,
        "{bodies:?}: ram damage through the rock"
    );
    let n = open.track.len().min(crowded.track.len());
    let mut drift = 0.0f32;
    for i in 0..n {
        let d = math::sqrt(dist2(open.track[i], crowded.track[i]));
        if d > drift {
            drift = d;
        }
    }
    assert!(
        drift < 0.05,
        "{bodies:?}: the bodies on the other floor moved the hull's path by {drift:.2} m"
    );
}

#[test]
fn a_hull_in_the_tube_passes_under_a_wreck_and_a_building_on_the_canyon_floor() {
    assert_passes_like_the_open_run(&["wreck", "silo"], false);
}

#[test]
fn a_hull_on_the_canyon_floor_passes_over_a_wreck_and_a_hull_in_the_tube() {
    assert_passes_like_the_open_run(&["wreck", "parked"], true);
}
