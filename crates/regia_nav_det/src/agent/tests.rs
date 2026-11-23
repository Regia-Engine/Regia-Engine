// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pathing/tests.rs, commit c9513623f, tag regia-absorb-4), ADR-8012.
// Lines 284-583 (the agent proofs: round a cliff, round a building, the choke, ten hulls through a gap, head-on
// avoidance, a structure on the route, two runs give the same hash, a cloned state, the tick budget, turned-off
// pathing, large-hull clearance, group slots) and 1239-1427 (the pit and reachability proofs), mark: as-is
// for every assertion. Adapt (named issue: the game state is not in Regia, ADR-8405): the fixtures `world_with`,
// `hull`, `building`, `step`, `pos`, `d2`, `pit_world`, `pit_step` and `ground_pass` drive a small test world that
// implements `DetAgentWorld` and `DetMover` instead of DO27's game state, with Regia's own orders (go to, patrol,
// group slots), disc separation and position hash; the unit and map names are neutral. The terrain fixtures at
// lines 13-42 come as S-02 ported them (`search/tests.rs`). Not ported: `the_bore_twelve_shaft_...` (it reads a
// game map file) and the tunnel tests (W-04). The crowd proofs live in `det/crowd.rs` (S-05).

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_detsim::float::det_clamp;
use regia_detsim::math;

use super::*;
use crate::grid::{GridGround, GridSettings, HeightCells, NavStatic, Profile};
use crate::search::{self, PATH_TICK_BUDGET, Plan, PlanKind};

const SIDE: usize = 256;
const WIDTH: f32 = 1280.0;
const BASE: u16 = 16384;
const M_PER_COUNT: f32 = 1.0 / 32.0;

/// The source's order park distance (m).
const GOTO_PARK_M: f32 = 8.0;
/// The source's group slot spacing and slot park distance (m).
const GROUP_SLOT_GAP_M: f32 = 11.0;
const GROUP_SLOT_PARK_M: f32 = 2.5;
/// The source's battle hull radius and default building radius (m).
const HULL_RADIUS_M: f32 = 3.9;
const SILO_RADIUS_M: f32 = 9.0;

// ------------------------------------------------------------------ terrain fixtures (13-42)

