// SPDX-License-Identifier: MPL-2.0
// Requirements from Direct Order 27 (ir-core/tests/tunnels_switchbacks.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 1117-1200 (the tunnel fight behind DO27's lockstep pin), mark: requirements (Stage A: unchanged; the
// range is the same at regia-absorb-2 e6c84ba6d). No code copied: DO27's fight needs its whole sim (AI orders,
// weapons, the net snapshot hash). It is rebuilt here as a Regia two-peer scenario over `regia_detsim::hover`
// and the native `regia_world_geom` query, with a drive through every mouth, a blocker flip and a crater.
// DO27 keeps its own pin. Plan row W-29, spec 121 § 13.2, § 13.3, § 14 rule 3.

//! W-29 native acceptance suite (plan row W-29): the Stage B bars on the cooked fixtures.
//!
//! - `two_peer_tunnel_drive_checksums_equal_x86_aarch64`: two peers load the same cooked bytes and
//!   step two hover hulls for 3600 ticks through every Switchbacks mouth, with a blocker flip and a
//!   crater; their per-tick checksums (hull state, hits and `GeomState::state_hash`) are equal, and
//!   the run is pinned per target in `goldens/tunnel_drive_two_peer.detgold`.
//! - `native_mouth_hop_every_hull_class_le_5cm_per_tick`: W-11's 5 cm hop bar on every fixture
//!   and every hull class (the pilot and the hover hull, each with its wall push).
//! - `cinder_south_junction_hop_le_5cm_per_tick` (K7): the same meter across the Cinder Steps'
//!   south junction, every passage pair.
//! - `rubble_drop_le_step_bound` (K3): the whole-system walk on Stage B drops at most one pilot
//!   step in a tick (DO27's 2.0 m `WALK_JUMP_LIMIT_M` allowance removed).
//! - `native_walks_reach_every_point`: the 688-point whole-system walk both ways and the 69-point
//!   gallery walk on the native query.
//!
//! Every helper is lockstep-shaped: IEEE `+ - * / sqrt` in source order, transcendentals through
//! `regia_detsim::math`, sign-sensitive min/max through `regia_detsim::float`.

#![cfg(all(feature = "stage_b", feature = "fixtures"))]
#![allow(clippy::unwrap_used)]

pub mod native_mouths;

use regia_detsim::float::{det_max, det_min};
use regia_detsim::golden::{self, DetRun, DetScenario, GoldenMode, REPORT_PREFIX, StateHasher};
use regia_detsim::hover::{self, HoverParams, HoverState};
use regia_detsim::math;
use regia_detsim::tick::TickRate;
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::hover_ground::LEDGE_M;
use regia_world_geom::compat::layout::{SampledLayout, TunnelLayout};
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::compat::state::{PILOT_STEP_M, UNIT_COLLISION_RADIUS_M};
use regia_world_geom::cook::{CookInputs, GroundInput, cook};
use regia_world_geom::passages::PassagesDocument;
use regia_world_geom::{Band, BlockerId, FaceMask, GeomState, GroundEdit, SurfaceHit, WorldGeom, m_to_mm};

/// The fixed tick (60 Hz, the rate DO27's sim and the hop meter use).
const TICK_HZ: f32 = 60.0;
/// The hop bar of spec 121 § 13.3 (E1, K7, W-11): metres per tick at full speed.
const HOP_BAR_M: f32 = 0.05;
/// How far over the feet a body asks for its floor (DO27's `BODY_REACH_M`, the waist rule's reach).
const REACH_M: f32 = 0.9;
/// The pilot's capsule radius and height (spec § 4.7 nav agents: 0.35 m, 1.8 m tall).
const PILOT_RADIUS_M: f32 = 0.35;
const PILOT_HEIGHT_M: f32 = 1.8;
/// The hover hull's ride over its floor (the host hover's set altitude, W-05's `HULL_RIDE_M`).
const HULL_RIDE_M: f32 = 1.0;