/// Heightfield from a function of world (x, z) in meters, 5 m cells, quantized to 1/32 m as the
/// source fixture stores it.
fn terrain(height: impl Fn(f32, f32) -> f32) -> HeightCells {
    let spacing = WIDTH / SIDE as f32;
    let half = WIDTH * 0.5;
    let mut heights = Vec::with_capacity(SIDE * SIDE);
    for z in 0..SIDE {
        for x in 0..SIDE {
            let wx = x as f32 * spacing - half;
            let wz = z as f32 * spacing - half;
            let h = det_clamp(height(wx, wz), -400.0, 400.0);
            let raw = (BASE as f32 + h * 32.0).round() as u16;
            heights.push(raw as f32 * M_PER_COUNT);
        }
    }
    let mut cells = HeightCells::centered(SIDE, SIDE, spacing, heights);
    // Absolute heights: 0 m is BASE counts.
    cells.center_height = BASE as f32 * M_PER_COUNT;
    cells
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

// ------------------------------------------------------------------ the test world

#[derive(Debug, Clone)]
struct Body {
    pos: [f32; 3],
    radius: f32,
    team: u8,
    building: bool,
    cruise: f32,
    yaw: f32,
}

/// The bodies and the ground: implements the agent layer's world seam.
#[derive(Debug, Clone, Default)]
struct Bodies {
    map: BTreeMap<u32, Body>,
    ground: Option<Arc<HeightCells>>,
}

impl DetAgentWorld for Bodies {
    fn ids(&self) -> Vec<u32> {
        self.map.keys().copied().collect()
    }

    fn body(&self, id: u32) -> Option<DetBody> {
        let b = self.map.get(&id)?;
        Some(DetBody {
            pos: b.pos,
            radius: b.radius,
            profile: if b.building { None } else { Some(Profile::HOVER) },
            team: b.team,
            vel: [0.0, 0.0],
            yaw: b.yaw,
            cruise_m_s: b.cruise,
            soft: false,
        })
    }

    fn obstacle(&self, id: u32) -> Option<Stamp> {
        let b = self.map.get(&id)?;
        b.building
            .then(|| Stamp::from_world(b.pos[0], b.pos[2], b.radius))
    }

    fn ground_y(&self, x: f32, z: f32) -> Option<f32> {
        self.ground.as_ref().map(|g| g.height_at_world(x, z))
    }
}

impl DetAgentWorldMut for Bodies {
    fn set_position(&mut self, id: u32, pos: [f32; 3]) {
        if let Some(b) = self.map.get_mut(&id) {
            b.pos = pos;
        }
    }

    fn set_yaw(&mut self, id: u32, yaw: f32) {
        if let Some(b) = self.map.get_mut(&id) {
            b.yaw = yaw;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Order {
    GoTo {
        x: f32,
        z: f32,
    },
    Patrol {
        ax: f32,
        az: f32,
        bx: f32,
        bz: f32,
        toward_b: bool,
    },
}

#[derive(Debug, Clone)]
struct Job {
    entity: u32,
    order: Order,
    group_n: usize,
    group_k: usize,
}

#[derive(Debug, Clone, Default)]
struct World {
    bodies: Bodies,
    agents: DetAgents,
    jobs: Vec<Job>,
    tick: u64,
}

impl World {
    fn issue_unit_order(&mut self, _team: u8, ids: &[u32], order: Order) -> usize {
        let live: Vec<u32> = ids
            .iter()
            .copied()
            .filter(|id| self.bodies.map.get(id).is_some_and(|b| !b.building))
            .collect();
        self.jobs.retain(|j| !live.contains(&j.entity));
        for (k, id) in live.iter().enumerate() {
            self.jobs.push(Job {
                entity: *id,
                order,
                group_n: live.len(),
                group_k: k,
            });
        }
        self.jobs.sort_by_key(|j| j.entity);
        live.len()
    }

    /// A group member's own slot: rows behind the order point, `GROUP_SLOT_GAP_M` apart (see
    /// `group_slot`).
    fn group_slot(&self, job: &Job, _order: &Order, anchor: [f32; 3], park: f32) -> ([f32; 3], f32) {
        group_slot(job, anchor, park)
    }

    fn nav_prepare(&mut self) {
        self.agents.prepare(&self.bodies);
    }

    fn nav_steer(&mut self, id: u32, goal: [f32; 3], park: f32) -> NavSteer {
        self.agents.steer(id, goal, park, self.tick, &self.bodies)
    }

    /// The source's producer reachability check: an unknown mover or a missing grid is trusted.
    fn reachable(&mut self, id: u32, goal: [f32; 3]) -> bool {
        let Some(b) = self.bodies.body(id) else {
            return true;
        };
        let profile = b.profile.unwrap_or(Profile::HOVER);
        self.agents
            .route_reaches(profile, [b.pos[0], b.pos[2]], [goal[0], goal[2]])
            .unwrap_or(true)
    }
}

/// The test world's group slots (the source assigns its crowd slots by position; this world keeps
/// the group's shape instead): up to six members stand in one line across the order point, a
/// larger group in two rows, members in id order, so a group marching in formation never crosses
/// its own paths.
fn group_slot(job: &Job, anchor: [f32; 3], park: f32) -> ([f32; 3], f32) {
    if job.group_n <= 1 {
        return (anchor, park);
    }
    let cols = if job.group_n <= 6 {
        job.group_n
    } else {
        job.group_n.div_ceil(2)
    };
    let row = (job.group_k / cols) as f32;
    let col = (job.group_k % cols) as f32 - (cols - 1) as f32 * 0.5;
    (
        [
            anchor[0] - row * GROUP_SLOT_GAP_M,
            anchor[1],
            anchor[2] + col * GROUP_SLOT_GAP_M,
        ],
        GROUP_SLOT_PARK_M,
    )
}

/// Every order's mover call for this tick, in entity order, through `mover`.
fn advance_unit_orders(jobs: &mut [Job], mover: &mut dyn DetMover) {
    for job in jobs.iter_mut() {
        match job.order {
            Order::GoTo { x, z } => {
                let (goal, park) = group_slot(job, [x, 0.0, z], GOTO_PARK_M);
                mover.move_kinematic(job.entity, goal, park);
            }
            Order::Patrol {
                ax,
                az,
                bx,
                bz,
                toward_b,
            } => {
                let point = if toward_b { [bx, 0.0, bz] } else { [ax, 0.0, az] };
                let (goal, park) = group_slot(job, point, GOTO_PARK_M);
                if mover.move_kinematic(job.entity, goal, park) {
                    job.order = Order::Patrol {
                        ax,
                        az,
                        bx,
                        bz,
                        toward_b: !toward_b,
                    };
                }
            }
        }
    }
}

/// Hull against hull: both give half the overlap. Hull against building: the hull gives all of it.
fn resolve_entity_collisions(bodies: &mut Bodies) {
    let ids: Vec<u32> = bodies.map.keys().copied().collect();
    for (i, a) in ids.iter().enumerate() {
        for b in &ids[i + 1..] {
            let (pa, ra, sa) = {
                let x = &bodies.map[a];
                (x.pos, x.radius, x.building)
            };
            let (pb, rb, sb) = {
                let x = &bodies.map[b];
                (x.pos, x.radius, x.building)
            };
            if sa && sb {
                continue;
            }
            let dx = pb[0] - pa[0];
            let dz = pb[2] - pa[2];
            let d = math::sqrt(dx * dx + dz * dz);
            let need = ra + rb;
            if d >= need {
                continue;
            }
            let (nx, nz) = if d > 1e-4 { (dx / d, dz / d) } else { (1.0, 0.0) };
            let over = need - d;
            let (wa, wb) = match (sa, sb) {
                (true, _) => (0.0, 1.0),
                (_, true) => (1.0, 0.0),
                _ => (0.5, 0.5),
            };
            if let Some(x) = bodies.map.get_mut(a) {
                x.pos[0] -= nx * over * wa;
                x.pos[2] -= nz * over * wa;
            }
            if let Some(x) = bodies.map.get_mut(b) {
                x.pos[0] += nx * over * wb;
                x.pos[2] += nz * over * wb;
            }
        }
    }
}

fn world_with(ground: Option<HeightCells>) -> World {
    let mut world = World::default();
    let ground = ground.map(Arc::new);
    world.bodies.ground = ground.clone();
    world
        .agents
        .set_ground(ground.map(|g| g as Arc<dyn GridGround + Send + Sync>));
    world
}

fn hull(world: &mut World, id: u32, team: u8, _class: &str, at: [f32; 3]) {
    world.bodies.map.insert(
        id,
        Body {
            pos: at,
            radius: HULL_RADIUS_M,
            team,
            building: false,
            cruise: 30.0,
            yaw: 0.0,
        },
    );
}

fn building(world: &mut World, id: u32, at: [f32; 3]) {
    world.bodies.map.insert(
        id,
        Body {
            pos: at,
            radius: SILO_RADIUS_M,
            team: 0,
            building: true,
            cruise: 0.0,
            yaw: 0.0,
        },
    );
}

/// One sim step of the parts the proofs need: pathing upkeep, the unit orders (kinematic movers),
/// hull-vs-hull and hull-vs-building pushes, then the wedge slide, which has to land after
/// separation or the push puts the hull straight back.
fn step(world: &mut World) {
    let mut kinematic = |agents: &mut DetAgents, bodies: &mut Bodies, tick: u64, jobs: &mut [Job]| {
        let mut mover = KinematicMover::new(agents, bodies, tick);
        advance_unit_orders(jobs, &mut mover);
    };
    step_with(world, &mut kinematic);
}

type MoverRun<'a> = dyn FnMut(&mut DetAgents, &mut Bodies, u64, &mut [Job]) + 'a;

fn step_with(world: &mut World, run: &mut MoverRun<'_>) {
    world.tick += 1;
    world.agents.service(world.tick, &world.bodies);
    run(&mut world.agents, &mut world.bodies, world.tick, &mut world.jobs);
    resolve_entity_collisions(&mut world.bodies);
    world.agents.commit_escapes(world.tick, &mut world.bodies);
}

fn pos(world: &World, id: u32) -> [f32; 3] {
    world.bodies.map[&id].pos
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

/// FNV-1a over every body's id, position and yaw bits, in id order.
fn position_hash(world: &World) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
    };
    for (id, b) in &world.bodies.map {
        eat(&id.to_le_bytes());
        for v in b.pos {
            eat(&v.to_bits().to_le_bytes());
        }
        eat(&b.yaw.to_bits().to_le_bytes());
    }
    h
}

// ------------------------------------------------------------------ the trait seam (ADR-8405)

/// A mover that records every call and drives through the default kinematic mover.
struct Recording<'a> {
    inner: KinematicMover<'a, Bodies>,
    calls: &'a mut Vec<(u32, [f32; 3], f32)>,
    drive: bool,
}

impl DetMover for Recording<'_> {
    fn move_kinematic(&mut self, id: u32, goal: [f32; 3], park: f32) -> bool {
        self.calls.push((id, goal, park));
        if self.drive {
            self.inner.move_kinematic(id, goal, park)
        } else {
            // Steer only: the agent layer runs, nothing moves.
            self.inner
                .agents
                .steer(id, goal, park, self.inner.tick, &*self.inner.world)
                .arrived
        }
    }
}

#[test]
fn agents_follow_through_a_custom_mover() {
    let mut state = world_with(Some(wall(150.0, None)));
    hull(&mut state, 10, 1, "hull", [-80.0, 0.0, 0.0]);
    assert_eq!(
        state.issue_unit_order(1, &[10], Order::GoTo { x: 80.0, z: 0.0 }),
        1
    );
    let mut calls = Vec::new();
    for _ in 0..60 * 40 {
        let mut run = |agents: &mut DetAgents, bodies: &mut Bodies, tick: u64, jobs: &mut [Job]| {
            let mut mover = Recording {
                inner: KinematicMover::new(agents, bodies, tick),
                calls: &mut calls,
                drive: true,
            };
            advance_unit_orders(jobs, &mut mover);
        };
        step_with(&mut state, &mut run);
        if d2(pos(&state, 10), [80.0, 0.0, 0.0]) <= GOTO_PARK_M + 0.5 {
            break;
        }
    }
    assert!(
        d2(pos(&state, 10), [80.0, 0.0, 0.0]) <= GOTO_PARK_M + 0.5,
        "arrived through the custom mover, at {:?}",
        pos(&state, 10)
    );
    assert!(
        calls.len() > 60,
        "every tick went through the mover: {} calls",
        calls.len()
    );
    assert!(calls.iter().all(|c| c.0 == 10 && c.2 == GOTO_PARK_M));
    assert!(
        state.agents.stats.routed_calls > 0,
        "the route came from the agent layer"
    );

    // A mover that only steers: the agent layer never moves a body itself, so the hull stays put
    // until the wedge recovery (the one write the layer owns, `commit_escapes`) slides it.
    let mut still = world_with(Some(wall(150.0, None)));
    hull(&mut still, 10, 1, "hull", [-80.0, 0.0, 0.0]);
    still.issue_unit_order(1, &[10], Order::GoTo { x: 80.0, z: 0.0 });
    let mut calls = Vec::new();
    for _ in 0..60 {
        let mut run = |agents: &mut DetAgents, bodies: &mut Bodies, tick: u64, jobs: &mut [Job]| {
            let mut mover = Recording {
                inner: KinematicMover::new(agents, bodies, tick),
                calls: &mut calls,
                drive: false,
            };
            advance_unit_orders(jobs, &mut mover);
        };
        step_with(&mut still, &mut run);
    }
    assert_eq!(pos(&still, 10), [-80.0, 0.0, 0.0], "only the mover moves bodies");
    assert_eq!(calls.len(), 60);
    for _ in 0..60 * 3 {
        let mut run = |agents: &mut DetAgents, bodies: &mut Bodies, tick: u64, jobs: &mut [Job]| {
            let mut mover = Recording {
                inner: KinematicMover::new(agents, bodies, tick),
                calls: &mut calls,
                drive: false,
            };
            advance_unit_orders(jobs, &mut mover);
        };
        step_with(&mut still, &mut run);
    }
    assert!(
        still.agents.stats.escape_attempts > 0,
        "a body that never moves is a wedge"
    );
}

#[test]
fn the_report_lists_counters_and_waypoints() {
    let mut state = world_with(Some(wall(300.0, Some((0.0, 60.0)))));
    hull(&mut state, 10, 1, "hull", [-200.0, 0.0, 0.0]);
    hull(&mut state, 11, 1, "hull", [-200.0, 0.0, 30.0]);
    state.issue_unit_order(1, &[10, 11], Order::GoTo { x: 200.0, z: 0.0 });
    for _ in 0..30 {
        step(&mut state);
    }
    let report = state.agents.report();
    assert_eq!(report.agents, 2);
    assert_eq!(report.queue, 0);
    assert!(report.grid.is_some());
    assert_eq!(report.routes.len(), 2);
    assert_eq!(report.routes[0].id, 10, "ascending ids");
    assert!(report.routes[0].waypoints.len() >= 2);
    assert!(report.routes[0].next >= 1);
    assert_eq!(report.stats.requests, state.agents.stats.requests);
    let text = ron::to_string(&report).expect("plain rows");
    assert!(text.contains("waypoints:[("), "{text}");
}

#[test]
fn settings_default_to_the_source_values() {
    let s = DetAgentSettings::default();
    assert_eq!(s.stuck_ticks, STUCK_TICKS);
    assert_eq!(s.stuck_ticks, 150);
    assert_eq!(s.stuck_move_m, 2.5);
    assert_eq!(s.repath_min_ticks, 30);
    assert_eq!(s.agent_idle_ticks, 180);
    assert_eq!(s.give_up_stuck, 4);
    assert_eq!(s.escape_nudge_ticks, 90);
    assert_eq!(s.escape_still_ticks, 180);
    assert_eq!(s.escape_depart_ticks, 45);
    assert_eq!(s.direct_m, 60.0);
    assert_eq!(s.near_goal_m, 10.0);
    assert_eq!(s.off_path_m, 20.0);
    assert_eq!(s.pit_exit_m, 10.0);
    assert_eq!(s.separation_gap_m, 2.0);
    assert_eq!(s.tick_dt, 1.0 / 60.0);
    assert_eq!(s.tick_budget, None);
}

/// A pinned digest of the hash run: the agent layer moves the same bits on every target (the
/// aarch64 leg runs this on the Mac).
#[test]
fn the_hash_run_is_pinned_on_every_arch() {
    let (hash, requests, expansions) = hash_run();
    assert_eq!(
        (hash, requests, expansions),
        PINNED_HASH_RUN,
        "the agent run moved: (0x{hash:016x}, {requests}, {expansions})"
    );
}

/// Re-pinned for ADR-8876 (`MATH_BITS_VERSION` 1, the canonical correctly rounded `regia_detsim::math::atan2`;
/// the hash was `0xb3da_1601_ca11_0068`; requests and expansions did not move).
const PINNED_HASH_RUN: (u64, u64, u64) = (0x405d_e2d6_b71b_43aa, 35, 6506);

// ------------------------------------------------------------------ the source's agent proofs (284-583)

#[test]
fn a_unit_drives_round_a_cliff_to_its_goal() {
    let mut state = world_with(Some(wall(150.0, None)));
    hull(&mut state, 10, 1, "hull", [-80.0, 0.0, 0.0]);
    assert_eq!(
        state.issue_unit_order(1, &[10], Order::GoTo { x: 80.0, z: 0.0 }),
        1
    );
    // Never on the wall itself (15 m thick, 300 m long, 60 m high).
    let grid_check = |_: &World, at: [f32; 3]| !(at[0].abs() <= 7.5 + 1.0 && at[2].abs() <= 150.0 + 1.0);
    let mut travelled = 0.0;
    let mut last = pos(&state, 10);
    for _ in 0..60 * 40 {
        step(&mut state);
        let now = pos(&state, 10);
        travelled += d2(now, last);
        last = now;
        assert!(grid_check(&state, now), "drove onto the cliff at {now:?}");
        if d2(now, [80.0, 0.0, 0.0]) <= GOTO_PARK_M + 0.5 {
            break;
        }
    }
    assert!(
        d2(last, [80.0, 0.0, 0.0]) <= GOTO_PARK_M + 0.5,
        "arrived, at {last:?}"
    );
    assert!(travelled > 300.0, "went round the 300 m wall ({travelled} m)");
    assert_eq!(state.agents.stats.stuck_events, 0);
}

#[test]
fn a_unit_drives_round_a_building_without_touching_it() {
    let mut state = world_with(None);
    building(&mut state, 50, [0.0, 0.0, 0.0]);
    hull(&mut state, 10, 1, "hull", [-60.0, 0.0, 0.0]);
    state.issue_unit_order(1, &[10], Order::GoTo { x: 60.0, z: 0.0 });
    let silo_r = SILO_RADIUS_M;
    let hull_r = HULL_RADIUS_M;
    let mut closest = f32::MAX;
    for _ in 0..60 * 20 {
        step(&mut state);
        let d = d2(pos(&state, 10), [0.0, 0.0, 0.0]);
        if d < closest {
            closest = d;
        }
    }
    let end = pos(&state, 10);
    assert!(
        d2(end, [60.0, 0.0, 0.0]) <= GOTO_PARK_M + 0.5,
        "arrived, at {end:?}"
    );
    assert!(
        closest >= silo_r + hull_r - 0.05,
        "kept off the {silo_r} m silo: closest {closest}"
    );
    assert_eq!(state.agents.stats.stuck_events, 0);
}

#[test]
fn a_unit_finds_the_choke_in_a_long_wall() {
    // 800 m wall with one 30 m pass at z = 150.
    let mut state = world_with(Some(wall(400.0, Some((150.0, 30.0)))));
    hull(&mut state, 10, 1, "hull", [-100.0, 0.0, 0.0]);
    state.issue_unit_order(1, &[10], Order::GoTo { x: 100.0, z: 0.0 });
    let mut crossed_at = None;
    for _ in 0..60 * 40 {
        let before = pos(&state, 10);
        step(&mut state);
        let now = pos(&state, 10);
        if before[0] < 0.0 && now[0] >= 0.0 {
            crossed_at = Some(now[2]);
        }
    }
    let z = crossed_at.expect("crossed the wall line");
    assert!((z - 150.0).abs() <= 15.0, "crossed through the pass, at z {z}");
    assert!(d2(pos(&state, 10), [100.0, 0.0, 0.0]) <= GOTO_PARK_M + 0.5);
}

#[test]
fn ten_hulls_go_through_a_gap_without_wedging() {
    let mut state = world_with(Some(wall(300.0, Some((0.0, 35.0)))));
    let ids: Vec<u32> = (0..10).map(|k| 100 + k).collect();
    for (k, id) in ids.iter().enumerate() {
        let row = (k / 5) as f32;
        let col = (k % 5) as f32;
        hull(
            &mut state,
            *id,
            1,
            "hull",
            [-120.0 - row * 12.0, 0.0, -24.0 + col * 12.0],
        );
    }
    assert_eq!(
        state.issue_unit_order(1, &ids, Order::GoTo { x: 120.0, z: 0.0 }),
        10
    );
    let mut arrived_tick = None;
    for t in 0..60 * 60 {
        step(&mut state);
        let all = ids.iter().all(|id| {
            let job = state
                .jobs
                .iter()
                .find(|j| j.entity == *id)
                .expect("a job")
                .clone();
            let (slot, park) = state.group_slot(&job, &job.order, [120.0, 0.0, 0.0], GOTO_PARK_M);
            d2(pos(&state, *id), slot) <= park + 1.0
        });
        if all {
            arrived_tick = Some(t);
            break;
        }
    }
    let t = arrived_tick.unwrap_or_else(|| {
        let at: Vec<[f32; 3]> = ids.iter().map(|id| pos(&state, *id)).collect();
        panic!(
            "the group did not all arrive: {at:?} stats {:?}",
            state.agents.stats
        )
    });
    assert!(t < 60 * 45, "took {} s", t / 60);
    // Nobody stands inside another hull at the end.
    for (i, a) in ids.iter().enumerate() {
        for b in &ids[i + 1..] {
            let d = d2(pos(&state, *a), pos(&state, *b));
            assert!(d >= 2.0 * HULL_RADIUS_M - 0.5, "{a} and {b} overlap ({d} m)");
        }
    }
    assert!(
        state.agents.stats.stuck_events <= 3,
        "stuck events {}",
        state.agents.stats.stuck_events
    );
}

/// Two hulls meeting head-on give way through the velocity solver instead of wedging nose to nose.
#[test]
fn two_hulls_meeting_head_on_give_way_with_orca() {
    let mut state = world_with(Some(terrain(|_, _| 0.0)));
    hull(&mut state, 10, 1, "hull", [-60.0, 0.0, -3.0]);
    hull(&mut state, 11, 1, "hull", [60.0, 0.0, 3.0]);
    state.issue_unit_order(1, &[10], Order::GoTo { x: 60.0, z: 3.0 });
    state.issue_unit_order(1, &[11], Order::GoTo { x: -60.0, z: -3.0 });
    for _ in 0..60 * 20 {
        step(&mut state);
    }
    assert!(
        state.agents.stats.orca_calls > 0,
        "the solver steered the meeting"
    );
    assert!(
        d2(pos(&state, 10), [60.0, 0.0, 3.0]) < 60.0,
        "hull 10 worked past"
    );
    assert!(
        d2(pos(&state, 11), [-60.0, 0.0, -3.0]) < 60.0,
        "hull 11 worked past"
    );
    assert!(
        state.agents.stats.stuck_events <= 3,
        "stuck events {}",
        state.agents.stats.stuck_events
    );
}

#[test]
fn a_structure_placed_on_the_route_makes_the_unit_re_plan() {
    let mut state = world_with(Some(wall(300.0, Some((0.0, 60.0)))));
    hull(&mut state, 10, 1, "hull", [-200.0, 0.0, 0.0]);
    state.issue_unit_order(1, &[10], Order::GoTo { x: 200.0, z: 0.0 });
    for _ in 0..60 {
        step(&mut state);
    }
    let routes = state.agents.debug_paths();
    assert!(!routes.is_empty(), "the hull is on a route");
    let before = state.agents.stats.repath_obstacle;
    // Two silos close most of the pass, across the route.
    building(&mut state, 60, [0.0, 0.0, -16.0]);
    building(&mut state, 61, [0.0, 0.0, 16.0]);
    let silo_r = SILO_RADIUS_M;
    let mut closest = f32::MAX;
    for _ in 0..60 * 40 {
        step(&mut state);
        let p = pos(&state, 10);
        for c in [d2(p, [0.0, 0.0, -16.0]), d2(p, [0.0, 0.0, 16.0])] {
            if c < closest {
                closest = c;
            }
        }
    }
    assert!(
        state.agents.stats.repath_obstacle > before,
        "the route was re-planned: {:?} routes {:?}",
        state.agents.stats,
        routes
    );
    assert!(
        closest >= silo_r + HULL_RADIUS_M - 0.05,
        "kept off the silos: {closest}"
    );
    assert!(
        d2(pos(&state, 10), [200.0, 0.0, 0.0]) <= GOTO_PARK_M + 0.5,
        "still arrived"
    );
}

fn hash_run() -> (u64, u64, u64) {
    let mut state = world_with(Some(wall(300.0, Some((40.0, 40.0)))));
    building(&mut state, 60, [-60.0, 0.0, 30.0]);
    let ids: Vec<u32> = (0..12).map(|k| 200 + k).collect();
    for (k, id) in ids.iter().enumerate() {
        hull(
            &mut state,
            *id,
            1,
            "hull",
            [-150.0 + (k % 4) as f32 * 9.0, 0.0, -40.0 + (k / 4) as f32 * 11.0],
        );
    }
    state.issue_unit_order(1, &ids[..6], Order::GoTo { x: 150.0, z: 20.0 });
    state.issue_unit_order(
        1,
        &ids[6..],
        Order::Patrol {
            ax: -150.0,
            az: 100.0,
            bx: 120.0,
            bz: -100.0,
            toward_b: true,
        },
    );
    for t in 0..60 * 30 {
        step(&mut state);
        if t == 600 {
            building(&mut state, 61, [0.0, 0.0, 40.0]);
        }
    }
    (
        position_hash(&state),
        state.agents.stats.requests,
        state.agents.stats.fine_expansions,
    )
}

#[test]
fn two_runs_give_the_same_hash() {
    let a = hash_run();
    let b = hash_run();
    assert_eq!(a, b);
    assert!(a.1 > 0, "the run planned routes");
}

#[test]
fn a_cloned_state_keeps_the_same_routes() {
    let mut left = world_with(Some(wall(300.0, Some((40.0, 40.0)))));
    for k in 0..6u32 {
        hull(
            &mut left,
            300 + k,
            1,
            "hull",
            [-150.0, 0.0, -30.0 + k as f32 * 12.0],
        );
    }
    let ids: Vec<u32> = (0..6).map(|k| 300 + k).collect();
    left.issue_unit_order(1, &ids, Order::GoTo { x: 150.0, z: 0.0 });
    for _ in 0..200 {
        step(&mut left);
    }
    let mut right = left.clone();
    for _ in 0..600 {
        step(&mut left);
        step(&mut right);
    }
    assert_eq!(position_hash(&left), position_hash(&right));
}

#[test]
fn the_tick_budget_queues_requests_and_serves_them_in_order() {
    let mut state = world_with(Some(wall(500.0, Some((-300.0, 40.0)))));
    let ids: Vec<u32> = (0..40).map(|k| 400 + k).collect();
    for (k, id) in ids.iter().enumerate() {
        hull(
            &mut state,
            *id,
            1,
            "hull",
            [-300.0, 0.0, -200.0 + k as f32 * 10.0],
        );
        state.issue_unit_order(
            1,
            &[*id],
            Order::GoTo {
                x: 300.0,
                z: 200.0 - k as f32 * 10.0,
            },
        );
    }
    step(&mut state);
    assert!(state.agents.stats.max_tick_work <= PATH_TICK_BUDGET + search::MAX_FINE_EXPANSIONS as i64 * 5);
    for _ in 0..120 {
        step(&mut state);
    }
    assert!(
        state.agents.queue_len() == 0,
        "every request was served: queue {} {:?}",
        state.agents.queue_len(),
        state.agents.stats
    );
    assert!(state.agents.stats.solved >= 40);
}

#[test]
fn turned_off_pathing_steers_straight_like_before() {
    let mut state = world_with(Some(wall(150.0, None)));
    state.agents.disabled = true;
    hull(&mut state, 10, 1, "hull", [-80.0, 0.0, 0.0]);
    state.issue_unit_order(1, &[10], Order::GoTo { x: 80.0, z: 0.0 });
    for _ in 0..30 {
        step(&mut state);
    }
    let p = pos(&state, 10);
    assert!(p[2].abs() < 1e-3 && p[0] > -80.0, "straight at the goal: {p:?}");
}

#[test]
fn a_large_hull_keeps_further_from_buildings_than_a_light_one() {
    let mut state = world_with(None);
    building(&mut state, 50, [0.0, 0.0, 0.0]);
    state.nav_prepare();
    let grid = state.agents.grid().expect("grid");
    let near = grid.cell_at(16.0, 0.0);
    assert!(grid.passable(Profile::FOOT, near));
    assert!(!grid.passable(Profile::HOVER_LARGE, near));
}

#[test]
fn each_movement_class_has_its_own_slope_limit() {
    // A 40 m shelf for x > 10 between z = -300 and 300; its long face rises 2.0 (63 degrees), its ends ramp 0.5.
    let sampler = terrain(|x, z| {
        let face = det_clamp((x + 10.0) / 20.0, 0.0, 1.0);
        let ends = det_clamp((380.0 - z.abs()) / 80.0, 0.0, 1.0);
        40.0 * face * ends
    });
    let grid = NavGrid::new(Arc::new(
        NavStatic::build(Some(&sampler), Arc::new(GridSettings::hover_ground_jet())).expect("ground"),
    ))
    .expect("grid");
    let mut scratch = search::Scratch::default();
    let len = |plan: &Plan| -> f32 { plan.waypoints.windows(2).map(|w| dist2(w[0], w[1])).sum() };
    let from = [-80.0, 0.0];
    let to = [80.0, 0.0];
    let ground = search::plan(&grid, &mut scratch, Profile::FOOT, from, to).expect("ground route");
    let jet = search::plan(&grid, &mut scratch, Profile::JET, from, to).expect("jet route");
    let hover = search::plan(&grid, &mut scratch, Profile::HOVER, from, to).expect("hover route");
    assert_eq!(ground.kind, PlanKind::Full);
    assert!(
        len(&ground) > 600.0,
        "a pilot on foot walks round by the end ramps: {}",
        len(&ground)
    );
    assert!(len(&jet) < 200.0, "a jetpack goes up the face: {}", len(&jet));
    // The face is 63 degrees. Fans lose grip near 38, so a hover hull takes the
    // end ramps with the pilots instead of cutting the shelf.
    assert!(
        len(&hover) > 600.0,
        "a hover hull walks the ramps, not the 63 degree face: {}",
        len(&hover)
    );
    for cell in &ground.cells {
        assert!(grid.passable(Profile::FOOT, *cell));
    }
}

#[test]
fn group_orders_give_every_hull_its_own_slot() {
    let mut state = world_with(None);
    let ids: Vec<u32> = (0..6).map(|k| 500 + k).collect();
    for (k, id) in ids.iter().enumerate() {
        hull(&mut state, *id, 1, "hull", [-100.0, 0.0, -30.0 + k as f32 * 12.0]);
    }
    state.issue_unit_order(1, &ids, Order::GoTo { x: 100.0, z: 0.0 });
    let mut slots: Vec<[f32; 3]> = Vec::new();
    for id in &ids {
        let job = state
            .jobs
            .iter()
            .find(|j| j.entity == *id)
            .expect("a job")
            .clone();
        assert_eq!(job.group_n, 6);
        let (slot, park) = state.group_slot(&job, &job.order, [100.0, 0.0, 0.0], GOTO_PARK_M);
        assert!(park < GOTO_PARK_M);
        for other in &slots {
            assert!(d2(*other, slot) >= GROUP_SLOT_GAP_M - 0.01, "slots overlap");
        }
        slots.push(slot);
    }
    for _ in 0..60 * 60 {
        step(&mut state);
        let parked = ids.iter().all(|id| {
            let job = state
                .jobs
                .iter()
                .find(|j| j.entity == *id)
                .expect("a job")
                .clone();
            let (slot, _) = state.group_slot(&job, &job.order, [100.0, 0.0, 0.0], GOTO_PARK_M);
            d2(pos(&state, *id), slot) <= GROUP_SLOT_PARK_M + 1.0
        });
        if parked {
            break;
        }
    }
    for id in &ids {
        let job = state
            .jobs
            .iter()
            .find(|j| j.entity == *id)
            .expect("a job")
            .clone();
        let (slot, _) = state.group_slot(&job, &job.order, [100.0, 0.0, 0.0], GOTO_PARK_M);
        assert!(
            d2(pos(&state, *id), slot) <= GROUP_SLOT_PARK_M + 1.0,
            "{id} parked on its slot at {:?} (slot {:?})",
            pos(&state, *id),
            slot
        );
    }
}

// ------------------------------------------------------------------ the pit proofs (924-950, 1239-1427)

/// The source's shaft: a 25 m kill cylinder at the origin with its kill plane at -30 m.
fn shaft() -> DetKillVolume {
    DetKillVolume {
        center_x: 0.0,
        center_z: 0.0,
        radius: 25.0,
        kill_y: -30.0,
    }
}

fn pit_terrain() -> HeightCells {
    terrain(|x, z| {
        let r = math::sqrt(x * x + z * z);
        if r >= 30.0 {
            0.0
        } else if r <= 20.0 {
            -100.0
        } else {
            -100.0 + (r - 20.0) * 10.0
        }
    })
}

/// The shaft's kill volume over the synthetic pit, on a stripped world.
fn pit_world() -> World {
    let mut state = world_with(Some(pit_terrain()));
    state.agents.set_kill_volume(Some(shaft()));
    state
}

/// Terrain following: the ground pass carries hulls down the pit wall the way a hover step does,
/// so a route across the mouth would really drop a hull into the shaft.
fn ground_pass(state: &mut World) {
    let Some(ground) = state.bodies.ground.clone() else {
        return;
    };
    for b in state.bodies.map.values_mut() {
        if !b.building {
            b.pos[1] = ground.height_at_world(b.pos[0], b.pos[2]);
        }
    }
}

fn pit_step(state: &mut World) {
    step(state);
    ground_pass(state);
}

#[test]
fn ai_routes_round_a_pit_kill_volume() {
    let mut state = pit_world();
    state.nav_prepare();
    let vol = shaft();
    let grid: NavGrid = state.agents.grid().expect("grid").clone();
    // The graph excludes the interior for every profile: the shaft floor is
    // no-go, the far rim stays open.
    assert!(grid.excluded(grid.cell_at(0.0, 0.0)), "shaft floor is excluded");
    for profile in grid.settings().all_profiles() {
        assert!(
            !grid.passable(profile, grid.cell_at(0.0, 0.0)),
            "{profile:?} cannot stand in the shaft"
        );
    }
    assert!(grid.passable(Profile::HOVER, grid.cell_at(200.0, 0.0)));
    // A cross-map plan goes round, never through a no-go cell.
    let mut scratch = search::Scratch::default();
    let plan =
        search::plan(&grid, &mut scratch, Profile::HOVER, [-200.0, 0.0], [200.0, 0.0]).expect("a route");
    assert_ne!(plan.kind, PlanKind::Partial, "the far rim is reachable");
    for cell in &plan.cells {
        assert!(grid.passable(Profile::HOVER, *cell), "route enters no-go ground");
    }
    for wp in &plan.waypoints {
        let h = grid.surface_y(wp[0], wp[1]);
        assert!(
            !vol.claims(wp[0], wp[1], h),
            "waypoint {wp:?} aims into the shaft"
        );
    }
    // Dense cells: crossing x = 0 happens outside the shaft radius.
    assert!(
        plan.cells.iter().any(|c| {
            let p = grid.center(*c);
            p[0].abs() < 5.0 && p[1].abs() > vol.radius
        }),
        "the route crosses the middle outside the shaft"
    );
    let len: f32 = plan.waypoints.windows(2).map(|w| dist2(w[0], w[1])).sum();
    assert!(len > 400.0, "round the pit, not through it: {len}");
    // Deterministic: planning twice plans the same route.
    let mut scratch2 = search::Scratch::default();
    let again =
        search::plan(&grid, &mut scratch2, Profile::HOVER, [-200.0, 0.0], [200.0, 0.0]).expect("a route");
    assert_eq!(plan.waypoints, again.waypoints);
}

#[test]
fn a_unit_at_the_lip_steers_away_from_the_pit() {
    let mut state = pit_world();
    hull(&mut state, 10, 1, "hull", [45.0, 0.0, 0.0]);
    pit_step(&mut state);
    let vol = shaft();
    // Deterministic: two identical worlds steer the same point.
    let mut twin = pit_world();
    hull(&mut twin, 10, 1, "hull", [45.0, 0.0, 0.0]);
    pit_step(&mut twin);
    let goal = [-200.0, 0.0, 0.0];
    let first = state.nav_steer(10, goal, GOTO_PARK_M);
    let same = twin.nav_steer(10, goal, GOTO_PARK_M);
    assert_eq!(first.point, same.point, "steering is deterministic");
    // The look-ahead already refuses the mouth: neither the steer point nor
    // the ground halfway there is no-go ground.
    let grid: NavGrid = state.agents.grid().expect("grid").clone();
    for p in [
        first.point,
        [
            (first.point[0] + 45.0) * 0.5,
            first.point[1],
            first.point[2] * 0.5,
        ],
    ] {
        let h = grid.surface_y(p[0], p[2]);
        assert!(!vol.claims(p[0], p[2], h), "steer aims into the shaft at {p:?}");
    }
    // Then drive it across: round the pit, never inside, then arrived.
    state.issue_unit_order(1, &[10], Order::GoTo { x: -200.0, z: 0.0 });
    let mut arrived = false;
    for _ in 0..60 * 120 {
        pit_step(&mut state);
        let p = pos(&state, 10);
        assert!(!vol.claims(p[0], p[2], p[1]), "drove into the shaft at {p:?}");
        if d2(p, [-200.0, 0.0, 0.0]) <= GOTO_PARK_M + 0.5 {
            arrived = true;
            break;
        }
    }
    assert!(arrived, "went round the pit, at {:?}", pos(&state, 10));
    assert_eq!(state.agents.stats.stuck_events, 0);
}

#[test]
fn a_unit_sliding_into_the_pit_climbs_back_out() {
    let mut state = pit_world();
    // Mid-shaft on the wall side, below the margin plane, ordered out.
    hull(&mut state, 10, 1, "hull", [10.0, -40.0, 0.0]);
    state.issue_unit_order(1, &[10], Order::GoTo { x: 200.0, z: 0.0 });
    let vol = shaft();
    // The steer forgets the goal and aims at the rim exit.
    let steer = state.nav_steer(10, [200.0, 0.0, 0.0], GOTO_PARK_M);
    assert!(!steer.arrived);
    let exit_r = math::sqrt(steer.point[0] * steer.point[0] + steer.point[2] * steer.point[2]);
    assert!(
        exit_r >= vol.radius + PIT_EXIT_M - 0.01,
        "climbs to the rim, aims at {exit_r} m"
    );
    assert!(state.agents.stats.pit_climbs > 0);
    // Drive: the radial distance only grows until it is out of the shaft.
    let mut out = false;
    let mut min_r = f32::MAX;
    for _ in 0..60 * 30 {
        step(&mut state);
        let p = pos(&state, 10);
        let r = math::sqrt(p[0] * p[0] + p[2] * p[2]);
        if r < min_r {
            min_r = r;
        }
        if r > vol.radius {
            out = true;
            break;
        }
    }
    assert!(out, "climbed out of the shaft");
    assert!(min_r >= 9.0, "never slid deeper, closest {min_r} m");
}

#[test]
fn no_ai_unit_enters_the_shaft_in_a_ten_minute_pit_crossing_fixture() {
    // Six AI hulls, three a side, march across the pit mouth and back for ten
    // minutes (60 Hz). Terrain following carries them down the wall, so
    // a route across the mouth would really drop hulls into the shaft.
    let mut state = pit_world();
    let ids = [10u32, 11, 12, 20, 21, 22];
    let lanes = [-100.0, -60.0, -20.0, 20.0, 60.0, 100.0];
    for (k, id) in ids.iter().enumerate() {
        let team = if *id < 20 { 1 } else { 2 };
        let x = if team == 1 { -300.0 } else { 300.0 };
        hull(&mut state, *id, team, "hull", [x, 0.0, lanes[k]]);
    }
    let vol = shaft();
    // Each hull walks its lane to the far side and back; the side flips on arrival.
    let mut side = [1.0f32, 1.0, 1.0, -1.0, -1.0, -1.0];
    let mut flips = [0u32; 6];
    for (k, id) in ids.iter().enumerate() {
        let team = if *id < 20 { 1 } else { 2 };
        state.issue_unit_order(
            team,
            &[*id],
            Order::GoTo {
                x: side[k] * 300.0,
                z: lanes[k],
            },
        );
    }
    for _ in 0..60 * 600 {
        pit_step(&mut state);
        for (k, id) in ids.iter().enumerate() {
            let p = pos(&state, *id);
            assert!(
                !vol.claims(p[0], p[2], p[1]),
                "hull {id} entered the shaft at {p:?}"
            );
            if d2(p, [side[k] * 300.0, 0.0, lanes[k]]) <= GOTO_PARK_M + 2.0 {
                side[k] = -side[k];
                flips[k] += 1;
                let team = if *id < 20 { 1 } else { 2 };
                state.issue_unit_order(
                    team,
                    &[*id],
                    Order::GoTo {
                        x: side[k] * 300.0,
                        z: lanes[k],
                    },
                );
            }
        }
    }
    // Every hull crossed several times: they travelled, not sat still.
    for (k, id) in ids.iter().enumerate() {
        assert!(flips[k] >= 2, "hull {id} crossed only {} times", flips[k]);
    }
}

#[test]
fn a_goal_behind_a_cliff_ring_is_not_reachable() {
    // A cliff ring round the pad at the origin.
    let ring = terrain(|x, z| {
        let r = math::sqrt(x * x + z * z);
        if (40.0..=55.0).contains(&r) { 80.0 } else { 0.0 }
    });
    let mut state = world_with(Some(ring));
    hull(&mut state, 10, 1, "producer", [-300.0, 0.0, 0.0]);
    state.nav_prepare();
    assert!(!state.reachable(10, [0.0, 0.0, 0.0]), "inside the ring");
    assert!(state.reachable(10, [-200.0, 0.0, 100.0]), "open ground");
    assert!(
        state.reachable(99, [0.0, 0.0, 0.0]),
        "an unknown producer is not second-guessed"
    );
    let mut bare = world_with(None);
    hull(&mut bare, 10, 1, "producer", [-300.0, 0.0, 0.0]);
    assert!(
        bare.reachable(10, [0.0, 0.0, 0.0]),
        "no nav grid: the order stands"
    );
}

#[test]
fn a_goal_on_a_pit_floor_is_not_reachable_even_when_the_goal_snaps_to_the_rim() {
    // A 60 m deep round pit: its floor cells are not driveable, so the search snaps the goal out to the rim.
    let pit = terrain(|x, z| {
        if math::sqrt(x * x + z * z) <= 45.0 {
            -60.0
        } else {
            0.0
        }
    });
    let mut state = world_with(Some(pit));
    hull(&mut state, 10, 1, "producer", [-300.0, 0.0, 0.0]);
    state.nav_prepare();
    assert!(!state.reachable(10, [0.0, -60.0, 0.0]), "pit floor");
    assert!(state.reachable(10, [-150.0, 0.0, 80.0]), "open ground");
}

#[test]
fn the_kill_volume_claims_inside_the_radius_below_the_margin_plane() {
    let vol = shaft();
    assert!(vol.claims(0.0, 0.0, -26.0));
    assert!(!vol.claims(0.0, 0.0, -25.0), "the margin plane itself is open");
    assert!(!vol.claims(26.0, 0.0, -90.0), "outside the radius");
    assert_eq!(vol.rim_exit([10.0, 0.0], PIT_EXIT_M), [35.0, 0.0]);
    assert_eq!(
        vol.rim_exit([0.0, 0.0], PIT_EXIT_M),
        [35.0, 0.0],
        "the exact center exits on +X"
    );
    let disc = vol.exclusion();
    assert_eq!(disc.below_y, -25.0);
    assert_eq!(disc.radius, 25.0);
}