fn dist2(a: [f32; 2], b: [f32; 2]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[1] - b[1];
    dx * dx + dz * dz
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

/// The 17-sample / 10-halving discontinuity meter of the accepted Stage A hop test (W-02/W-11).
fn discontinuity(f: impl Fn(f32) -> f32) -> f32 {
    let mut worst = (0.0, 0.0, 0.0, 0.0, 0.0);
    let mut previous = f(0.0);
    for i in 1..=16 {
        let (a, b) = ((i - 1) as f32 / 16.0, i as f32 / 16.0);
        let current = f(b);
        if (current - previous).abs() > worst.0 {
            worst = ((current - previous).abs(), a, b, previous, current);
        }
        previous = current;
    }
    if worst.0 < 0.005 {
        return 0.0;
    }
    let (_, mut a, mut b, mut fa, mut fb) = worst;
    for _ in 0..10 {
        let middle = (a + b) * 0.5;
        let fm = f(middle);
        if (fm - fa).abs() >= (fb - fm).abs() {
            (b, fb) = (middle, fm);
        } else {
            (a, fa) = (middle, fm);
        }
    }
    (fb - fa).abs()
}

/// A hull class of spec 121 § 4.7: its wall-push radius and band, and whether it rides.
#[derive(Clone, Copy, Debug)]
struct HullClass {
    name: &'static str,
    radius: f32,
    /// The wall band above the feet: `[feet + step, feet + top]`.
    step: f32,
    top: f32,
}

const PILOT: HullClass = HullClass {
    name: "pilot",
    radius: PILOT_RADIUS_M,
    step: PILOT_STEP_M,
    top: PILOT_HEIGHT_M,
};
const HOVER_HULL: HullClass = HullClass {
    name: "hover_hull",
    radius: UNIT_COLLISION_RADIUS_M,
    // DO27's confine band (compat::state::TunnelsState::confine): [floor + 0.5, body + 2.0] at the ride.
    step: 0.5,
    top: HULL_RIDE_M + 2.0,
};

/// The floor a body at `feet` stands on at `xz` (the native support at the waist reach).
fn floor_at(geom: &WorldGeom, state: &GeomState, xz: [f32; 2], feet: f32) -> SurfaceHit {
    geom.support(state, xz[0], xz[1], feet + REACH_M).floor
}

/// One meter tick: from `from` toward `to` with the class's wall push; returns the pushed XZ, the
/// new floor and the discontinuity of the floor along the pushed leg.
fn meter_tick(
    geom: &WorldGeom,
    state: &GeomState,
    class: HullClass,
    from: [f32; 2],
    to: [f32; 2],
    feet: f32,
) -> ([f32; 2], f32, f32) {
    let band = Band {
        lo: feet + class.step,
        hi: feet + class.top,
    };
    let pushed = geom.push_disc(state, to[0], to[1], class.radius, band).xz;
    let next_feet = floor_at(geom, state, pushed, feet).y;
    let jump = discontinuity(|u| {
        geom.support(
            state,
            from[0] + (pushed[0] - from[0]) * u,
            from[1] + (pushed[1] - from[1]) * u,
            feet + (next_feet - feet) * u + REACH_M,
        )
        .floor
        .y
    });
    (pushed, next_feet, jump)
}

/// The worst one-tick hop of a drive along `route` at `speed` (m/s) for `class`, with where it was.
fn drive_hop(
    geom: &WorldGeom,
    state: &GeomState,
    class: HullClass,
    route: &[[f32; 2]],
    start_feet: f32,
    speed: f32,
) -> (f32, String, u32) {
    let step = speed / TICK_HZ;
    let mut feet = floor_at(geom, state, route[0], start_feet).y;
    let mut pos = route[0];
    let mut worst = 0.0f32;
    let mut at = String::new();
    let mut ticks = 0u32;
    for leg in route.windows(2) {
        let target = leg[1];
        let mut guard = 0u32;
        while dist2(pos, target) > 1e-6 && guard < 100_000 {
            let next = step_toward(pos, target, step);
            let (pushed, next_feet, jump) = meter_tick(geom, state, class, pos, next, feet);
            if jump > worst {
                at = format!(
                    "({:.2}, {:.3}, {:.2}) -> ({:.2}, {:.3}, {:.2})",
                    pos[0], feet, pos[1], pushed[0], next_feet, pushed[1]
                );
            }
            worst = det_max(worst, jump);
            ticks += 1;
            guard += 1;
            // A push that holds the body back ends this leg where it stands (a wall the
            // centreline brushes); the meter keeps the floor it measured.
            let stalled = dist2(pushed, pos) < 1e-10;
            pos = pushed;
            feet = next_feet;
            if stalled {
                break;
            }
            if dist2(next, target) <= 1e-6 {
                break;
            }
        }
    }
    (worst, at, ticks)
}

/// The fills a fixture tries, most mouths first: all; all but one skylight opening, each in turn;
/// all but every skylight opening.
fn fill_candidates(layout: &TunnelLayout) -> Vec<Fill> {
    let skylights: Vec<String> = layout
        .openings
        .iter()
        .filter(|o| regia_world_geom::sections::is_skylight_kind(&o.kind))
        .map(|o| o.id.clone())
        .collect();
    let mut out = vec![Fill::All];
    out.extend(skylights.iter().map(|id| Fill::Except(vec![id.clone()])));
    if skylights.len() > 1 {
        out.push(Fill::Except(skylights));
    }
    out
}

/// The fill a fixture cooks with: the first of [`fill_candidates`] whose cook passes its checks
/// (see `filled_fixtures_pass_the_cook_checks`).
fn fixture_fill(map: &str, counts: u16, extra: &str) -> Fill {
    let layout = for_map(map).unwrap();
    for fill in fill_candidates(&layout) {
        match try_cook_fixture(map, counts, extra, &fill) {
            Ok(_) => return fill,
            Err(report) => println!("FILL {map} {fill:?}: {}", failed_checks(&report).join("; ")),
        }
    }
    panic!("{map}: no fill cooks clean")
}

/// A measurement fixture on W-11's acceptance ground (`native_mouths::acceptance_ground_m`) with
/// [`fixture_fill`]'s mouths filled. Returns the geometry, the layout, its samples and the ids of
/// the openings left unfilled.
fn fixture_world(map: &str) -> (WorldGeom, TunnelLayout, SampledLayout, Vec<String>) {
    let ground = native_mouths::acceptance_ground_m(map);
    let counts = (16384.0 + ground * 32.0) as u16;
    let fill = fixture_fill(map, counts, "");
    let (bytes, layout, sampled) = cook_fixture(map, counts, "", &fill);
    let unfilled = match fill {
        Fill::All => Vec::new(),
        Fill::Except(ids) => ids,
    };
    println!("FIXTURE {map}: unfilled openings {unfilled:?}");
    (
        WorldGeom::from_canonical_bytes(&bytes).unwrap(),
        layout,
        sampled,
        unfilled,
    )
}

/// A mouth drive: 10 m outside the ramp top, down the ramp to its foot and back up and out.
fn mouth_route(sampled: &SampledLayout, ramp_id: &str) -> Vec<[f32; 2]> {
    let ramp = &sampled.passages.iter().find(|p| p.id == ramp_id).unwrap().samples;
    let last = ramp[ramp.len() - 1].p;
    let previous = ramp[ramp.len() - 2].p;
    let (dx, dz) = (last[0] - previous[0], last[2] - previous[2]);
    let length = math::sqrt(dx * dx + dz * dz);
    let outside = [last[0] + dx / length * 10.0, last[2] + dz / length * 10.0];
    let mut route = vec![outside];
    route.extend(ramp.iter().rev().map(|s| [s.p[0], s.p[2]]));
    route.extend(ramp.iter().skip(1).map(|s| [s.p[0], s.p[2]]));
    route.push(outside);
    route
}

/// W-11's `native_mouth_hop_at_full_speed_le_5cm_per_tick` on every fixture and every hull class
/// of spec 121 § 4.7 (plan row W-29): the pilot at walking and full speed, the hover hull at the
/// meter's three speeds, each with its own wall push in its own band.
#[test]
fn native_mouth_hop_every_hull_class_le_5cm_per_tick() {
    let mut failures = Vec::new();
    for map in ["switchbacks", "cinder_steps", "atacama_rail"] {
        let (geom, layout, sampled, unfilled) = fixture_world(map);
        let state = geom.new_state();
        let ground_y = native_mouths::acceptance_ground_m(map);
        for opening in &layout.openings {
            if unfilled.contains(&opening.id) {
                println!(
                    "NATIVE_HOP_CLASS {map} {}: not driven, its mouth is unfilled (owed to W-08/W-14)",
                    opening.id
                );
                continue;
            }
            let route = mouth_route(&sampled, &opening.ramp);
            for (class, speeds) in [(PILOT, &[6.0f32, 20.0][..]), (HOVER_HULL, &[50.0, 20.0, 6.0][..])] {
                for &speed in speeds {
                    let (worst, at, ticks) = drive_hop(&geom, &state, class, &route, ground_y, speed);
                    println!(
                        "NATIVE_HOP_CLASS {map} {} {} speed={speed} ticks={ticks} worst_m={worst} at {at}",
                        opening.id, class.name
                    );
                    assert!(ticks > 0);
                    if worst > HOP_BAR_M {
                        failures.push(format!(
                            "{map}/{}/{}/{speed}: {worst} m at {at}",
                            opening.id, class.name
                        ));
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "native hops over 50 mm:\n{}",
        failures.join("\n")
    );
}

/// K7 (plan § 4.3, R5): at the Cinder Steps' south junction the overlapping passage floors meet,
/// and a body crossing from any member into any other at full speed sees at most 5 cm a tick.
/// The slice is every ordered pair of the junction's passages, 40 m either side of the junction.
#[test]
fn cinder_south_junction_hop_le_5cm_per_tick() {
    let (geom, layout, sampled, _) = fixture_world("cinder_steps");
    let state = geom.new_state();
    let junction = layout.junctions.iter().find(|j| j.id == "south").unwrap();
    let at = [junction.at[0], junction.at[2]];
    let arm = |id: &str| -> Vec<[f32; 2]> {
        let samples = &sampled.passages.iter().find(|p| p.id == id).unwrap().samples;
        let near = (0..samples.len())
            .min_by(|&a, &b| {
                let pa = [samples[a].p[0], samples[a].p[2]];
                let pb = [samples[b].p[0], samples[b].p[2]];
                dist2(pa, at).total_cmp(&dist2(pb, at))
            })
            .unwrap();
        // The arm walks out of the junction along the passage, the longer way, 40 m.
        let forward: Vec<[f32; 2]> = samples[near..].iter().map(|s| [s.p[0], s.p[2]]).collect();
        let backward: Vec<[f32; 2]> = samples[..=near].iter().rev().map(|s| [s.p[0], s.p[2]]).collect();
        let pick = if forward.len() >= backward.len() {
            forward
        } else {
            backward
        };
        let mut out = vec![at];
        for p in pick {
            out.push(p);
            if dist2(p, at) > 40.0 * 40.0 {
                break;
            }
        }
        out
    };
    let mut failures = Vec::new();
    let mut slices = 0;
    for a in &junction.passages {
        for b in &junction.passages {
            if a == b {
                continue;
            }
            let mut route: Vec<[f32; 2]> = arm(a).into_iter().rev().collect();
            route.extend(arm(b).into_iter().skip(1));
            for class in [PILOT, HOVER_HULL] {
                for speed in [50.0f32, 20.0, 6.0] {
                    let (worst, where_, ticks) =
                        drive_hop(&geom, &state, class, &route, junction.at[1], speed);
                    println!(
                        "CINDER_SOUTH {a}->{b} {} speed={speed} ticks={ticks} worst_m={worst} at {where_}",
                        class.name
                    );
                    slices += 1;
                    if worst > HOP_BAR_M {
                        failures.push(format!("{a}->{b} {} {speed}: {worst} m at {where_}", class.name));
                    }
                }
            }
        }
    }
    assert_eq!(
        slices, 36,
        "three passages, six ordered pairs, two classes, three speeds"
    );
    assert!(
        failures.is_empty(),
        "south junction hops over 50 mm:\n{}",
        failures.join("\n")
    );
}

// ---------------------------------------------------------------------------------------------
// The walks of W-05 on the native query.
// ---------------------------------------------------------------------------------------------

/// The whole-system route of W-05's `tunnel_walks.rs` (688 points), on the native fixture's samples.
fn whole_system_route(sampled: &SampledLayout, reverse: bool) -> Vec<[f32; 2]> {
    let xz = |id: &str| -> Vec<[f32; 2]> {
        sampled
            .passages
            .iter()
            .find(|p| p.id == id)
            .unwrap()
            .samples
            .iter()
            .map(|q| [q.p[0], q.p[2]])
            .collect()
    };
    let nearest = |line: &[[f32; 2]], at: [f32; 2]| -> usize {
        let mut best = (0usize, f32::INFINITY);
        for (i, p) in line.iter().enumerate() {
            let d = dist2(*p, at);
            if d < best.1 {
                best = (i, d);
            }
        }
        best.0
    };
    let past_top = |l: &[[f32; 2]], d: f32| -> [f32; 2] {
        let (a, b) = (l[l.len() - 2], l[l.len() - 1]);
        let n = math::sqrt((b[0] - a[0]) * (b[0] - a[0]) + (b[1] - a[1]) * (b[1] - a[1]));
        [b[0] + (b[0] - a[0]) / n * d, b[1] + (b[1] - a[1]) / n * d]
    };
    let (tube, gallery) = (xz("tube"), xz("gallery"));
    let (rw, re) = (xz("ramp_west"), xz("ramp_east"));
    let (rmw, rme) = (xz("ramp_mid_west"), xz("ramp_mid_east"));
    let (iw, ie) = (nearest(&tube, rw[0]), nearest(&tube, re[0]));
    let (jw, je) = (
        nearest(&tube, gallery[0]),
        nearest(&tube, *gallery.last().unwrap()),
    );
    let cw = nearest(&tube, rmw[0]);
    let last = tube.len() - 1;
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

/// W-05's gallery route: 8 m past the west ramp top, down it, along the tube to the west junction,
/// then the gallery to its middle (69 points).
fn gallery_route(sampled: &SampledLayout) -> Vec<[f32; 2]> {
    let xz = |id: &str| -> Vec<[f32; 2]> {
        sampled
            .passages
            .iter()
            .find(|p| p.id == id)
            .unwrap()
            .samples
            .iter()
            .map(|q| [q.p[0], q.p[2]])
            .collect()
    };
    let nearest = |line: &[[f32; 2]], at: [f32; 2]| -> usize {
        let mut best = (0usize, f32::INFINITY);
        for (i, p) in line.iter().enumerate() {
            let d = dist2(*p, at);
            if d < best.1 {
                best = (i, d);
            }
        }
        best.0
    };
    let (tube, gallery, rw) = (xz("tube"), xz("gallery"), xz("ramp_west"));
    let iw = nearest(&tube, rw[0]);
    let jw = nearest(&tube, [-120.0, 40.0]);
    let mid = nearest(&gallery, [0.0, 150.0]);
    let mut route = vec![rw[rw.len() - 1]];
    route.extend(rw.iter().rev());
    if iw <= jw {
        route.extend(&tube[iw..=jw]);
    } else {
        route.extend(tube[jw..=iw].iter().rev());
    }
    route.extend(&gallery[..=mid]);
    let (top, prev) = (route[1], route[2]);
    let len = math::sqrt((top[0] - prev[0]) * (top[0] - prev[0]) + (top[1] - prev[1]) * (top[1] - prev[1]));
    route[0] = [
        top[0] + (top[0] - prev[0]) / len * 8.0,
        top[1] + (top[1] - prev[1]) / len * 8.0,
    ];
    route
}

/// The source's waypoint bubble (m) and the walker's speed (6 m/s).
const BUBBLE_M: f32 = 7.0;
const WALK_SPEED: f32 = 6.0;
const STUCK_TICKS: u32 = 180;

struct NativeWalk {
    reached: usize,
    route_len: usize,
    underground_ticks: u32,
    /// Largest one-tick rise and drop of the feet, metres.
    max_rise: f32,
    max_drop: f32,
    worst_drop: String,
    end: [f32; 2],
}

/// A kinematic walk on the native query: step toward the next waypoint, push off the walls in the
/// body band, stand on the support at the waist reach. A hull rides 1 m and keeps its world height
/// over a ledge (the hover ledge rule) and falls back to the ride.
fn native_walk(
    geom: &WorldGeom,
    state: &GeomState,
    route: &[[f32; 2]],
    class: HullClass,
    start_feet: f32,
) -> NativeWalk {
    let ride = if class.name == "pilot" { 0.0 } else { HULL_RIDE_M };
    let step = WALK_SPEED / TICK_HZ;
    let mut floor = floor_at(geom, state, route[0], start_feet).y;
    let mut body = floor + ride;
    let mut fall = 0.0f32;
    let mut pos = route[0];
    let mut idx = 0usize;
    let (mut max_rise, mut max_drop) = (0.0f32, 0.0f32);
    let mut worst_drop = String::new();
    let mut underground_ticks = 0u32;
    let mut still = 0u32;
    let cap_ticks = route.len().saturating_mul(400).max(1_000);
    for _ in 0..cap_ticks {
        while idx + 1 < route.len() && dist2(pos, route[idx + 1]) < BUBBLE_M * BUBBLE_M {
            idx += 1;
        }
        if idx + 1 >= route.len() {
            break;
        }
        let next = step_toward(pos, route[idx + 1], step);
        let feet = body - ride;
        let band = Band {
            lo: feet + class.step,
            hi: feet + class.top,
        };
        let pushed = geom.push_disc(state, next[0], next[1], class.radius, band).xz;
        let new_floor = floor_at(geom, state, pushed, feet).y;
        let new_body = if ride == 0.0 {
            new_floor
        } else {
            // The ledge rule (compat::hover_ground::ground_hover_hull), kinematic: a floor change
            // past LEDGE_M keeps the world height and the hull falls back to its ride.
            let mut altitude = if (new_floor - floor).abs() > LEDGE_M {
                det_max(body - new_floor, 0.0)
            } else {
                body - floor
            };
            if altitude > ride {
                fall += 9.81 / TICK_HZ;
                altitude = det_max(altitude - fall / TICK_HZ, ride);
            }
            if altitude <= ride {
                fall = 0.0;
                altitude = ride;
            }
            new_floor + altitude
        };
        let delta = new_body - body;
        max_rise = det_max(max_rise, delta);
        if -delta > max_drop {
            max_drop = -delta;
            worst_drop = format!(
                "({:.2}, {:.3}, {:.2}) -> ({:.2}, {:.3}, {:.2})",
                pos[0], body, pos[1], pushed[0], new_body, pushed[1]
            );
        }
        if geom.covered_at(state, pushed[0], pushed[1], new_body) {
            underground_ticks += 1;
        }
        if dist2(pushed, pos) < 1e-8 {
            still += 1;
            if still >= STUCK_TICKS {
                pos = pushed;
                break;
            }
        } else {
            still = 0;
        }
        pos = pushed;
        floor = new_floor;
        body = new_body;
    }
    while idx + 1 < route.len() && dist2(pos, route[idx + 1]) < BUBBLE_M * BUBBLE_M {
        idx += 1;
    }
    NativeWalk {
        reached: idx + 1,
        route_len: route.len(),
        underground_ticks,
        max_rise,
        max_drop,
        worst_drop,
        end: pos,
    }
}

/// K8 on the native query (spec 121 § 5.6; ADR-8898): a skylight climb is an open cut. At the
/// middle of every segment K8 leaves open on every skylight ramp of every fixture (the layout as the
/// cook meshes it, `sections::open_skylight_shafts`), the query answers no lid over the floor and
/// nothing solid stands over the body: a vertical segment from the body up 60 m meets no face and
/// no ground. Before ADR-8898 the cook cut the mouth from the layout before K8, and the part of the
/// climb K8 opened past that cut (Switchbacks `west` samples 5-10, `mid_east` 6-12) had no lid while
/// the uncut ground sheet stood 15-30 m over it.
#[test]
fn skylight_climbs_are_open_to_the_sky() {
    let mut failures = Vec::new();
    for map in ["switchbacks", "cinder_steps", "atacama_rail"] {
        let (geom, layout, sampled, unfilled) = fixture_world(map);
        let state = geom.new_state();
        let mut meshed = sampled.clone();
        regia_world_geom::sections::open_skylight_shafts(&mut meshed, &layout.openings);
        for opening in &layout.openings {
            if !regia_world_geom::sections::is_skylight_kind(&opening.kind) {
                continue;
            }
            assert!(
                !unfilled.contains(&opening.id),
                "{map}/{}: the fixture leaves this skylight unfilled",
                opening.id
            );
            let ramp = meshed.passages.iter().find(|p| p.id == opening.ramp).unwrap();
            let mut open = 0u32;
            for (k, w) in ramp.samples.windows(2).enumerate() {
                if ramp.covered.get(k).copied().unwrap_or(true) {
                    continue;
                }
                open += 1;
                let mid = [
                    (w[0].p[0] + w[1].p[0]) * 0.5,
                    (w[0].p[1] + w[1].p[1]) * 0.5,
                    (w[0].p[2] + w[1].p[2]) * 0.5,
                ];
                let (x, z) = (mid[0], mid[2]);
                let support = geom.support(&state, x, z, mid[1] + REACH_M);
                let body = support.floor.y + HULL_RIDE_M;
                let over = geom.segment(&state, [x, body, z], [x, body + 60.0, z], FaceMask::SOLID);
                if support.ceiling.is_some() || over.is_some() {
                    failures.push(format!(
                        "{map}/{} segment {k} ({x:.2}, {:.3}, {z:.2}): lid {:?}, over the body {:?}",
                        opening.id,
                        support.floor.y,
                        support.ceiling.map(|c| c.y),
                        over.map(|h| (h.kind, h.point[1]))
                    ));
                }
            }
            println!(
                "SKYLIGHT_CLIMB {map}/{}: {open} open segments of {}",
                opening.id,
                ramp.covered.len()
            );
            assert!(
                open > 0,
                "{map}/{}: no open segment on a skylight climb",
                opening.id
            );
        }
    }
    assert!(
        failures.is_empty(),
        "skylight climb segments that are not open to the sky:\n{}",
        failures.join("\n")
    );
}

/// W-05's walks rerun on Stage B (plan row W-29): the 688-point whole-system walk both ways and the
/// 69-point gallery walk reach every point on the native query, on foot (sim walls only, as W-05).
#[test]
fn native_walks_reach_every_point() {
    let (geom, _, sampled, unfilled) = fixture_world("switchbacks");
    assert!(
        unfilled.is_empty(),
        "blocked: the walks enter and leave by mouths the fixture cannot fill on this line ({unfilled:?}; see filled_fixtures_pass_the_cook_checks)"
    );
    let state = geom.new_state();
    let ground = native_mouths::acceptance_ground_m("switchbacks");
    for reverse in [false, true] {
        let route = whole_system_route(&sampled, reverse);
        let w = native_walk(&geom, &state, &route, PILOT, ground);
        println!(
            "NATIVE_WALK reverse {reverse}: reached {} of {} end {:?}, underground {} ticks, rise {} drop {} ({})",
            w.reached, w.route_len, w.end, w.underground_ticks, w.max_rise, w.max_drop, w.worst_drop
        );
        assert_eq!(w.route_len, 688, "the whole-system route");
        assert!(
            w.reached >= w.route_len - 1,
            "the walk stopped at point {} of {}, end {:?}",
            w.reached,
            w.route_len,
            w.end
        );
        assert!(
            w.underground_ticks > 60 * 120,
            "underground only {} ticks",
            w.underground_ticks
        );
    }
    let route = gallery_route(&sampled);
    let w = native_walk(&geom, &state, &route, PILOT, ground);
    println!(
        "NATIVE_GALLERY reached {} of {} end {:?}, rise {} drop {}",
        w.reached, w.route_len, w.end, w.max_rise, w.max_drop
    );
    assert_eq!(w.route_len, 69, "the gallery route");
    assert!(
        w.reached >= w.route_len - 1,
        "the gallery walk stopped at point {} of {}, end {:?}",
        w.reached,
        w.route_len,
        w.end
    );
}

/// K3 (plan § 4.3, R5): DO27's whole-system walk allows a 2.0 m one-tick height change
/// (`WALK_JUMP_LIMIT_M`) because a walker drops 1.4-1.65 m off the drawn rock pile at the mid-east
/// skylight. Without that allowance a walker on Stage B drops at most one pilot step in a tick.
/// Red on W-29's line, where the walk could not leave the west mouth (a 30 m drop at its ramp top);
/// green since ADR-8533's strip at that top edge (worst drop 0.158 m, ADR-8898).
#[test]
fn rubble_drop_le_step_bound() {
    let (geom, _, sampled, unfilled) = fixture_world("switchbacks");
    assert!(
        unfilled.is_empty(),
        "blocked: the walk enters and leaves by mouths the fixture cannot fill on this line ({unfilled:?}; see filled_fixtures_pass_the_cook_checks)"
    );
    let state = geom.new_state();
    let ground = native_mouths::acceptance_ground_m("switchbacks");
    let mut failures = Vec::new();
    for reverse in [false, true] {
        let route = whole_system_route(&sampled, reverse);
        let w = native_walk(&geom, &state, &route, PILOT, ground);
        println!(
            "RUBBLE_DROP reverse {reverse}: max drop {} m at {}, max rise {} m",
            w.max_drop, w.worst_drop, w.max_rise
        );
        assert!(
            w.reached >= w.route_len - 1,
            "the walk stopped at point {} of {}",
            w.reached,
            w.route_len
        );
        if w.max_drop > PILOT_STEP_M {
            failures.push(format!("reverse {reverse}: {} m at {}", w.max_drop, w.worst_drop));
        }
    }
    assert!(
        failures.is_empty(),
        "a walker drops more than one step ({PILOT_STEP_M} m) in a tick:\n{}",
        failures.join("\n")
    );
}

// ---------------------------------------------------------------------------------------------
// The two-peer tunnel drive (the fight scenario rebuilt).
// ---------------------------------------------------------------------------------------------

/// Ticks of the default run (60 s at 60 Hz).
const DRIVE_TICKS: u32 = 3600;
/// The blocker goes active, then clear again.
const BLOCKER_ON_TICK: u32 = 900;
const BLOCKER_OFF_TICK: u32 = 2100;
/// The crater lands.
const CRATER_TICK: u32 = 1500;
/// A shot every this many ticks: the segment from one hull's gun to the other's.
const SHOT_EVERY: u32 = 20;
/// The hull's eye and gun height over its body, metres.
const EYE_M: f32 = 1.5;
/// The ride and lid clamp half height of the hull, metres.
const HULL_HALF_M: f32 = 1.0;

/// Which mouths a fixture cook fills.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Fill {
    /// Every mouth the layout derives (W-10/W-11's fixtures).
    All,
    /// Every mouth but these openings' (by opening id).
    Except(Vec<String>),
}

/// Cooks a fixture on the 4096-sample lattice standing on a flat ground of `counts` (1/32 m, 16384 is
/// height 0: 8192 is the Switchbacks canyon floor at -256 m, 17536 W-05's +36 m Cinder Steps shield),
/// with the mouths `fill` names filled and the layer's `extra` (blocker plugs). The document carries the
/// content's authored waivers for its steep ramp facets (`checks::waive_ramp_facets`, W-09's own
/// Switchbacks workflow); any other failed check is returned. Returns the cooked canonical bytes, the
/// layout and its samples.
fn try_cook_fixture(
    map: &str,
    counts: u16,
    extra: &str,
    fill: &Fill,
) -> Result<(Vec<u8>, TunnelLayout, SampledLayout), regia_world_geom::TerrainBakeReport> {
    let (output, layout, sampled) = try_cook_output(map, counts, extra, fill)?;
    let bytes = regia_world_geom::cooked::encode_canonical(&output.file.canonical).unwrap();
    Ok((bytes, layout, sampled))
}

/// [`try_cook_fixture`]'s cook, returning the whole cook output.
fn try_cook_output(
    map: &str,
    counts: u16,
    extra: &str,
    fill: &Fill,
) -> Result<
    (regia_world_geom::cook::CookOutput, TunnelLayout, SampledLayout),
    regia_world_geom::TerrainBakeReport,
> {
    let terrain =
        TerrainSampler::new(HeightGrid::square(4, vec![counts; 16]).unwrap(), 1280.0).with_zero_counts(16384);
    let layout = for_map(map).unwrap();
    let sampled = layout.sample(Some(&terrain));
    let fills: Vec<String> = regia_world_geom::cut::passage_mouths(&layout, &sampled)
        .iter()
        .filter(|m| match &fill {
            Fill::All => true,
            Fill::Except(ids) => !ids.contains(&m.id),
        })
        .map(|m| format!("{:?}", m.id))
        .collect();
    let document = format!(
        "(version:1,layers:[(id:\"rings\",source:Passages(path:\"map.passages.ron\"),fills:[{}]{extra})])",
        fills.join(",")
    );
    let passages = PassagesDocument::from_layout(layout.clone()).to_ron().unwrap();
    let ground = GroundInput {
        lattice_n: 4096,
        width_m: 1280.0,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![counts; 4096 * 4096],
    };
    let read = |_: &str| Ok(passages.as_bytes().to_vec());
    let run = |doc: &str| {
        cook(&CookInputs {
            document: doc.as_bytes(),
            ground: &ground,
            terrain: Some(&terrain),
            read_file: &read,
        })
    };
    let output = match run(&document) {
        Ok(out) => out,
        Err(regia_world_geom::cook::CookError::Checks(report)) => {
            let waived = regia_world_geom::checks::waive_ramp_facets(&document, &report).unwrap();
            match run(&waived) {
                Ok(out) => out,
                Err(regia_world_geom::cook::CookError::Checks(report)) => return Err(report),
                Err(e) => panic!("{map}: {e} ({})", e.code()),
            }
        }
        Err(e) => panic!("{map}: {e} ({})", e.code()),
    };
    Ok((output, layout, sampled))
}

/// The cook's section tags and ranges agree (W-08 by index, after W-14's seal drops lips): every
/// triangle tagged with a section lies in one of that section's ranges (or its plug's), and every
/// triangle in a section's range carries that section. Plugs' ranges hold only blocker triangles. Fails on the
/// merge without the seal's section bookkeeping (`retain_with_meta`, ADR-8421; W-29's `retain_layer`, ADR-8538: the
/// filled Switchbacks cook shifted sections).
#[test]
fn cooked_sections_hold_their_own_triangles() {
    let mut problems = Vec::new();
    for (map, counts) in [
        ("switchbacks", 8192u16),
        ("cinder_steps", 16384),
        ("atacama_rail", 16384),
    ] {
        let extra = if map == "switchbacks" {
            ",blockers:[(passage:\"tube\",from_s_m:200.0,to_s_m:230.0)]"
        } else {
            ""
        };
        let output = match try_cook_output(map, counts, extra, &Fill::All) {
            Ok((output, _, _)) => output,
            Err(report) => {
                problems.push(format!(
                    "{map}: the filled cook fails: {}",
                    failed_checks(&report).join("; ")
                ));
                continue;
            }
        };
        let c = &output.file.canonical;
        let mut by_ref = std::collections::BTreeMap::new();
        for tile in &c.tiles {
            for t in &tile.triangles {
                by_ref.insert((t.layer, t.index), (t.section, t.kind));
            }
        }
        for section in &c.sections {
            for r in &section.triangles {
                for index in r.start..r.end {
                    match by_ref.get(&(r.layer, index)) {
                        Some((tag, _)) if *tag == Some(section.id) => {}
                        other => problems.push(format!(
                            "{map}: section {} ({}) range holds triangle {index} tagged {other:?}",
                            section.id, section.source
                        )),
                    }
                }
            }
        }
        for (&(layer, index), (tag, _)) in &by_ref {
            if let Some(id) = tag {
                let section = c.sections.iter().find(|s| s.id == *id).unwrap();
                let in_section = section
                    .triangles
                    .iter()
                    .any(|r| r.layer == layer && (r.start..r.end).contains(&index));
                // A plug's triangles carry its section through the plug's own range (W-08).
                let in_plug = c.plugs.iter().any(|p| {
                    p.section == *id
                        && p.triangles.layer == layer
                        && (p.triangles.start..p.triangles.end).contains(&index)
                });
                if !in_section && !in_plug {
                    problems.push(format!(
                        "{map}: triangle {index} tagged section {id} is outside its ranges"
                    ));
                }
            }
        }
        for plug in &c.plugs {
            for index in plug.triangles.start..plug.triangles.end {
                let kind = by_ref.get(&(plug.triangles.layer, index)).map(|(_, k)| *k);
                if kind != Some(regia_world_geom::cooked::TriKind::Blocker) {
                    problems.push(format!(
                        "{map}: plug {} range holds triangle {index} of kind {kind:?}",
                        plug.id
                    ));
                }
            }
        }
        println!(
            "SECTION_TAGS {map}: {} sections, {} plugs, {} triangles checked",
            c.sections.len(),
            c.plugs.len(),
            by_ref.len()
        );
    }
    let shown: Vec<&String> = problems.iter().take(20).collect();
    assert!(
        problems.is_empty(),
        "{} problems (first {}):\n{}",
        problems.len(),
        shown.len(),
        shown.iter().map(|p| p.as_str()).collect::<Vec<_>>().join("\n")
    );
}

/// The failed, unwaived checks of a report, one line each.
fn failed_checks(report: &regia_world_geom::TerrainBakeReport) -> Vec<String> {
    report
        .checks
        .iter()
        .filter(|c| !c.passed && !c.waived)
        .map(|c| format!("{} at {}: {}", c.code, c.location, c.measured))
        .collect()
}

fn cook_fixture(map: &str, counts: u16, extra: &str, fill: &Fill) -> (Vec<u8>, TunnelLayout, SampledLayout) {
    try_cook_fixture(map, counts, extra, fill).unwrap_or_else(|report| {
        panic!(
            "{map} ({fill:?}): the cook's checks fail:\n{}",
            failed_checks(&report).join("\n")
        )
    })
}

/// The acceptance fixtures cook clean (spec 121 § 10, W-09's checks) with every mouth filled, after
/// the content's authored facet waivers. W-29 found Switchbacks failing `shaft_lid_present` with the
/// mid-east mouth filled (the seal shifted W-08's sections, fixed by ADR-8421); every fixture cooks
/// clean with every mouth filled since, and [`fixture_fill`] keeps its fallback for a future map.
#[test]
fn filled_fixtures_pass_the_cook_checks() {
    let mut failures = Vec::new();
    for (map, counts) in [
        ("switchbacks", 8192u16),
        ("cinder_steps", 17536),
        ("atacama_rail", 16384),
    ] {
        for fill in fill_candidates(&for_map(map).unwrap()) {
            match try_cook_fixture(map, counts, "", &fill) {
                Ok(_) => println!("COOK_CHECKS {map} {fill:?}: passed"),
                Err(report) => {
                    let lines = failed_checks(&report);
                    println!("COOK_CHECKS {map} {fill:?}: {}", lines.join("; "));
                    if fill == Fill::All {
                        failures.push(format!("{map}: {}", lines.join("; ")));
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "filled fixtures fail the cook checks:\n{}",
        failures.join("\n")
    );
}

/// The drive's map: Switchbacks on the canyon floor with one blocker plug in the tube, with
/// [`fixture_fill`]'s mouths. Also returns the openings left unfilled (the drive skips them).
fn drive_world_bytes() -> (Vec<u8>, TunnelLayout, SampledLayout, Vec<String>) {
    let extra = ",blockers:[(passage:\"tube\",from_s_m:200.0,to_s_m:230.0)]";
    let fill = fixture_fill("switchbacks", 8192, extra);
    let (bytes, layout, sampled) = cook_fixture("switchbacks", 8192, extra, &fill);
    let unfilled = match fill {
        Fill::All => Vec::new(),
        Fill::Except(ids) => ids,
    };
    (bytes, layout, sampled, unfilled)
}

/// One seat's scripted tunnel drive: from outside the first mouth down its ramp, along the tube to
/// the next ramp's foot, up and out through that mouth, back down it, and on through every mouth in
/// turn (each is entered and left), ending outside the last one.
fn seat_route(
    layout: &TunnelLayout,
    sampled: &SampledLayout,
    unfilled: &[String],
    reverse: bool,
) -> Vec<[f32; 2]> {
    let xz = |id: &str| -> Vec<[f32; 2]> {
        sampled
            .passages
            .iter()
            .find(|p| p.id == id)
            .unwrap()
            .samples
            .iter()
            .map(|q| [q.p[0], q.p[2]])
            .collect()
    };
    let tube = xz("tube");
    let nearest = |at: [f32; 2]| -> usize {
        let mut best = (0usize, f32::INFINITY);
        for (i, p) in tube.iter().enumerate() {
            let d = dist2(*p, at);
            if d < best.1 {
                best = (i, d);
            }
        }
        best.0
    };
    let mut ramps: Vec<String> = layout
        .openings
        .iter()
        .filter(|o| !unfilled.contains(&o.id))
        .map(|o| o.ramp.clone())
        .collect();
    if reverse {
        ramps.reverse();
    }
    let mut route = Vec::new();
    let mut at_tube: Option<usize> = None;
    for ramp in &ramps {
        let mouth = mouth_route(sampled, ramp);
        // mouth = [outside, top .. foot, .. top, outside]; its middle sample is the ramp's foot.
        let foot_at = mouth.len() / 2;
        let foot = mouth[foot_at];
        let k = nearest(foot);
        if let Some(from) = at_tube {
            // Along the tube from the last foot to this one, then up this ramp and out.
            if from <= k {
                route.extend(&tube[from..=k]);
            } else {
                route.extend(tube[k..=from].iter().rev());
            }
            route.extend(&mouth[foot_at..]);
            // Back in through the same mouth to its foot.
            route.extend(mouth[1..=foot_at].iter());
        } else {
            route.extend(&mouth[..=foot_at]);
        }
        at_tube = Some(k);
    }
    // Out through the first mouth's ramp at the end, so the drive ends on the surface.
    let first = mouth_route(sampled, &ramps[0]);
    let k = nearest(first[first.len() / 2]);
    if let Some(from) = at_tube {
        if from <= k {
            route.extend(&tube[from..=k]);
        } else {
            route.extend(tube[k..=from].iter().rev());
        }
    }
    route.extend(&first[first.len() / 2..]);
    route
}

#[derive(Clone, Debug)]
struct Seat {
    route: Vec<[f32; 2]>,
    idx: usize,
    speed: f32,
    xz: [f32; 2],
    body: f32,
    floor: f32,
    floor_tri: u64,
    hover: HoverState,
    /// Ticks spent under a lid, ticks with the other hull in sight, shots that hit it.
    underground: u32,
    sight: u32,
    hits: u32,
    /// The last shot's first solid hit: fraction bits and surface (0 none, 1 + kind).
    shot_t: u32,
    shot_kind: u32,
    /// Route legs finished (a mouth counts when both of its legs are done).
    legs_done: u32,
}

/// The two-seat tunnel drive on one peer.
struct TunnelDrive {
    geom: WorldGeom,
    state: GeomState,
    params: HoverParams,
    routes: [Vec<[f32; 2]>; 2],
    seats: Vec<Seat>,
    crater: GroundEdit,
    dt: f32,
}

impl TunnelDrive {
    fn new(bytes: &[u8], layout: &TunnelLayout, sampled: &SampledLayout, unfilled: &[String]) -> TunnelDrive {
        let geom = WorldGeom::from_canonical_bytes(bytes).unwrap();
        let routes = [
            seat_route(layout, sampled, unfilled, false),
            seat_route(layout, sampled, unfilled, true),
        ];
        // The crater: on the canyon floor over the tube's middle, so it meets the tube's roof cover.
        let tube = &sampled.passages.iter().find(|p| p.id == "tube").unwrap().samples;
        let mid = tube[tube.len() / 2].p;
        let crater = GroundEdit::Crater {
            x_mm: m_to_mm(mid[0]),
            z_mm: m_to_mm(mid[2]),
            radius_mm: 6_000,
            depth_mm: 3_000,
        };
        let state = geom.new_state();
        let mut drive = TunnelDrive {
            geom,
            state,
            params: HoverParams::DEFAULT_HULL,
            routes,
            seats: Vec::new(),
            crater,
            dt: TickRate::DEFAULT.dt(),
        };
        drive.reset(&drive.default_run());
        drive
    }

    fn step_seat(&mut self, i: usize) {
        let (geom, state) = (&self.geom, &self.state);
        let s = &mut self.seats[i];
        let step = s.speed / TICK_HZ;
        while s.idx + 1 < s.route.len() && dist2(s.xz, s.route[s.idx + 1]) < 1.0 {
            s.idx += 1;
            s.legs_done += 1;
        }
        let target = s.route[(s.idx + 1).min(s.route.len() - 1)];
        // DO27's host order (K4): the hover step reads the floor at the pre-move XZ ...
        let feet = s.floor;
        let pre = floor_at(geom, state, s.xz, feet).y;
        hover::step(&self.params, &mut s.hover, &[0.0, 0.0, 0.0], pre, self.dt);
        // ... the mover goes toward the waypoint and the wall push holds it in the passage ...
        let next = step_toward(s.xz, target, step);
        let band = Band {
            lo: feet + HOVER_HULL.step,
            hi: feet + HOVER_HULL.top,
        };
        let pushed = geom
            .push_disc(state, next[0], next[1], HOVER_HULL.radius, band)
            .xz;
        // ... and the ground pass reads the floor at the post-move XZ (the ledge rule).
        let hit = floor_at(geom, state, pushed, feet);
        let post = hit.y;
        let mut altitude = s.hover.altitude;
        if (post - pre).abs() > LEDGE_M {
            altitude = det_max(s.body - post, 0.0);
            if altitude == 0.0 {
                s.hover.vertical_vel = det_max(s.hover.vertical_vel, 0.0);
            }
        }
        s.hover.altitude = altitude;
        s.hover.prev_terrain = Some(post);
        let body = geom.clamp_under_ceiling(state, pushed[0], post + altitude, pushed[1], post, HULL_HALF_M);
        s.xz = pushed;
        s.body = body;
        s.floor = post;
        s.floor_tri = hit
            .tri
            .map_or(u64::MAX, |t| (u64::from(t.layer) << 32) | u64::from(t.triangle));
        if geom.covered_at(state, pushed[0], pushed[1], body) {
            s.underground += 1;
        }
    }
}

impl DetScenario for TunnelDrive {
    fn name(&self) -> &str {
        "tunnel_drive_two_peer"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(0x5eed_0d29, DRIVE_TICKS, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        self.dt = run.rate.dt();
        self.state = self.geom.new_state();
        // Each seat's speed finishes its route by about tick 3000 (capped at full speed, 50 m/s).
        let speed = |route: &[[f32; 2]]| {
            let mut len = 0.0f32;
            for leg in route.windows(2) {
                len += math::sqrt(dist2(leg[0], leg[1]));
            }
            det_min(det_max(len / 50.0, 6.0), 50.0)
        };
        self.seats = self
            .routes
            .iter()
            .map(|route| {
                let ground = native_mouths::acceptance_ground_m("switchbacks");
                let floor = floor_at(&self.geom, &self.state, route[0], ground).y;
                let hover = HoverState {
                    altitude: self.params.ride_height,
                    prev_terrain: Some(floor),
                    ..HoverState::default()
                };
                Seat {
                    route: route.clone(),
                    idx: 0,
                    speed: speed(route),
                    xz: route[0],
                    body: floor + self.params.ride_height,
                    floor,
                    floor_tri: u64::MAX,
                    hover,
                    underground: 0,
                    sight: 0,
                    hits: 0,
                    shot_t: 0,
                    shot_kind: 0,
                    legs_done: 0,
                }
            })
            .collect();
    }

    fn step(&mut self, tick: u32) {
        if tick == BLOCKER_ON_TICK || tick == BLOCKER_OFF_TICK {
            self.state
                .set_blocker(&self.geom, BlockerId(0), tick == BLOCKER_ON_TICK)
                .unwrap();
        }
        if tick == CRATER_TICK {
            self.state.apply_ground_edit(&self.geom, &self.crater).unwrap();
        }
        for i in 0..self.seats.len() {
            self.step_seat(i);
        }
        // The fight: each hull looks at the other; every SHOT_EVERY ticks it fires the segment from
        // its gun to the other's body and scores a hit when nothing solid is in the way.
        let eye = |s: &Seat| [s.xz[0], s.body + EYE_M, s.xz[1]];
        let (e0, e1) = (eye(&self.seats[0]), eye(&self.seats[1]));
        if self.geom.line_of_sight(&self.state, e0, e1) {
            self.seats[0].sight += 1;
            self.seats[1].sight += 1;
        }
        if tick.is_multiple_of(SHOT_EVERY) {
            for (shooter, from, to) in [(0usize, e0, e1), (1, e1, e0)] {
                let hit = self.geom.segment(&self.state, from, to, FaceMask::SOLID);
                let s = &mut self.seats[shooter];
                match hit {
                    Some(h) => {
                        s.shot_t = h.t.to_bits();
                        s.shot_kind = 1 + h.kind as u32;
                    }
                    None => {
                        s.shot_t = 0;
                        s.shot_kind = 0;
                        s.hits += 1;
                    }
                }
            }
        }
    }

    fn hash_state(&self, h: &mut StateHasher) {
        h.u64("geom_state", self.state.state_hash());
        h.u64("generation", self.state.generation().0);
        for (i, s) in self.seats.iter().enumerate() {
            h.item("seat", i as u32);
            h.f32("x", s.xz[0]);
            h.f32("y", s.body);
            h.f32("z", s.xz[1]);
            h.f32("floor", s.floor);
            h.u64("floor_tri", s.floor_tri);
            h.f32("altitude", s.hover.altitude);
            h.f32("vertical_vel", s.hover.vertical_vel);
            h.u32("idx", s.idx as u32);
            h.u32("underground", s.underground);
            h.u32("sight", s.sight);
            h.u32("hits", s.hits);
            h.u32("shot_t", s.shot_t);
            h.u32("shot_kind", s.shot_kind);
        }
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "test harness: the golden directory override, read once before the run (ADR-8367)"
)]
fn golden_dir() -> std::path::PathBuf {
    match std::env::var_os(golden::GOLDEN_DIR_ENV) {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("goldens"),
    }
}

/// Plan row W-29, spec 121 § 13.2 (the tunnel-fight requirement, rebuilt): two peers load the same
/// cooked bytes and step two hover hulls through every Switchbacks mouth for 3600 ticks, with a
/// blocker flip and a crater, firing segments at each other. Their per-tick checksums, which fold
/// in `GeomState::state_hash`, are equal; the run is pinned per target in
/// `goldens/tunnel_drive_two_peer.detgold`, recorded on x86-64 Linux and checked on aarch64 (the
/// Mac; the qemu-user and Android device legs are listed in the book page).
#[test]
fn two_peer_tunnel_drive_checksums_equal_x86_aarch64() {
    let (bytes, layout, sampled, unfilled) = drive_world_bytes();
    // A mouth whose ramp top has no floor within 1 m of the top sample (the west skylight's 30 m
    // drop on this line, `native_mouth_hop_every_hull_class_le_5cm_per_tick`) cannot be driven
    // through; the drive skips it and says so.
    let probe = WorldGeom::from_canonical_bytes(&bytes).unwrap();
    let probe_state = probe.new_state();
    let mut skipped = unfilled.clone();
    for o in &layout.openings {
        let ramp = &sampled.passages.iter().find(|p| p.id == o.ramp).unwrap().samples;
        let top = ramp[ramp.len() - 1].p;
        let floor = floor_at(&probe, &probe_state, [top[0], top[2]], top[1]).y;
        if (floor - top[1]).abs() > 1.0 && !skipped.contains(&o.id) {
            println!(
                "TUNNEL_DRIVE {}: no floor at the ramp top ({floor} m under a top at {} m)",
                o.id, top[1]
            );
            skipped.push(o.id.clone());
        }
    }
    println!("TUNNEL_DRIVE mouths not driven on this line: {skipped:?}");
    let mut a = TunnelDrive::new(&bytes, &layout, &sampled, &skipped);
    let mut b = TunnelDrive::new(&bytes, &layout, &sampled, &skipped);
    assert_eq!(
        a.geom.content_hash(),
        b.geom.content_hash(),
        "the peers load the same map"
    );
    assert!(
        !a.geom.blockers().is_empty(),
        "the drive map has its tube blocker"
    );
    let run = a.default_run();
    let ta = golden::record(&mut a, &run);
    let tb = golden::record(&mut b, &run);
    assert_eq!(ta.tick_hashes.len(), DRIVE_TICKS as usize);
    if let Some(tick) = golden::first_divergence(&ta.tick_hashes, &tb.tick_hashes) {
        panic!("the two peers diverge at tick {tick}");
    }
    // The drive did what the scenario claims: both seats went through every mouth (every route
    // point passed), spent time under the lids, saw each other, the blocker flipped twice and the
    // crater moved the state hash.
    for (i, s) in a.seats.iter().enumerate() {
        println!(
            "TUNNEL_DRIVE seat {i}: point {} of {} at {:?}, underground {} ticks, sight {} ticks, hits {}, speed {}",
            s.idx + 1,
            s.route.len(),
            s.xz,
            s.underground,
            s.sight,
            s.hits,
            s.speed
        );
        assert!(
            s.idx + 1 >= s.route.len(),
            "seat {i} stopped at point {} of {}",
            s.idx + 1,
            s.route.len()
        );
        assert!(
            s.underground > 600,
            "seat {i} was under a lid for only {} ticks",
            s.underground
        );
    }
    assert!(a.seats[0].sight > 0, "the hulls never saw each other");
    assert_eq!(a.state.generation().0, 3, "a blocker on, a crater, a blocker off");
    assert!(
        a.state.state_hash() != regia_world_geom::edit::FRESH_STATE_HASH,
        "the crater stays in the state"
    );
    let report = golden::check_trace(
        &ta,
        &golden::golden_path(&golden_dir(), "tunnel_drive_two_peer"),
        &golden::current_target(),
        GoldenMode::from_env(),
    );
    println!("{REPORT_PREFIX}{}", report.to_json());
    assert!(report.passed(), "{report}");
    assert_ne!(
        report.agrees_with_reference,
        Some(false),
        "this target's run differs from the reference target's: {report}"
    );
}

// ---------------------------------------------------------------------------------------------
// W-05's sweep on the native query. The station, lane and offset helpers are W-05's
// (`tests/tunnel_sweep.rs`, from DO27's ir-core/tests/tunnel_sweep.rs 1-536 at regia-absorb-3,
// requirements here); the probe asks the native query instead of compat.
// ---------------------------------------------------------------------------------------------

/// Station step along every centreline, lateral step across a lane, the disc's margin inside its
/// lane edge, and the push tolerance of a lane (W-05's values; the bar here is push 0.000 m).
const STATION_M: f32 = 0.5;
const LATERAL_M: f32 = 0.25;
const LANE_MARGIN_M: f32 = 0.05;
const PUSH_TOL_M: f32 = 0.02;
/// The hull radii the pathing lets into a tunnel (W-05's list).
const HULL_RADII: [f32; 6] = [2.5, 2.6, 3.6, 3.9, 4.1, 4.5];

struct Station {
    s: f32,
    p: [f32; 3],
    left: [f32; 2],
    k: usize,
}

fn stations(passage: &regia_world_geom::compat::layout::SampledPassage) -> Vec<Station> {
    let mut out = Vec::new();
    for (k, w) in passage.samples.windows(2).enumerate() {
        let (a, b) = (&w[0], &w[1]);
        let seg = b.s - a.s;
        let n = ((seg / STATION_M).ceil() as usize).max(1);
        for j in 0..n {
            let f = j as f32 / n as f32;
            let p = [
                a.p[0] + (b.p[0] - a.p[0]) * f,
                a.p[1] + (b.p[1] - a.p[1]) * f,
                a.p[2] + (b.p[2] - a.p[2]) * f,
            ];
            let l = [
                a.left[0] + (b.left[0] - a.left[0]) * f,
                a.left[1] + (b.left[1] - a.left[1]) * f,
            ];
            let ln = det_max(math::sqrt(l[0] * l[0] + l[1] * l[1]), 1e-6);
            out.push(Station {
                s: a.s + seg * f,
                p,
                left: [l[0] / ln, l[1] / ln],
                k,
            });
        }
    }
    if let Some(last) = passage.samples.last() {
        out.push(Station {
            s: last.s,
            p: last.p,
            left: last.left,
            k: passage.samples.len().saturating_sub(2),
        });
    }
    out
}

fn seg_dist_xz(q: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let (abx, abz) = (b[0] - a[0], b[1] - a[1]);
    let len2 = abx * abx + abz * abz;
    let t = if len2 < 1e-9 {
        0.0
    } else {
        regia_detsim::float::det_clamp(((q[0] - a[0]) * abx + (q[1] - a[1]) * abz) / len2, 0.0, 1.0)
    };
    let dx = q[0] - a[0] - abx * t;
    let dz = q[1] - a[1] - abz * t;
    math::sqrt(dx * dx + dz * dz)
}

/// The lane of a disc of radius `r` at a station, as lateral offsets `(right, left)` (W-05's rule:
/// the passage's own wall polylines, `r` plus the margin clear of each).
fn lane(passage: &regia_world_geom::compat::layout::SampledPassage, st: &Station, r: f32) -> (f32, f32) {
    let half = passage.width_m * 0.5;
    let need = r + LANE_MARGIN_M;
    let nseg = passage.samples.len().saturating_sub(1);
    let edge = |i: usize, sign: f32| -> [f32; 2] {
        let q = &passage.samples[i];
        [q.p[0] + q.left[0] * half * sign, q.p[2] + q.left[1] * half * sign]
    };
    let mut walls: Vec<([f32; 2], [f32; 2])> = Vec::new();
    let last = nseg.saturating_sub(1);
    let from = st.k.saturating_sub(1);
    let to = if st.k + 1 < last { st.k + 1 } else { last };
    for j in from..=to {
        if passage.wall_left.get(j).copied().unwrap_or(false) {
            walls.push((edge(j, 1.0), edge(j + 1, 1.0)));
        }
        if passage.wall_right.get(j).copied().unwrap_or(false) {
            walls.push((edge(j, -1.0), edge(j + 1, -1.0)));
        }
    }
    let clear = |o: f32| {
        let c = [st.p[0] + st.left[0] * o, st.p[2] + st.left[1] * o];
        walls.iter().all(|(a, b)| seg_dist_xz(c, *a, *b) >= need)
    };
    let floor_edge = det_max(half - need, 0.0);
    let reach = |dir: f32| -> f32 {
        if !clear(0.0) {
            return 0.0;
        }
        if clear(dir * floor_edge) {
            return dir * floor_edge;
        }
        let (mut lo, mut hi) = (0.0f32, floor_edge);
        for _ in 0..24 {
            let mid = (lo + hi) * 0.5;
            if clear(dir * mid) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        dir * lo
    };
    (reach(-1.0), reach(1.0))
}

fn offsets(lo: f32, hi: f32) -> Vec<f32> {
    if hi - lo <= 0.0 {
        return vec![(lo + hi) * 0.5];
    }
    let a = (lo / LATERAL_M).ceil() as i32;
    let b = (hi / LATERAL_M).floor() as i32;
    let mut out: Vec<f32> = vec![lo];
    out.extend(
        (a..=b)
            .map(|i| i as f32 * LATERAL_M)
            .filter(|o| *o > lo && *o < hi),
    );
    out.push(hi);
    out
}

fn by_a_cap(passage: &regia_world_geom::compat::layout::SampledPassage, s: f32, r: f32) -> bool {
    let total = passage.samples.last().map_or(0.0, |q| q.s);
    (passage.cap_start && s < r + 0.6) || (passage.cap_end && s > total - r - 0.6)
}

/// The canonical triangles by reference, in integer world millimetres (the cooked geometry exactly:
/// the run-time `f32` metres are `mm / 1000`, so `m_to_mm` gives the stored millimetres back).
type MmTriangles = std::collections::BTreeMap<(u16, u32), [[i64; 3]; 3]>;

fn mm_triangles(geom: &WorldGeom) -> MmTriangles {
    geom.triangles()
        .iter()
        .map(|g| {
            (
                (g.tri.layer, g.tri.triangle),
                g.v.map(|p| p.map(|c| i64::from(m_to_mm(c)))),
            )
        })
        .collect()
}

/// The height of a hit's surface at `(x, z)`, millimetres, evaluated in `f64` on the stored
/// integer-millimetre triangle (exact to about 1e-9 mm at these magnitudes). A ground hit (no
/// triangle) is its own height: lattice heights are multiples of 1/32 m, exact in `f32`.
fn exact_height_mm(tris: &MmTriangles, hit: &SurfaceHit, x: f32, z: f32) -> f64 {
    let Some(tri) = hit.tri else {
        return f64::from(hit.y) * 1000.0;
    };
    let v = tris[&(tri.layer, tri.triangle)].map(|p| p.map(|c| c as f64));
    let (x, z) = (f64::from(x) * 1000.0, f64::from(z) * 1000.0);
    // The plane through the three vertices: n · (p - v0) = 0 solved for y.
    let u = [v[1][0] - v[0][0], v[1][1] - v[0][1], v[1][2] - v[0][2]];
    let w = [v[2][0] - v[0][0], v[2][1] - v[0][1], v[2][2] - v[0][2]];
    let n = [
        u[1] * w[2] - u[2] * w[1],
        u[2] * w[0] - u[0] * w[2],
        u[0] * w[1] - u[1] * w[0],
    ];
    if n[1] == 0.0 {
        return f64::from(hit.y) * 1000.0;
    }
    v[0][1] - (n[0] * (x - v[0][0]) + n[2] * (z - v[0][2])) / n[1]
}

/// One native probe: the floor under a body hovering `hover` over the station's floor, whether
/// that floor stands a wall over the body (W-05's wall rule), the wall push of a disc of radius
/// `r` in DO27's confine band `[floor + 0.5, body + 2.0]`, the lid over the floor as the query
/// answers it (`f32`), and the lid's clearance over the floor measured on the cooked
/// integer-millimetre triangles at the same point (metres; see `native_sweep_bars_switchbacks_and_cinder`).
fn native_probe(
    geom: &WorldGeom,
    state: &GeomState,
    tris: &MmTriangles,
    at: (f32, f32),
    hint: f32,
    hover: f32,
    r: f32,
) -> (f32, f32, bool, Option<(f32, f64)>) {
    let (x, z) = at;
    let body = hint + hover;
    let floor = geom.support(state, x, z, hint + det_max(hover, REACH_M)).floor;
    let floor_y = floor.y;
    let wall_rule = floor_y - body > regia_world_geom::compat::hover_ground::HULL_WALL_M;
    let band = Band {
        lo: floor_y + 0.5,
        hi: det_max(floor_y + hover, floor_y) + 2.0,
    };
    let pushed = geom.push_disc(state, x, z, r, band).xz;
    let push = math::sqrt(dist2(pushed, [x, z]));
    let lid = geom.ceiling_above(state, x, z, floor_y).map(|h| {
        let clearance_mm = exact_height_mm(tris, &h, x, z) - exact_height_mm(tris, &floor, x, z);
        (h.y, clearance_mm / 1000.0)
    });
    (floor_y, push, wall_rule, lid)
}

/// W-05's sweep bars on Stage B (plan row W-29): every hull radius at every station of every
/// passage of the Switchbacks (canyon floor) and The Cinder Steps (W-05's +36 m shield) is not
/// pushed by the walls (0 blocked probes, push 0.000 m), stands no wall over its body, and has a
/// lid at least 10 m over its floor; the pilot disc is pushed nowhere across the floor.
#[test]
fn native_sweep_bars_switchbacks_and_cinder() {
    let mut failures: Vec<String> = Vec::new();
    for (map, counts) in [("switchbacks", 8192u16), ("cinder_steps", 17536)] {
        let (bytes, _, sampled) = cook_fixture(map, counts, "", &fixture_fill(map, counts, ""));
        let geom = WorldGeom::from_canonical_bytes(&bytes).unwrap();
        let state = geom.new_state();
        let tris = mm_triangles(&geom);
        for passage in &sampled.passages {
            let st = stations(passage);
            for &r in &HULL_RADII {
                let (mut probes, mut blocked, mut worst, mut min_lid) =
                    (0usize, 0usize, 0.0f32, f32::INFINITY);
                let mut min_lid_mm_grid = f64::INFINITY;
                let mut worst_at = String::new();
                let mut first_block = String::new();
                for s in &st {
                    if by_a_cap(passage, s.s, r) {
                        continue;
                    }
                    let (lo, hi) = lane(passage, s, r);
                    for o in offsets(lo, hi) {
                        let (x, z) = (s.p[0] + s.left[0] * o, s.p[2] + s.left[1] * o);
                        let (floor_y, push, wall_rule, lid) =
                            native_probe(&geom, &state, &tris, (x, z), s.p[1], HULL_RIDE_M, r);
                        probes += 1;
                        if let Some((lid, clearance)) = lid {
                            min_lid = det_min(min_lid, lid - floor_y);
                            if clearance < min_lid_mm_grid {
                                min_lid_mm_grid = clearance;
                            }
                        }
                        let low_lid =
                            lid.is_some_and(|(lid, _)| lid - floor_y < HULL_RIDE_M + UNIT_COLLISION_RADIUS_M);
                        if push > worst {
                            worst = push;
                            worst_at =
                                format!("s {:.1} offset {o:+.2} at ({x:.1}, {floor_y:.2}, {z:.1})", s.s);
                        }
                        if push > PUSH_TOL_M || wall_rule || low_lid {
                            if blocked == 0 {
                                first_block = format!(
                                    "s {:.1} offset {o:+.2} at ({x:.1}, {floor_y:.3}, {z:.1}) over station floor {:.3}: push {push:.4}{}{}",
                                    s.s,
                                    s.p[1],
                                    if wall_rule {
                                        ", the floor is a wall over the body"
                                    } else {
                                        ""
                                    },
                                    if low_lid { ", lid too low" } else { "" }
                                );
                            }
                            blocked += 1;
                        }
                    }
                }
                println!(
                    "NATIVE_SWEEP {map}/{} hull r {r}: {} stations, {probes} probes, {blocked} blocked, worst push {worst:.4} m ({worst_at}), min lid {min_lid:.6} m (query f32) {min_lid_mm_grid:.9} m (cooked mm); first blocked {first_block}",
                    passage.id,
                    st.len()
                );
                if blocked > 0 || worst >= 0.0005 {
                    failures.push(format!("{map}/{} r {r}: {blocked} blocked (first {first_block}), worst push {worst:.4} m at {worst_at}", passage.id));
                }
                // W-05's lid bar holds where W-05 had it: on the covered passages (a lid at all). It is
                // measured on the cooked geometry (integer millimetres, the planes evaluated in f64 at the
                // probe), not as the difference of two f32 query heights: on a sloped passage those carry
                // about one f32 ulp each (3.8e-6 m at 40 m), so a lid exactly 10 000 mm over its floor read
                // 9.999999 m on the Cinder Steps pinch (ADR-8898). The query's own figure is printed beside it.
                if min_lid_mm_grid.is_finite() && min_lid_mm_grid < 10.0 {
                    failures.push(format!(
                        "{map}/{} r {r}: min lid {min_lid_mm_grid:.9} m (cooked mm; query f32 {min_lid:.6} m)",
                        passage.id
                    ));
                }
            }
            let reach = PILOT_RADIUS_M;
            let mut pilot_blocked_full = 0usize;
            for s in &st {
                if by_a_cap(passage, s.s, reach) {
                    continue;
                }
                let (lo, hi) = lane(passage, s, reach);
                let clear = offsets(lo, hi)
                    .into_iter()
                    .filter(|o| {
                        let (x, z) = (s.p[0] + s.left[0] * o, s.p[2] + s.left[1] * o);
                        let (_, push, wall_rule, _) =
                            native_probe(&geom, &state, &tris, (x, z), s.p[1], 0.0, PILOT_RADIUS_M);
                        push <= PUSH_TOL_M && !wall_rule
                    })
                    .count();
                if clear == 0 {
                    pilot_blocked_full += 1;
                }
            }
            println!(
                "NATIVE_SWEEP {map}/{} pilot: {pilot_blocked_full} stations blocked across the floor",
                passage.id
            );
            if pilot_blocked_full > 0 {
                failures.push(format!(
                    "{map}/{} pilot: {pilot_blocked_full} stations blocked across the floor",
                    passage.id
                ));
            }
        }
    }
    let shown: Vec<&String> = failures.iter().take(60).collect();
    assert!(
        failures.is_empty(),
        "{} native sweep failures (first {}):\n{}",
        failures.len(),
        shown.len(),
        shown.iter().map(|f| f.as_str()).collect::<Vec<_>>().join("\n")
    );
}
