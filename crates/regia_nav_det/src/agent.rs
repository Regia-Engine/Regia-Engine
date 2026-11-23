// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pathing/mod.rs, commit c9513623f, tag regia-absorb-4), ADR-8012.
// Lines 72-131 (the agent constants), 134-182 (`NavSteer`), 221-323 (`PathStats`), 325-407 (`Agent`), 421-456
// (the neighbour hash), 522-568 (helpers), 606-664 (`SteerInput`, `route_reaches`, `debug_paths`, counts),
// 714-765 (`crowd_refresh`, `crowd_slot`, `crowd_bent_slot`), 832-1361
// (`ensure_grid`, `sync_obstacles`, `ensure_neighbours`, the FIFO queue, `pump`, `service`, `goal_ring`,
// `clear_line`), 1362-2511 (`steer`, `steer_inner`, `pit_refusal`, `slot_taken`, `traffic_ahead`, `escape_cell`,
// `escape_step`, `escape_reserved`, `clearance_cm`, `cell_clear`, `ridge_hide`, `avoidance`, the hold and back-off
// helpers, `follow`) and 2513-3149 (`nav_steer`, `nav_line_clear`, `nav_move_kinematic`, `kinematic_speed`,
// `nav_prepare`, `nav_commit_escapes`, `nav_service`, `nav_note_moved`). Mark: adapt, for these named issues
// (ADR-8405). ir-sim/src/state.rs lines 20-66 (the kill volume) were read as requirements only: `DetKillVolume`
// is written here from that behavior (a radius test below the kill plane plus a 5 m margin, a radial rim exit).
// 1. The game state is not in Regia: `GameplayRuntimeState`/`RuntimeEntity` reads become the `DetAgentWorld`
//    trait (ascending ids, position, radius, movement profile, team, `obstacle()`), the writes `DetAgentWorldMut`.
// 2. `nav_move_kinematic` becomes the `DetMover` trait; the source's kinematic step is `KinematicMover`, the
//    default implementation.
// 3. `std::time::Instant` leaves the step path: the wall-clock fields of `PathStats` are dropped (a host times
//    the calls itself); `cargo xtask det-scan` holds the module to the lockstep rules.
// 4. Float `min`/`max`/`clamp` become `regia_detsim::float::det_*` (ADR-8085); transcendentals come from
//    `regia_detsim::math`; ORCA is `regia_detsim::avoid`; terrain comes through S-01's grid (`GridGround`).
// 5. The source's caps become `DetAgentSettings` fields whose defaults are the source's values.
// 6. The crowd methods (lines 714-765) are wired here (plan row S-05, ADR-8448). Still left out (requirements
//    only, see docs/do27-port-review.md and the S-03 report): the tunnel layer and crawlway setters (W-04,
//    W-20), the navmesh arm (S-06), the hover drive requests, the hover escape drive, jetpack steering, and
//    the hull-name profile and mesh-agent tables.

//! Deterministic agents: path following, stuck recovery and re-planning for lockstep movers
//! (plan row S-03, ADR-8405).
//!
//! [`DetAgents`] keeps one agent per steered mover: its route from [`crate::search`], a
//! look-ahead point with corner slow-down, the stuck clocks (no progress, back-off, a bounded
//! wedge slide to a clear cell, give-up counting), re-plan triggers (goal moved, a stamp changed
//! a cluster on the route, off the route, stuck), the kill-volume refusal, and local avoidance
//! (contact separation plus [`regia_detsim::avoid`]). Requests wait in a FIFO queue and are
//! solved under a per-tick expansion budget. Formation slots and choke lanes live on
//! [`crate::crowd`] and are driven from [`DetAgents::crowd_refresh`], [`DetAgents::crowd_slot`]
//! and [`DetAgents::crowd_bent_slot`] (plan row S-05, ADR-8448).
//!
//! The host supplies the world through two traits: [`DetAgentWorld`] (reads) and
//! [`DetAgentWorldMut`] (the two writes the wedge slide and the default mover need). Moving a body
//! is the host's [`DetMover`]; [`KinematicMover`] is the default. The layer itself moves a body
//! only in [`DetAgents::commit_escapes`].
//!
//! Every decision is integer or a fixed-order float expression from [`regia_detsim`]; agents live
//! in a `BTreeMap`; no wall clock. See the book page "Deterministic navigation grid".

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use regia_detsim::avoid;
use regia_detsim::float::{det_clamp, det_max, det_min};
use regia_detsim::math;
use serde::Serialize;

use crate::crowd::{Crowd, CrowdReport};
use crate::grid::{
    self, ExclusionDisc, GridGround, GridSettings, GroundKey, NavGrid, NavStatic, Profile, Stamp,
};
use crate::search::{self, PlanKind};

/// A goal in clear sight and nearer than this is steered at directly.
pub const DIRECT_M: f32 = 60.0;
/// No progress for this long is a stuck mover (2.5 s at 60 Hz).
pub const STUCK_TICKS: u64 = 150;
/// Less movement than this in the stuck window counts as no progress.
pub const STUCK_MOVE_M: f32 = 2.5;
/// A moving goal re-plans at most this often (0.5 s).
pub const REPATH_MIN_TICKS: u64 = 30;
/// An agent nobody steered for this long is dropped (3 s).
pub const AGENT_IDLE_TICKS: u64 = 180;
/// Stuck this many times in a row counts one give-up (the counter restarts).
pub const GIVE_UP_STUCK: u32 = 4;
/// Immobile this long (1.5 s) and not in a queue: slide to a clear cell before the stuck window
/// closes, so a recovered mover is not an event.
pub const ESCAPE_NUDGE_TICKS: u64 = 90;
/// Immobile this long (3 s) even in a queue: slide anyway.
pub const ESCAPE_STILL_TICKS: u64 = 180;
/// After a slide lands, keep steering along it this long (0.75 s) so the mover drives out of the
/// crowd instead of turning straight back into it.
pub const ESCAPE_DEPART_TICKS: u64 = 45;
/// How far one escape slide may move a mover (m).
pub const ESCAPE_STEP_M: f32 = 12.0;
/// A slide shorter than this does not clear the disc, so it is not used.
const ESCAPE_MIN_M: f32 = 6.0;
/// Furthest a slide may go: one cell past a neighbour.
const ESCAPE_MAX_M: f32 = 12.0;
/// The slide moves at about this speed (m/s), and never more than the cap per tick.
const ESCAPE_SLIDE_SPEED_M_S: f32 = 3.0;
const ESCAPE_SLIDE_STEP_M: f32 = 3.0;
/// Kinematic movers: cruise speeds above this carry a folded boost; the mover cruises at this
/// fraction of them.
const KIN_TURBO_FOLDED_ABOVE_M_S: f32 = 15.0;
const KIN_CRUISE_FRACTION: f32 = 0.55;
/// Ramp: cruise speed reached in about 1.25 s, never slower than this.
const KIN_ACCEL_PER_S: f32 = 0.8;
const KIN_ACCEL_MIN_M_S2: f32 = 6.0;
/// A sidestep may be this much further from the goal. Further is a retreat.
const ESCAPE_RETREAT_M: f32 = 4.0;
/// No progress this close to the goal (past the park distance) is arrival, not stuck.
pub const NEAR_GOAL_M: f32 = 10.0;
/// Further than this from the current leg re-plans.
pub const OFF_PATH_M: f32 = 20.0;
/// A pit refusal or climb-out aims this far past the kill volume's radius.
pub const PIT_EXIT_M: f32 = 10.0;
/// Height above a kill volume's kill plane that routes and steering already treat as no-go.
pub const KILL_VOLUME_MARGIN_M: f32 = 5.0;
/// How far (in cell rings) the stuck steer-out looks for an open cell to walk to.
const ESCAPE_AIM_RINGS: i32 = 6;
/// Extra gap kept between two discs by separation.
pub const SEPARATION_GAP_M: f32 = 2.0;
const NB_CELL_M: f32 = 20.0;
/// How far from the asked goal a snapped route may end and still count as arriving.
const ROUTE_SNAP_SLACK_M: f32 = 20.0;

/// The agent layer's settings. Every peer of a lockstep match must use the same values. The
/// defaults are the source's values.
#[derive(Debug, Clone, PartialEq)]
pub struct DetAgentSettings {
    /// The grid's settings (cell size, movement and size classes, profiles).
    pub grid: Arc<GridSettings>,
    /// Seconds per tick for the kinematic mover and the wedge slide.
    pub tick_dt: f32,
    /// A goal in clear sight and nearer than this is steered at directly (m).
    pub direct_m: f32,
    /// No progress for this many ticks is a stuck mover.
    pub stuck_ticks: u64,
    /// Less movement than this in the stuck window counts as no progress (m).
    pub stuck_move_m: f32,
    /// A moving goal re-plans at most this often (ticks).
    pub repath_min_ticks: u64,
    /// An agent nobody steered for this many ticks is dropped.
    pub agent_idle_ticks: u64,
    /// Stuck events in a row that count one give-up.
    pub give_up_stuck: u32,
    /// Immobile this many ticks with no progress: slide to a clear cell.
    pub escape_nudge_ticks: u64,
    /// Immobile this many ticks even in a queue: slide anyway.
    pub escape_still_ticks: u64,
    /// Ticks to keep steering along a landed slide.
    pub escape_depart_ticks: u64,
    /// No progress this close to the goal (past the park distance) is arrival (m).
    pub near_goal_m: f32,
    /// Further than this from the current leg re-plans (m).
    pub off_path_m: f32,
    /// A climb-out aims this far past the kill volume's radius (m).
    pub pit_exit_m: f32,
    /// Extra gap separation keeps between two discs (m).
    pub separation_gap_m: f32,
    /// Cell expansions all requests may spend in one tick; `None` scales the source's budget with
    /// the grid ([`search::path_tick_budget_for`]).
    pub tick_budget: Option<i64>,
}

impl Default for DetAgentSettings {
    fn default() -> Self {
        DetAgentSettings {
            grid: Arc::new(GridSettings::hover_ground_jet()),
            tick_dt: 1.0 / 60.0,
            direct_m: DIRECT_M,
            stuck_ticks: STUCK_TICKS,
            stuck_move_m: STUCK_MOVE_M,
            repath_min_ticks: REPATH_MIN_TICKS,
            agent_idle_ticks: AGENT_IDLE_TICKS,
            give_up_stuck: GIVE_UP_STUCK,
            escape_nudge_ticks: ESCAPE_NUDGE_TICKS,
            escape_still_ticks: ESCAPE_STILL_TICKS,
            escape_depart_ticks: ESCAPE_DEPART_TICKS,
            near_goal_m: NEAR_GOAL_M,
            off_path_m: OFF_PATH_M,
            pit_exit_m: PIT_EXIT_M,
            separation_gap_m: SEPARATION_GAP_M,
            tick_budget: None,
        }
    }
}

/// One body as the agent layer reads it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DetBody {
    /// World position `[x, y, z]` (m).
    pub pos: [f32; 3],
    /// Collision radius (m).
    pub radius: f32,
    /// The movement profile of a mover; `None` for a body that never moves (a building, a wreck).
    pub profile: Option<Profile>,
    /// Team id (the host's meaning; the layer reports it only).
    pub team: u8,
    /// Ground-plane velocity `[x, z]` (m/s) when the host moves the body itself; zero otherwise.
    pub vel: [f32; 2],
    /// Heading (radians, 0 = +Z, `atan2(x, z)`).
    pub yaw: f32,
    /// Top speed (m/s) of a mover.
    pub cruise_m_s: f32,
    /// A soft body (a person on foot): vehicles neither avoid it nor queue behind it.
    pub soft: bool,
}

/// The host's world, read side. Every peer must answer with the same bits.
pub trait DetAgentWorld {
    /// Every live body id, ascending.
    fn ids(&self) -> Vec<u32>;
    /// One live body, `None` for an unknown or dead id.
    fn body(&self, id: u32) -> Option<DetBody>;
    /// The disc a body stamps on the grid (a building, a wreck, a parked vehicle), if any.
    fn obstacle(&self, id: u32) -> Option<Stamp>;
    /// Ground height at a world point for a landed slide; `None` keeps the body's height.
    fn ground_y(&self, x: f32, z: f32) -> Option<f32> {
        let _ = (x, z);
        None
    }
}

/// The host's world, write side: the wedge slide and [`KinematicMover`] write through it.
pub trait DetAgentWorldMut: DetAgentWorld {
    /// Set a body's position.
    fn set_position(&mut self, id: u32, pos: [f32; 3]);
    /// Set a body's heading (radians, `atan2(x, z)`).
    fn set_yaw(&mut self, id: u32, yaw: f32);
}

/// Moves one mover toward a goal for one tick, steering through the agent layer.
///
/// [`KinematicMover`] is the default. A host with its own physics implements this: call
/// [`DetAgents::steer`] for the steer point and drive the body there, then report heading and
/// speed with [`DetAgents::note_moved`] so the neighbours' avoidance reads it.
pub trait DetMover {
    /// Drive `id` one tick toward `goal`, stopping within `park` meters. True once parked.
    fn move_kinematic(&mut self, id: u32, goal: [f32; 3], park: f32) -> bool;
}

/// A deterministic kill volume: a vertical cylinder whose inside, below the kill plane plus
/// [`KILL_VOLUME_MARGIN_M`], is no-go ground for routes and steering (a shaft or a pit).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DetKillVolume {
    /// Center X (m).
    pub center_x: f32,
    /// Center Z (m).
    pub center_z: f32,
    /// Radius (m).
    pub radius: f32,
    /// The kill plane (m).
    pub kill_y: f32,
}

impl DetKillVolume {
    /// True when the point at body height `y` is inside the radius and below
    /// `kill_y + KILL_VOLUME_MARGIN_M`.
    pub fn claims(&self, x: f32, z: f32, y: f32) -> bool {
        let (dx, dz) = (x - self.center_x, z - self.center_z);
        let inside = dx * dx + dz * dz <= self.radius * self.radius;
        inside && y < self.kill_y + KILL_VOLUME_MARGIN_M
    }

    /// Nearest point outside the volume plus a stand-off, on the radial from the center through
    /// `from`. At the exact center the exit is +X.
    pub fn rim_exit(&self, from: [f32; 2], stand_off_m: f32) -> [f32; 2] {
        let (dx, dz) = (from[0] - self.center_x, from[1] - self.center_z);
        let len = math::sqrt(dx * dx + dz * dz);
        let (ux, uz) = if len < 1e-3 {
            (1.0, 0.0)
        } else {
            (dx / len, dz / len)
        };
        let r = self.radius + stand_off_m;
        [self.center_x + ux * r, self.center_z + uz * r]
    }

    /// The grid's no-go disc for this volume.
    pub fn exclusion(&self) -> ExclusionDisc {
        ExclusionDisc {
            center_x: self.center_x,
            center_z: self.center_z,
            radius: self.radius,
            below_y: self.kill_y + KILL_VOLUME_MARGIN_M,
        }
    }
}

/// What a mover should do this tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavSteer {
    /// Point to steer at (look-ahead on the route, or the goal).
    pub point: [f32; 3],
    /// Distance left along the route to the goal.
    pub remaining: f32,
    /// Straight distance to the goal.
    pub goal_dist: f32,
    /// Inside the park distance, or holding at the end of a partial route.
    pub arrived: bool,
    /// 0.25..1: slow for a corner or to let another mover through.
    pub speed_scale: f32,
    /// World XZ push from neighbours (0..1.5, unitless).
    pub avoid: [f32; 2],
    /// Stuck: reverse this tick.
    pub backoff: bool,
    /// +1 or -1: which way to turn while backing off.
    pub backoff_side: f32,
    /// Following a route (false = steering straight at the goal).
    pub routed: bool,
    /// Wedge recovery: the slide toward `point` lands in [`DetAgents::commit_escapes`].
    pub escape: bool,
    /// Distance to the next route corner.
    pub corner_dist: f32,
    /// How far the route turns at the next corner (radians, 0 = straight on).
    pub corner_turn: f32,
}

impl NavSteer {
    /// Straight at the goal, arrived inside `park`.
    pub fn straight(pos: [f32; 3], goal: [f32; 3], park: f32) -> NavSteer {
        let d = dist2([pos[0], pos[2]], [goal[0], goal[2]]);
        NavSteer {
            point: goal,
            remaining: d,
            goal_dist: d,
            arrived: d <= park,
            speed_scale: 1.0,
            avoid: [0.0, 0.0],
            backoff: false,
            backoff_side: 1.0,
            routed: false,
            corner_dist: f32::MAX,
            corner_turn: 0.0,
            escape: false,
        }
    }
}

/// Counters for tests, probes and the agent protocol. The sim never reads them.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PathStats {
    /// Ticks [`KinematicMover`] moved a body.
    pub kinematic_moves: u64,
    /// Route requests queued.
    pub requests: u64,
    /// Requests solved (any kind).
    pub solved: u64,
    /// Solved as a partial route.
    pub partial: u64,
    /// Solved as the coarse fallback.
    pub coarse_fallback: u64,
    /// Requests with no route at all.
    pub failed: u64,
    /// Fine search expansions spent.
    pub fine_expansions: u64,
    /// Coarse search expansions spent.
    pub coarse_expansions: u64,
    /// Most work spent in one tick.
    pub max_tick_work: i64,
    /// Longest the queue has been.
    pub queued_max: usize,
    /// Steer calls.
    pub steer_calls: u64,
    /// Steer calls that followed a route.
    pub routed_calls: u64,
    /// Steer calls that steered straight at a goal in sight.
    pub direct_calls: u64,
    /// Stuck events (a wedge with nowhere to go, counted once per wedge).
    pub stuck_events: u64,
    /// Longest continuous immobility while a mover still had a goal, in ticks.
    pub max_still_ticks: u64,
    /// Escape slides that moved a body by at least the stuck distance.
    pub escape_commits: u64,
    /// Ticks a steer asked for a slide.
    pub escape_attempts: u64,
    /// Steer calls where the velocity-obstacle solver ran.
    pub orca_calls: u64,
    /// Give-ups after repeated stuck events.
    pub gave_up: u64,
    /// The first 2000 stuck events: (tick, id, x, z, routed, goal x, goal z).
    pub stuck_log: Vec<(u64, u32, f32, f32, bool, f32, f32)>,
    /// Ticks the obstacle stamp set changed.
    pub obstacle_changes: u64,
    /// Re-plans because the goal moved.
    pub repath_goal: u64,
    /// Steer points deflected off the kill volume.
    pub pit_guards: u64,
    /// Steers that drove a mover inside the kill volume back toward the rim.
    pub pit_climbs: u64,
    /// A moved goal still in sight of the route's end: the last leg was moved, no search.
    pub route_extended: u64,
    /// Re-plans because a stamp changed a cluster on the route.
    pub repath_obstacle: u64,
    /// Re-plans after a stuck event.
    pub repath_stuck: u64,
    /// Re-plans because the mover left its route.
    pub repath_off_path: u64,
    /// Planned length over straight length, summed over full plans longer than 40 m.
    pub planned_len: f64,
    /// Straight length of those plans.
    pub straight_len: f64,
    /// Grid builds.
    pub grid_builds: u64,
    /// Ticks a flow's slots were in choke files.
    pub crowd_lane_ticks: u64,
    /// Flows started.
    pub crowd_flows: u64,
    /// New flows refused because the group cap was full.
    pub crowd_dropped: u64,
}

#[derive(Debug, Clone, Default)]
struct Agent {
    profile: Profile,
    pending: bool,
    req_from: [f32; 2],
    req_goal: [f32; 2],
    path: Vec<[f32; 2]>,
    /// suffix[i]: route length from waypoint i to the end.
    suffix: Vec<f32>,
    idx: usize,
    path_goal: [f32; 2],
    path_tick: u64,
    /// A plan was made at least once (`path_goal` and `path_tick` are real).
    asked: bool,
    invalid: bool,
    arrived: bool,
    /// Goal cell whose snap distance is cached in `goal_ring`.
    goal_cell: u32,
    goal_ring: i32,
    /// Start cell whose escape distance is cached in `start_ring`.
    start_cell: u32,
    start_ring: i32,
    prog_ref: f32,
    prog_pos: [f32; 2],
    prog_tick: u64,
    backoff_until: u64,
    /// Holds (arrived) until this tick.
    hold_until: u64,
    stuck: u32,
    /// Tick the current immobility started (0 = not yet sampled).
    still_tick: u64,
    still_pos: [f32; 2],
    /// An escape slide for this wedge has already been counted.
    escape_open: bool,
    /// Set by the steer; [`DetAgents::commit_escapes`] applies it after collisions.
    escape_to: Option<[f32; 3]>,
    /// Unit direction to keep steering after a commit.
    depart: [f32; 2],
    depart_until: u64,
    last_tick: u64,
    /// Current speed and heading of the mover (for the neighbours' avoidance).
    speed: f32,
    heading: [f32; 2],
    moved_tick: u64,
}

impl Agent {
    fn clear_path(&mut self) {
        self.path.clear();
        self.suffix.clear();
        self.idx = 0;
        self.invalid = false;
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct NbUnit {
    id: u32,
    x: f32,
    y: f32,
    z: f32,
    r: f32,
    /// Ground-plane velocity (m/s), for the solver.
    vx: f32,
    vz: f32,
    moving: bool,
    soft: bool,
}

#[derive(Debug, Clone, Default)]
struct Neighbours {
    tick: u64,
    valid: bool,
    units: Vec<NbUnit>,
    /// Bucket b holds `order[start[b]..start[b + 1]]`.
    start: Vec<u32>,
    order: Vec<u32>,
    side: usize,
    origin: f32,
}

impl Neighbours {
    fn bucket(&self, x: f32, z: f32) -> (usize, usize) {
        let max = self.side as i32 - 1;
        let bx = (((x - self.origin) / NB_CELL_M).floor() as i32).clamp(0, max);
        let b_z = (((z - self.origin) / NB_CELL_M).floor() as i32).clamp(0, max);
        (bx as usize, b_z as usize)
    }
}

#[derive(Clone, Copy)]
struct SteerInput {
    id: u32,
    pos: [f32; 2],
    /// Body height (m): who it can bump into.
    y: f32,
    yaw: f32,
    vel: [f32; 2],
    radius: f32,
    profile: Profile,
    goal: [f32; 3],
    park: f32,
    speed: f32,
    tick: u64,
    hull: bool,
}

/// One routed agent in [`DetAgentsReport`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DetAgentRoute {
    /// The mover's id.
    pub id: u32,
    /// World XZ waypoints.
    pub waypoints: Vec<[f32; 2]>,
    /// Index of the next waypoint.
    pub next: usize,
}

/// Grid size in [`DetAgentsReport`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DetGridSize {
    /// Cells along X.
    pub width: usize,
    /// Cells along Z.
    pub height: usize,
    /// Cell size (m).
    pub cell_m: f32,
}

/// The agent layer's state as plain rows: what `nav.det_agents` returns.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DetAgentsReport {
    /// Live agents.
    pub agents: usize,
    /// Requests waiting in the FIFO queue.
    pub queue: usize,
    /// Pathing switched off (every mover steers straight).
    pub disabled: bool,
    /// The grid, once built.
    pub grid: Option<DetGridSize>,
    /// The last grid build error, if the settings or the ground were refused.
    pub grid_error: Option<String>,
    /// The counters (stuck and give-up counters among them).
    pub stats: PathStats,
    /// Agents with a route, ascending id.
    pub routes: Vec<DetAgentRoute>,
    /// Flows, registered chokes and which flows are in lane files.
    pub crowd: CrowdReport,
}

/// The agent layer: one agent per steered mover, the request queue and the grid. Cloned with the
/// sim state; a clone keeps every route.
#[derive(Clone, Default)]
pub struct DetAgents {
    settings: DetAgentSettings,
    grid: Option<NavGrid>,
    grid_error: Option<String>,
    ground: Option<Arc<dyn GridGround + Send + Sync>>,
    declared_extent: Option<[f32; 2]>,
    kill_volume: Option<DetKillVolume>,
    agents: BTreeMap<u32, Agent>,
    queue: VecDeque<u32>,
    budget_tick: u64,
    budget_left: i64,
    budget_used: i64,
    budget_init: bool,
    nb: Neighbours,
    scratch: search::Scratch,
    /// Off switch: every mover steers straight at its goal.
    pub disabled: bool,
    /// Counters.
    pub stats: PathStats,
    /// Formation slots and choke lanes (plan row S-05).
    pub crowd: Crowd,
}

impl std::fmt::Debug for DetAgents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DetAgents")
            .field("grid", &self.grid.as_ref().map(|g| (g.width(), g.height())))
            .field("agents", &self.agents.len())
            .field("queue", &self.queue.len())
            .field("disabled", &self.disabled)
            .field("crowd_groups", &self.crowd.group_count())
            .field("crowd_chokes", &self.crowd.choke_count())
            .finish()
    }
}

/// How far a wedge slide may move. Near the goal the step stays short of it.
fn escape_reach(inp: &SteerInput, pos: [f32; 2], stuck_move_m: f32) -> (f32, f32) {
    let dx = inp.goal[0] - pos[0];
    let dz = inp.goal[2] - pos[1];
    let d = math::sqrt(dx * dx + dz * dz);
    if d < 15.0 {
        let min_m = stuck_move_m + 0.5;
        let max_m = det_min(det_max(d - inp.park, min_m), 10.0);
        (min_m, max_m)
    } else {
        (ESCAPE_MIN_M, ESCAPE_MAX_M)
    }
}

fn dist2(a: [f32; 2], b: [f32; 2]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[1] - b[1];
    math::sqrt(dx * dx + dz * dz)
}

fn plan_len(w: &[[f32; 2]]) -> f32 {
    w.windows(2).map(|p| dist2(p[0], p[1])).sum()
}

#[inline]
fn norm(v: [f32; 2]) -> [f32; 2] {
    let len = math::sqrt(v[0] * v[0] + v[1] * v[1]);
    if len < 1e-5 {
        [0.0, 0.0]
    } else {
        [v[0] / len, v[1] / len]
    }
}

/// The profile's cliff slope (rise over run).
fn cliff_of(grid: &NavGrid, profile: Profile) -> f32 {
    let s = grid.settings();
    let def = s.profile(profile);
    s.move_classes
        .get(usize::from(def.move_class))
        .map_or(0.0, |c| c.cliff)
}

impl DetAgents {
    /// An empty layer with these settings.
    pub fn new(settings: DetAgentSettings) -> DetAgents {
        DetAgents {
            settings,
            ..DetAgents::default()
        }
    }

    /// The settings.
    pub fn settings(&self) -> &DetAgentSettings {
        &self.settings
    }

    /// The grid, once built.
    pub fn grid(&self) -> Option<&NavGrid> {
        self.grid.as_ref()
    }

    /// Set the ground (`None`: the flat canvas, or the declared extent). A ground whose key
    /// differs from the grid's rebuilds the grid on the next call, which drops every route.
    pub fn set_ground(&mut self, ground: Option<Arc<dyn GridGround + Send + Sync>>) {
        self.ground = ground;
    }

    /// The playfield `[width, depth]` (m) used when there is no ground.
    pub fn set_declared_extent(&mut self, extent: Option<[f32; 2]>) {
        self.declared_extent = extent;
    }

    /// Set the kill volume (`None`: none). Routes and steering refuse its inside.
    pub fn set_kill_volume(&mut self, volume: Option<DetKillVolume>) {
        self.kill_volume = volume;
    }

    /// The kill volume.
    pub fn kill_volume(&self) -> Option<DetKillVolume> {
        self.kill_volume
    }

    /// Can a `profile` mover drive from `from` to `goal` (world XZ)? The same search the agents
    /// use, asked once. `None` while there is no grid (the caller then trusts the order). A goal
    /// in a cliff-locked region or with no passable cell is not reachable; a goal snapped to a
    /// free cell within 20 m is.
    pub fn route_reaches(&mut self, profile: Profile, from: [f32; 2], goal: [f32; 2]) -> Option<bool> {
        let grid = self.grid.as_ref()?;
        Some(match search::plan(grid, &mut self.scratch, profile, from, goal) {
            Some(plan) => {
                let end = plan.waypoints.last().copied().unwrap_or(from);
                plan.kind != PlanKind::Partial && (!plan.snapped || dist2(end, goal) <= ROUTE_SNAP_SLACK_M)
            }
            None => false,
        })
    }

    /// Agents with a route: (id, waypoints, next waypoint index), ascending id.
    pub fn debug_paths(&self) -> Vec<(u32, Vec<[f32; 2]>, usize)> {
        self.agents
            .iter()
            .filter(|(_, a)| !a.path.is_empty() && !a.arrived)
            .map(|(id, a)| (*id, a.path.clone(), a.idx))
            .collect()
    }

    /// Live agents.
    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }

    /// Requests waiting.
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// Last kinematic speed of `id` (m/s); zero when untracked.
    pub fn kinematic_speed(&self, id: u32) -> f32 {
        self.agents.get(&id).map(|agent| agent.speed).unwrap_or(0.0)
    }

    /// The state as plain rows (the `nav.det_agents` protocol method).
    pub fn report(&self) -> DetAgentsReport {
        DetAgentsReport {
            agents: self.agents.len(),
            queue: self.queue.len(),
            disabled: self.disabled,
            grid: self.grid.as_ref().map(|g| DetGridSize {
                width: g.width(),
                height: g.height(),
                cell_m: g.cell_m(),
            }),
            grid_error: self.grid_error.clone(),
            stats: self.stats.clone(),
            routes: self
                .debug_paths()
                .into_iter()
                .map(|(id, waypoints, next)| DetAgentRoute { id, waypoints, next })
                .collect(),
            crowd: self.crowd.report(),
        }
    }

    /// Build the grid now (map load), so the first tick does not pay for it.
    pub fn prepare(&mut self, world: &dyn DetAgentWorld) {
        let _span = tracing::debug_span!("det_agents_prepare").entered();
        self.ensure_grid(world);
    }

    /// Per-tick upkeep: grid, obstacles, idle agents, the request queue. Call once per tick
    /// before the movers.
    pub fn service(&mut self, tick: u64, world: &dyn DetAgentWorld) {
        if self.disabled {
            return;
        }
        // Idle flows evaporate even when nobody is routing yet.
        self.crowd.service(tick);
        // Nothing has asked for a route yet (and the map did not prebuild): no grid to keep.
        if self.grid.is_none() && self.agents.is_empty() {
            return;
        }
        let _span = tracing::trace_span!("det_agents_service").entered();
        self.ensure_grid(world);
        self.sync_obstacles(world);
        let idle = self.settings.agent_idle_ticks;
        self.agents.retain(|_, a| a.last_tick + idle >= tick);
        if self.queue.iter().any(|id| !self.agents.contains_key(id)) {
            let agents = &self.agents;
            self.queue.retain(|id| agents.contains_key(id));
        }
        self.pump(tick);
    }

    /// Register a narrow as a choke point. Points within a metre of a known one are the same
    /// choke. Tunnel mouths and junctions come from [`Self::register_tunnel_chokes`].
    pub fn register_choke(&mut self, at: [f32; 2]) {
        self.crowd.register_choke(at);
    }

    /// Register every tunnel mouth and junction of `layer` as a choke point (KI-WD-030,
    /// ADR-10070; see [`Crowd::register_tunnel_chokes`]). Returns how many chokes were added.
    pub fn register_tunnel_chokes(&mut self, layer: &crate::layer::TunnelLayer) -> usize {
        self.crowd.register_tunnel_chokes(layer)
    }

    /// Forget every registered choke (map change).
    pub fn clear_chokes(&mut self) {
        self.crowd.clear_chokes();
    }

    /// Refresh the flow for one order group. `members` are living ids with world XZ positions,
    /// `anchor` the order point. Slots face from the group's centroid toward the anchor and narrow
    /// to lanes near a registered choke. Under two living members the flow ends.
    pub fn crowd_refresh(&mut self, group: u32, members: &[(u32, [f32; 2])], anchor: [f32; 2], tick: u64) {
        if self.disabled {
            return;
        }
        let mut ids: Vec<u32> = members.iter().map(|(id, _)| *id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids.truncate(self.crowd.settings().max_members);
        if ids.len() < 2 {
            self.crowd.disband(group);
            return;
        }
        let mut cx = 0.0f32;
        let mut cz = 0.0f32;
        for id in &ids {
            if let Some((_, p)) = members.iter().find(|(m, _)| m == id) {
                cx += p[0];
                cz += p[1];
            }
        }
        let n = ids.len() as f32;
        let centroid = [cx / n, cz / n];
        let choke = self.crowd.choke_near(self.grid.as_ref(), centroid, anchor);
        let is_new = !self.crowd.contains(group);
        let lane = self.crowd.refresh(group, &ids, anchor, centroid, choke, tick);
        self.crowd.assign_by_position(group, members);
        if lane {
            self.stats.crowd_lane_ticks += 1;
        }
        if is_new && self.crowd.contains(group) {
            self.stats.crowd_flows += 1;
        } else if is_new {
            self.stats.crowd_dropped += 1;
        }
    }

    /// A flow member's slot as a world XZ point, or `None` without a flow.
    pub fn crowd_slot(&self, group: u32, member: u32) -> Option<[f32; 2]> {
        self.crowd.slot_offset(group, member)
    }

    /// Slide a slot back along a blocked member-to-slot leg so it bends round the obstacle.
    /// Off-grid pairs and a missing grid pass the slot through unchanged.
    pub fn crowd_bent_slot(&self, profile: Profile, member: [f32; 2], slot: [f32; 2]) -> ([f32; 2], bool) {
        match self.grid.as_ref() {
            Some(grid) => Crowd::bend_slot(grid, profile, member, slot),
            None => (slot, false),
        }
    }

    /// Where mover `id` should steer this tick to reach `goal` and stop within `park` meters.
    /// Never moves anything itself.
    pub fn steer(
        &mut self,
        id: u32,
        goal: [f32; 3],
        park: f32,
        tick: u64,
        world: &dyn DetAgentWorld,
    ) -> NavSteer {
        let Some(body) = world.body(id) else {
            return NavSteer::straight(goal, goal, park);
        };
        let p = body.pos;
        let input = SteerInput {
            id,
            pos: [p[0], p[2]],
            y: p[1],
            yaw: body.yaw,
            vel: body.vel,
            radius: body.radius,
            profile: body.profile.unwrap_or_default(),
            goal,
            park,
            speed: det_max(body.cruise_m_s, 1.0),
            tick,
            hull: !body.soft,
        };
        let _span = tracing::trace_span!("det_agents_steer").entered();
        let mut out = self.steer_inner(&input, world);
        out.point[1] = goal[1];
        out
    }

    /// Clear straight line on the grid from mover `id` to `goal` (any range). True when pathing
    /// is off, the mover is unknown or there is no grid.
    pub fn line_clear(&mut self, id: u32, goal: [f32; 3], world: &dyn DetAgentWorld) -> bool {
        if self.disabled {
            return true;
        }
        let Some(body) = world.body(id) else {
            return true;
        };
        let p = body.pos;
        let profile = body.profile.unwrap_or_default();
        if !self.ensure_grid(world) {
            return true;
        }
        let Some(grid) = self.grid.as_ref() else {
            return true;
        };
        if !grid.in_bounds_world(p[0], p[2]) || !grid.in_bounds_world(goal[0], goal[2]) {
            return true;
        }
        let from = grid.cell_at(p[0], p[2]);
        let to = grid.cell_at(goal[0], goal[2]);
        let start_ring = if grid.passable(profile, from) {
            0
        } else {
            Self::goal_ring(grid, profile, from)
        };
        let goal_ring = if grid.passable(profile, to) {
            0
        } else {
            Self::goal_ring(grid, profile, to)
        };
        Self::clear_line(grid, profile, from, to, start_ring, goal_ring)
    }

    /// Note that a host mover drove `id` this tick (heading and speed for the neighbours'
    /// avoidance).
    pub fn note_moved(&mut self, id: u32, heading: [f32; 2], speed: f32, tick: u64) {
        if let Some(agent) = self.agents.get_mut(&id) {
            agent.heading = heading;
            agent.speed = speed;
            agent.moved_tick = tick;
        }
    }

    /// Hold `id` as arrived until `tick` (a host's give-up hold).
    pub fn hold_until(&mut self, id: u32, tick: u64) {
        if let Some(agent) = self.agents.get_mut(&id) {
            agent.hold_until = tick;
        }
    }

    /// A point within 24 m of `from` whose grid line back to `threat` is blocked: eight fixed
    /// angles, the lowest index wins.
    pub fn ridge_hide(&self, profile: Profile, from: [f32; 3], threat: [f32; 3]) -> Option<[f32; 3]> {
        let grid = self.grid.as_ref()?;
        let threat_cell = grid.cell_at(threat[0], threat[2]);
        for i in 0..8 {
            let angle = i as f32 * std::f32::consts::TAU / 8.0;
            let x = from[0] + math::sin(angle) * 24.0;
            let z = from[2] + math::cos(angle) * 24.0;
            if !grid.in_bounds_world(x, z) {
                continue;
            }
            let cell = grid.cell_at(x, z);
            if !grid.passable(profile, cell) {
                continue;
            }
            if grid.los(profile, threat_cell, cell, grid::COST_STEEP, 1) {
                continue;
            }
            let c = grid.center(cell);
            return Some([c[0], from[1], c[1]]);
        }
        None
    }

    /// Land escape slides after collisions and orders. A slide applied during steering is pushed
    /// back by the same tick's separation; committing here leaves the mover on the clear cell the
    /// next steer samples. The slide is bounded: at most 3 m per tick at about 3 m/s, the cell
    /// stays claimed until the mover gets there.
    pub fn commit_escapes(&mut self, tick: u64, world: &mut dyn DetAgentWorldMut) {
        let pending: Vec<(u32, [f32; 3])> = self
            .agents
            .iter()
            .filter_map(|(id, agent)| agent.escape_to.map(|p| (*id, p)))
            .collect();
        let step = det_min(
            ESCAPE_SLIDE_SPEED_M_S * self.settings.tick_dt,
            ESCAPE_SLIDE_STEP_M,
        );
        for (id, target) in pending {
            let body = world.body(id);
            let mut p = target;
            let mut partial = false;
            if let Some(b) = body {
                let h = [b.pos[0], b.pos[2]];
                let (dx, dz) = (target[0] - h[0], target[2] - h[1]);
                let len = math::sqrt(dx * dx + dz * dz);
                if len > step {
                    let k = step / len;
                    p = [h[0] + dx * k, target[1], h[1] + dz * k];
                    partial = true;
                }
            }
            let moved = body.map(|b| {
                let mut y = b.pos[1];
                if let Some(ground) = world.ground_y(p[0], p[2])
                    && y > ground - 8.0
                {
                    y = ground;
                }
                world.set_position(id, [p[0], y, p[2]]);
                (p[0] - b.pos[0], p[2] - b.pos[2])
            });
            let Some(agent) = self.agents.get_mut(&id) else {
                continue;
            };
            if partial {
                // Still sliding: keep the claim, keep the stuck timers quiet, steer again once it lands.
                if moved.is_some() {
                    agent.still_pos = [p[0], p[2]];
                    agent.still_tick = tick;
                    agent.prog_pos = [p[0], p[2]];
                    agent.prog_tick = tick;
                    agent.speed = 0.0;
                } else {
                    agent.escape_to = None;
                }
                continue;
            }
            agent.escape_to = None;
            let Some((dx, dz)) = moved else {
                continue;
            };
            if math::sqrt(dx * dx + dz * dz) < self.settings.stuck_move_m {
                continue;
            }
            let landed = [p[0], p[2]];
            agent.still_pos = landed;
            agent.still_tick = tick;
            agent.prog_pos = landed;
            agent.prog_tick = tick;
            agent.speed = 0.0;
            // Keep steering toward the goal, not along a sidestep, and only while the landing is
            // still far from it.
            let gx = agent.path_goal[0] - p[0];
            let gz = agent.path_goal[1] - p[2];
            let glen = math::sqrt(gx * gx + gz * gz);
            if glen > 25.0 {
                agent.depart = [gx / glen, gz / glen];
                agent.depart_until = tick.saturating_add(self.settings.escape_depart_ticks);
            }
            self.stats.escape_commits += 1;
        }
    }

    /// Build the grid when there is none or the ground changed; refresh the kill-volume
    /// exclusion in place. False when the grid cannot be built (bad settings or ground).
    fn ensure_grid(&mut self, world: &dyn DetAgentWorld) -> bool {
        let ground: Option<&dyn GridGround> = match self.ground.as_deref() {
            Some(g) => Some(g),
            None => None,
        };
        let key = GroundKey::covering(ground, self.declared_extent, &self.settings.grid);
        let wanted: Vec<ExclusionDisc> = self.kill_volume.iter().map(DetKillVolume::exclusion).collect();
        if let Some(grid) = self.grid.as_mut()
            && grid.stat.key == key
        {
            // Same ground, but the kill volume may have changed: refresh in place.
            if grid.exclusions() != wanted.as_slice() {
                grid.set_exclusions(&wanted);
                for agent in self.agents.values_mut() {
                    agent.invalid = true;
                }
                self.nb.valid = false;
            }
            return true;
        }
        let built = NavStatic::build_sized(ground, self.declared_extent, self.settings.grid.clone())
            .and_then(|stat| NavGrid::new(Arc::new(stat)));
        match built {
            Ok(mut grid) => {
                grid.set_exclusions(&wanted);
                self.grid = Some(grid);
                self.grid_error = None;
            }
            Err(e) => {
                self.grid = None;
                self.grid_error = Some(e.to_string());
                return false;
            }
        }
        self.agents.clear();
        self.queue.clear();
        self.nb.valid = false;
        self.stats.grid_builds += 1;
        self.sync_obstacles(world);
        true
    }

    /// Stamp new obstacles, lift gone ones, and send routes through the touched clusters back to
    /// the planner.
    fn sync_obstacles(&mut self, world: &dyn DetAgentWorld) {
        let Some(grid) = self.grid.as_mut() else {
            return;
        };
        let now: Vec<(u32, Stamp)> = world
            .ids()
            .into_iter()
            .filter_map(|id| world.obstacle(id).map(|s| (id, s)))
            .collect();
        let sync = grid.sync_stamps(now);
        if !sync.changed {
            return;
        }
        self.stats.obstacle_changes += 1;
        for agent in self.agents.values_mut() {
            agent.goal_cell = u32::MAX;
            agent.start_cell = u32::MAX;
        }
        let mut dirty = vec![false; grid.clusters_x * grid.clusters_z];
        for c in &sync.dirty_clusters {
            if let Some(d) = dirty.get_mut(*c as usize) {
                *d = true;
            }
        }
        // Routes through a changed cluster re-plan.
        let step = grid.cell_m() * 0.5;
        for agent in self.agents.values_mut() {
            if agent.path.len() < 2 || agent.invalid {
                continue;
            }
            let from = agent.idx.saturating_sub(1);
            let mut hit = false;
            'legs: for k in from..agent.path.len() - 1 {
                let (a, b) = (agent.path[k], agent.path[k + 1]);
                let len = dist2(a, b);
                let n = ((len / step) as usize).max(1);
                for s in 0..=n {
                    let t = s as f32 / n as f32;
                    let x = a[0] + (b[0] - a[0]) * t;
                    let z = a[1] + (b[1] - a[1]) * t;
                    if dirty[grid.cluster_of(grid.cell_at(x, z))] {
                        hit = true;
                        break 'legs;
                    }
                }
            }
            if hit {
                agent.invalid = true;
                self.stats.repath_obstacle += 1;
            }
        }
    }

    fn ensure_neighbours(&mut self, tick: u64, world: &dyn DetAgentWorld) {
        if self.nb.valid && self.nb.tick == tick {
            return;
        }
        let Some(grid) = self.grid.as_ref() else {
            return;
        };
        // A square hash over the map's bounding box covers any rectangular grid too.
        let cells = if grid.width() > grid.height() {
            grid.width()
        } else {
            grid.height()
        };
        let world_m = cells as f32 * grid.cell_m();
        let side = ((world_m / NB_CELL_M) as usize + 1).max(1);
        let origin = det_min(grid.stat.origin_x, grid.stat.origin_z) - grid.cell_m() * 0.5;
        let mut units: Vec<NbUnit> = Vec::new();
        for id in world.ids() {
            let Some(b) = world.body(id) else {
                continue;
            };
            if b.profile.is_none() {
                continue;
            }
            let p = b.pos;
            let (mut vx, mut vz, mut moving);
            if let Some(agent) = self.agents.get(&id)
                && agent.moved_tick + 2 >= tick
                && agent.speed > 0.5
                && !agent.arrived
            {
                vx = agent.heading[0] * agent.speed;
                vz = agent.heading[1] * agent.speed;
                moving = true;
            } else {
                let v = b.vel;
                let speed = math::sqrt(v[0] * v[0] + v[1] * v[1]);
                moving = speed > 1.0;
                if moving {
                    vx = v[0];
                    vz = v[1];
                } else {
                    vx = 0.0;
                    vz = 0.0;
                }
                if self.agents.get(&id).is_some_and(|a| a.arrived) {
                    moving = false;
                    vx = 0.0;
                    vz = 0.0;
                }
            }
            if !vx.is_finite() || !vz.is_finite() {
                vx = 0.0;
                vz = 0.0;
                moving = false;
            }
            units.push(NbUnit {
                id,
                x: p[0],
                y: p[1],
                z: p[2],
                r: b.radius,
                vx,
                vz,
                moving,
                soft: b.soft,
            });
        }
        let mut nb = Neighbours {
            tick,
            valid: true,
            units,
            start: vec![0; side * side + 1],
            order: Vec::new(),
            side,
            origin,
        };
        let mut counts = vec![0u32; side * side];
        let keys: Vec<usize> = nb
            .units
            .iter()
            .map(|u| {
                let (bx, b_z) = nb.bucket(u.x, u.z);
                b_z * side + bx
            })
            .collect();
        for k in &keys {
            counts[*k] += 1;
        }
        let mut acc = 0u32;
        for (b, count) in counts.iter().enumerate() {
            nb.start[b] = acc;
            acc += count;
        }
        nb.start[side * side] = acc;
        let mut fill = nb.start.clone();
        nb.order = vec![0; nb.units.len()];
        for (i, k) in keys.iter().enumerate() {
            nb.order[fill[*k] as usize] = i as u32;
            fill[*k] += 1;
        }
        self.nb = nb;
    }

    fn reset_budget(&mut self, tick: u64) {
        if !self.budget_init || self.budget_tick != tick {
            if self.budget_used > self.stats.max_tick_work {
                self.stats.max_tick_work = self.budget_used;
            }
            self.budget_init = true;
            self.budget_tick = tick;
            self.budget_left = match (self.settings.tick_budget, self.grid.as_ref()) {
                (Some(b), _) => b,
                (None, Some(g)) => search::path_tick_budget_for(g.width(), g.height()),
                (None, None) => search::PATH_TICK_BUDGET,
            };
            self.budget_used = 0;
        }
    }

    fn enqueue(&mut self, id: u32, from: [f32; 2], goal: [f32; 2]) {
        let Some(agent) = self.agents.get_mut(&id) else {
            return;
        };
        agent.req_from = from;
        agent.req_goal = goal;
        if !agent.pending {
            agent.pending = true;
            self.queue.push_back(id);
            self.stats.requests += 1;
            if self.queue.len() > self.stats.queued_max {
                self.stats.queued_max = self.queue.len();
            }
        }
    }

    /// Solve queued requests, oldest first, while this tick's budget lasts.
    fn pump(&mut self, tick: u64) {
        self.reset_budget(tick);
        while self.budget_left > 0 {
            let Some(id) = self.queue.pop_front() else {
                break;
            };
            let Some(agent) = self.agents.get(&id) else {
                continue;
            };
            if !agent.pending {
                continue;
            }
            let (profile, from, goal) = (agent.profile, agent.req_from, agent.req_goal);
            let Some(grid) = self.grid.as_ref() else {
                // Wait for the grid: put the request back so a later pump still serves it.
                self.queue.push_front(id);
                break;
            };
            let plan = search::plan(grid, &mut self.scratch, profile, from, goal);
            let work = plan.as_ref().map(|p| p.work).unwrap_or(32) as i64;
            self.budget_left -= work;
            self.budget_used += work;
            let Some(agent) = self.agents.get_mut(&id) else {
                continue;
            };
            agent.pending = false;
            agent.asked = true;
            match plan {
                Some(plan) if plan.kind == PlanKind::Partial && plan_len(&plan.waypoints) < 30.0 => {
                    // Stranded in a pocket: no route worth following; the mover steers straight.
                    self.stats.solved += 1;
                    self.stats.partial += 1;
                    agent.clear_path();
                    agent.path_goal = goal;
                    agent.path_tick = tick;
                }
                Some(plan) if plan.waypoints.len() >= 2 => {
                    self.stats.solved += 1;
                    self.stats.fine_expansions += u64::from(plan.fine_expansions);
                    self.stats.coarse_expansions += u64::from(plan.coarse_expansions);
                    match plan.kind {
                        PlanKind::Partial => self.stats.partial += 1,
                        PlanKind::Coarse => self.stats.coarse_fallback += 1,
                        PlanKind::Full => {}
                    }
                    let mut suffix = vec![0.0f32; plan.waypoints.len()];
                    for k in (0..plan.waypoints.len() - 1).rev() {
                        suffix[k] = suffix[k + 1] + dist2(plan.waypoints[k], plan.waypoints[k + 1]);
                    }
                    let straight = dist2(from, goal);
                    if plan.kind == PlanKind::Full && straight > 40.0 {
                        self.stats.planned_len += f64::from(suffix[0]);
                        self.stats.straight_len += f64::from(straight);
                    }
                    agent.path = plan.waypoints;
                    agent.suffix = suffix;
                    agent.idx = 1;
                    agent.path_goal = goal;
                    agent.path_tick = tick;
                    agent.invalid = false;
                }
                Some(_) => {
                    // Start and goal share a cell: steer straight.
                    agent.clear_path();
                    agent.path_goal = goal;
                    agent.path_tick = tick;
                }
                None => {
                    self.stats.failed += 1;
                    agent.clear_path();
                    agent.path_goal = goal;
                    agent.path_tick = tick;
                }
            }
        }
    }

    /// Rings around a blocked cell to its nearest open cell, plus one.
    fn goal_ring(grid: &NavGrid, profile: Profile, cell: u32) -> i32 {
        match grid.nearest_passable(profile, cell, search::SNAP_RINGS) {
            None => search::SNAP_RINGS,
            Some(open) => {
                let (ax, az) = grid.xz(cell);
                let (bx, b_z) = grid.xz(open);
                let d = (ax as i32 - bx as i32).abs().max((az as i32 - b_z as i32).abs());
                d + 1
            }
        }
    }

    /// Straight line clear for this profile. Cells near a blocked start pass when only an
    /// inflated disc (not a core or a cliff) covers them, so a mover can drive out of its own
    /// yard; cells near a blocked goal pass (the target stands in its own disc).
    fn clear_line(
        grid: &NavGrid,
        profile: Profile,
        from: u32,
        to: u32,
        start_ring: i32,
        goal_ring: i32,
    ) -> bool {
        let (x0, z0) = grid.xz(from);
        let (x1, z1) = grid.xz(to);
        let (x0, z0, x1, z1) = (x0 as i32, z0 as i32, x1 as i32, z1 as i32);
        // No steeper than open ground, or than where the line starts or ends.
        let max_cost = grid::COST_STEEP
            .max(grid.cost(profile, from))
            .max(grid.cost(profile, to));
        grid::supercover(x0, z0, x1, z1, |px, pz, x, z| {
            let cell = grid.index(x as usize, z as usize);
            let cost = grid.cost(profile, cell);
            let step = grid.can_step(profile, grid.index(px as usize, pz as usize), x - px, z - pz);
            if cost != 0 && cost <= max_cost && step {
                return true;
            }
            if goal_ring > 0 && (x - x1).abs() <= goal_ring && (z - z1).abs() <= goal_ring {
                return true;
            }
            if start_ring > 0 && (x - x0).abs() <= start_ring && (z - z0).abs() <= start_ring {
                let core_free = grid.blockers.first().is_some_and(|b| b[cell as usize] == 0);
                return !grid.terrain_blocked(profile, cell) && step && core_free;
            }
            false
        })
    }

    fn steer_inner(&mut self, inp_in: &SteerInput, world: &dyn DetAgentWorld) -> NavSteer {
        let pos3 = [inp_in.pos[0], 0.0, inp_in.pos[1]];
        if self.disabled || !self.ensure_grid(world) {
            return NavSteer::straight(pos3, inp_in.goal, inp_in.park);
        }
        self.stats.steer_calls += 1;
        self.ensure_neighbours(inp_in.tick, world);
        let s = self.settings.clone();
        // Already sliding into the shaft (inside the radius and below the kill plane plus margin):
        // forget the route and climb back out along the radial. This outranks every other
        // steering, including arrival.
        if let Some(vol) = self.kill_volume
            && vol.claims(inp_in.pos[0], inp_in.pos[1], inp_in.y)
        {
            self.stats.pit_climbs += 1;
            let exit = vol.rim_exit(inp_in.pos, s.pit_exit_m);
            let goal_left = dist2(inp_in.pos, [inp_in.goal[0], inp_in.goal[2]]);
            let agent = self.agents.entry(inp_in.id).or_insert_with(|| Agent {
                prog_pos: inp_in.pos,
                prog_ref: goal_left,
                prog_tick: inp_in.tick,
                goal_cell: u32::MAX,
                start_cell: u32::MAX,
                heading: [math::sin(inp_in.yaw), math::cos(inp_in.yaw)],
                ..Agent::default()
            });
            agent.prog_tick = inp_in.tick;
            agent.prog_pos = inp_in.pos;
            agent.prog_ref = goal_left;
            agent.still_tick = inp_in.tick;
            agent.still_pos = inp_in.pos;
            agent.escape_open = false;
            agent.escape_to = None;
            agent.invalid = true;
            return NavSteer {
                point: [exit[0], inp_in.goal[1], exit[1]],
                remaining: dist2(inp_in.pos, exit),
                goal_dist: goal_left,
                arrived: false,
                speed_scale: 1.0,
                avoid: [0.0, 0.0],
                backoff: false,
                backoff_side: 1.0,
                routed: false,
                escape: false,
                corner_dist: f32::MAX,
                corner_turn: 0.0,
            };
        }
        // A goal on a cliff (a formation slot over the edge), or inside the pit, is the nearest
        // open ground instead: there is nothing to touch up there.
        let mut inp_buf = *inp_in;
        if let Some(grid) = self.grid.as_ref()
            && grid.in_bounds_world(inp_buf.goal[0], inp_buf.goal[2])
        {
            let cell = grid.cell_at(inp_buf.goal[0], inp_buf.goal[2]);
            if (grid.terrain_blocked(inp_buf.profile, cell) || grid.excluded(cell))
                && let Some(open) = grid.nearest_passable(inp_buf.profile, cell, search::SNAP_RINGS)
            {
                let c = grid.center(open);
                inp_buf.goal = [c[0], inp_buf.goal[1], c[1]];
            }
        }
        let inp = &inp_buf;
        let mut out = NavSteer::straight(pos3, inp.goal, inp.park);
        let tick = inp.tick;
        let goal2 = [inp.goal[0], inp.goal[2]];
        let pos2 = inp.pos;
        let d_goal = out.goal_dist;
        let agent = self.agents.entry(inp.id).or_insert_with(|| Agent {
            prog_pos: pos2,
            prog_ref: d_goal,
            prog_tick: tick,
            goal_cell: u32::MAX,
            start_cell: u32::MAX,
            heading: [math::sin(inp.yaw), math::cos(inp.yaw)],
            ..Agent::default()
        });
        if agent.profile != inp.profile {
            agent.profile = inp.profile;
            agent.clear_path();
            agent.goal_cell = u32::MAX;
            agent.start_cell = u32::MAX;
        }
        agent.last_tick = tick;

        let mut desired = norm([goal2[0] - pos2[0], goal2[1] - pos2[1]]);
        if out.arrived {
            if !agent.arrived {
                agent.arrived = true;
                agent.clear_path();
                agent.stuck = 0;
            }
            agent.prog_tick = tick;
            agent.prog_pos = pos2;
            agent.prog_ref = d_goal;
            agent.still_tick = tick;
            agent.still_pos = pos2;
            agent.escape_open = false;
            agent.escape_to = None;
            agent.depart_until = 0;
            let (push, _) = self.avoidance(inp, [0.0, 0.0], true);
            out.avoid = push;
            return out;
        }
        agent.arrived = false;

        let Some(grid) = self.grid.as_ref() else {
            return out;
        };
        let on_grid = grid.in_bounds_world(pos2[0], pos2[1]) && grid.in_bounds_world(goal2[0], goal2[1]);
        let from_cell = grid.cell_at(pos2[0], pos2[1]);
        let goal_cell = grid.cell_at(goal2[0], goal2[1]);
        let goal_ring = if grid.passable(inp.profile, goal_cell) {
            0
        } else {
            if agent.goal_cell != goal_cell {
                agent.goal_cell = goal_cell;
                agent.goal_ring = Self::goal_ring(grid, inp.profile, goal_cell);
            }
            agent.goal_ring
        };
        let start_ring = if grid.passable(inp.profile, from_cell) {
            0
        } else {
            if agent.start_cell != from_cell {
                agent.start_cell = from_cell;
                agent.start_ring = Self::goal_ring(grid, inp.profile, from_cell);
            }
            agent.start_ring
        };
        // A pad sitting on a terrace is inside the straight-line band and every cell on that line
        // can still be a cliff. Driving it parks the mover on the floor under the pad. Take the road.
        let cliff = cliff_of(grid, inp.profile);
        let rise = (grid.surface_y(goal2[0], goal2[1]) - grid.surface_y(pos2[0], pos2[1])).abs();
        let too_steep = rise > d_goal * cliff + 1.5;
        let direct = !too_steep
            && (!on_grid
                || (d_goal <= s.direct_m
                    && Self::clear_line(grid, inp.profile, from_cell, goal_cell, start_ring, goal_ring)));

        let mut at_end_hold = false;
        if direct {
            self.stats.direct_calls += 1;
            if !agent.path.is_empty() {
                agent.clear_path();
            }
            agent.pending = false;
        } else {
            // Re-plan when there is no route, it was invalidated, or the goal moved.
            let moved = dist2(agent.path_goal, goal2);
            let tolerance = det_clamp(0.2 * d_goal, 5.0, 40.0);
            let fresh = agent.path_tick + s.repath_min_ticks <= tick;
            let backing = tick < agent.backoff_until;
            let mut want = false;
            if agent.path.is_empty() {
                // No route: ask once per goal, again when it moves, or after the re-plan interval.
                want = !agent.asked || moved > tolerance || fresh;
            } else if agent.invalid && !backing {
                want = true;
            } else if moved > tolerance && fresh {
                // A goal that moved within sight of the route's end (a formation slot, a mover
                // being followed) extends the route; anything else plans again.
                let n = agent.path.len();
                let prev = agent.path[n - 2];
                let end_cell = grid.cell_at(prev[0], prev[1]);
                if moved <= 60.0
                    && agent.idx < n
                    && grid.passable(inp.profile, end_cell)
                    && grid.los(inp.profile, end_cell, goal_cell, grid::COST_STEEP, goal_ring)
                {
                    agent.path[n - 1] = goal2;
                    agent.suffix[n - 2] = dist2(prev, goal2);
                    for k in (0..n - 2).rev() {
                        agent.suffix[k] = agent.suffix[k + 1] + dist2(agent.path[k], agent.path[k + 1]);
                    }
                    agent.path_goal = goal2;
                    agent.path_tick = tick;
                    self.stats.route_extended += 1;
                } else {
                    want = true;
                    self.stats.repath_goal += 1;
                }
            }
            if want && !agent.pending {
                let id = inp.id;
                self.enqueue(id, pos2, goal2);
                self.pump(tick);
            }
            if let (Some(grid), Some(agent)) = (self.grid.as_ref(), self.agents.get_mut(&inp.id))
                && agent.path.len() >= 2
            {
                self.stats.routed_calls += 1;
                out.routed = true;
                let f = follow(agent, grid, inp, tick, s.off_path_m);
                if f.off_path {
                    agent.invalid = true;
                    self.stats.repath_off_path += 1;
                }
                out.point = [f.point[0], inp.goal[1], f.point[1]];
                out.remaining = f.remaining;
                out.speed_scale = f.scale;
                out.corner_dist = f.corner_dist;
                out.corner_turn = f.corner_turn;
                if f.at_end {
                    // Final approach: straight at the true goal (contact with a yard or a site, or
                    // the last stretch the grid could not prove; stuck detection gives up on it).
                    out.point = inp.goal;
                    out.remaining = d_goal;
                }
                desired = norm([out.point[0] - pos2[0], out.point[2] - pos2[1]]);
            }
            // No route at all (a pocket the grid cannot prove a way out of): steer straight. Stuck
            // detection backs it off.
        }
        // A mover pressed against a solid stands in a blocked cell. Once the grind is real
        // (stuck, or in its back-off), aim straight at the nearest open cell and walk out of the
        // disc first. Merely standing in a blocked cell is normal yard work and keeps its own
        // steering.
        let grinding = {
            let agent = self.agents.get(&inp.id);
            agent.is_some_and(|a| a.stuck > 0) || tick < agent_backoff_until(&self.agents, inp.id)
        };
        if grinding
            && let Some(g) = self.grid.as_ref()
            && !g.passable(inp.profile, from_cell)
            && let Some(open) = g.nearest_passable(inp.profile, from_cell, ESCAPE_AIM_RINGS)
        {
            let c = g.center(open);
            let out_d = dist2(pos2, [c[0], c[1]]);
            out.point = [c[0], inp.goal[1], c[1]];
            out.remaining = det_min(out.remaining, out_d);
            out.routed = false;
        }
        if tick < agent_hold_until(&self.agents, inp.id) {
            at_end_hold = true;
        }

        // Stuck: no progress and no movement over the window. Pressed against the solid it was
        // sent to is not stuck: that contact is the job. A friend already on the slot is arrival,
        // not a wedge. A queue behind a friend who is closer to the goal is not counted; a real
        // wedge slides to a clear cell instead of standing still.
        let slot_taken = d_goal <= inp.park + 8.0 && self.slot_taken(inp.id, goal2);
        let traffic = self.traffic_ahead(inp, pos2, goal2);
        let Some(agent) = self.agents.get_mut(&inp.id) else {
            return out;
        };
        let metric = out.remaining;
        let at_solid_goal = goal_ring > 0 && d_goal <= inp.park + inp.radius + 16.0;
        // Jostling a few meters short of the goal is close enough: hold instead of backing off.
        let near_goal = d_goal <= inp.park + s.near_goal_m;
        let suppress_stuck = slot_taken
            || (near_goal
                && tick >= agent.prog_tick + s.stuck_ticks
                && dist2(pos2, agent.prog_pos) < s.stuck_move_m);
        let mut escape_now = false;
        if at_end_hold || at_solid_goal || suppress_stuck {
            agent.prog_tick = tick;
            agent.prog_pos = pos2;
            agent.prog_ref = metric;
            agent.still_tick = tick;
            agent.still_pos = pos2;
            agent.escape_open = false;
            agent.escape_to = None;
        } else {
            if agent.still_tick == 0 {
                agent.still_tick = tick;
                agent.still_pos = pos2;
            }
            let drifted = dist2(pos2, agent.still_pos);
            if drifted >= s.stuck_move_m {
                agent.still_tick = tick;
                agent.still_pos = pos2;
                agent.escape_open = false;
            } else {
                let still_for = tick.saturating_sub(agent.still_tick);
                if still_for > self.stats.max_still_ticks {
                    self.stats.max_still_ticks = still_for;
                }
            }
            let still_for = tick.saturating_sub(agent.still_tick);
            let moved = dist2(pos2, agent.prog_pos);
            let improved = agent.prog_ref - metric;
            let window = tick >= agent.prog_tick + s.stuck_ticks;
            let wedged = window && moved < s.stuck_move_m && improved < 2.0 && tick >= agent.backoff_until;
            // Only a stop with no progress slides; near the goal is work, not a wedge.
            let prog_age = tick.saturating_sub(agent.prog_tick);
            let no_progress = prog_age >= s.escape_nudge_ticks && (agent.prog_ref - metric) < 2.0;
            let nudge =
                !near_goal && no_progress && still_for >= s.escape_nudge_ticks && drifted < s.stuck_move_m;
            let force = !near_goal && still_for >= s.escape_still_ticks && drifted < s.stuck_move_m;
            if nudge || (wedged && !traffic) || force {
                escape_now = true;
                if !agent.escape_open {
                    agent.escape_open = true;
                    agent.invalid = true;
                    // A queue behind a friend is slid clear but not counted. A mover with nowhere
                    // to go counts once per wedge.
                    if wedged && !traffic {
                        agent.stuck = agent.stuck.saturating_add(1);
                        self.stats.stuck_events += 1;
                        self.stats.repath_stuck += 1;
                        if self.stats.stuck_log.len() < 2000 {
                            self.stats
                                .stuck_log
                                .push((tick, inp.id, pos2[0], pos2[1], out.routed, goal2[0], goal2[1]));
                        }
                        if agent.stuck >= s.give_up_stuck {
                            agent.stuck = 0;
                            self.stats.gave_up += 1;
                        }
                    }
                }
                agent.backoff_until = tick;
            } else if wedged && traffic {
                // Queued, and not yet due a slide: do not count, do not reverse.
                agent.backoff_until = tick;
                agent.prog_tick = tick;
                agent.prog_pos = pos2;
                agent.prog_ref = metric;
            } else if window {
                if moved >= s.stuck_move_m * 4.0 || improved >= 10.0 {
                    agent.stuck = 0;
                }
                agent.prog_tick = tick;
                agent.prog_pos = pos2;
                agent.prog_ref = metric;
            } else if metric < agent.prog_ref - 2.0 {
                agent.prog_ref = metric;
            }
        }
        if escape_now {
            let xz = self.escape_cell(inp, pos2, world);
            let point = [xz[0], 0.0, xz[1]];
            self.stats.escape_attempts += 1;
            out.escape = true;
            out.point = [point[0], inp.goal[1], point[2]];
            out.backoff = false;
            out.routed = false;
            out.speed_scale = 1.0;
            if let Some(agent) = self.agents.get_mut(&inp.id) {
                agent.escape_to = Some(point);
            }
        }
        let agent = self.agents.get(&inp.id);
        let backoff_side = agent
            .map(|a| if a.stuck % 2 == 0 { 1.0 } else { -1.0 })
            .unwrap_or(1.0);
        let backoff_until = agent.map(|a| a.backoff_until).unwrap_or(0);
        if !out.escape && tick < backoff_until {
            out.backoff = true;
            out.backoff_side = backoff_side;
        }
        if at_end_hold {
            out.arrived = true;
        }
        if let Some(agent) = self.agents.get(&inp.id)
            && tick < agent.depart_until
            && agent.depart != [0.0, 0.0]
            && !out.arrived
        {
            let d = agent.depart;
            let x = pos2[0] + d[0] * 14.0;
            let z = pos2[1] + d[1] * 14.0;
            let pass = self
                .grid
                .as_ref()
                .is_some_and(|g| g.in_bounds_world(x, z) && g.passable(inp.profile, g.cell_at(x, z)));
            if pass && Self::cell_clear([x, z], inp.id, inp.radius, world) {
                out.point = [x, inp.goal[1], z];
                out.routed = false;
                out.backoff = false;
                out.escape = false;
                out.speed_scale = 1.0;
            }
        }
        // The pit refusal: a steer point (or the ground halfway to it) on no-go ground aims the
        // mover into the shaft. Aim at the rim exit instead.
        if !out.arrived
            && !out.escape
            && let Some(exit) = self.pit_refusal(pos2, out.point)
        {
            out.point = exit;
            out.routed = false;
            out.backoff = false;
            self.stats.pit_guards += 1;
        }
        let (push, scale) = self.avoidance(inp, desired, out.arrived);
        out.avoid = push;
        out.speed_scale = det_clamp(out.speed_scale * scale, 0.25, 1.0);
        out
    }

    /// `None` when the steer point and the ground halfway there stay off the kill volume's no-go
    /// cells, else the rim exit to aim at instead.
    fn pit_refusal(&self, pos: [f32; 2], point: [f32; 3]) -> Option<[f32; 3]> {
        let vol = self.kill_volume?;
        let grid = self.grid.as_ref()?;
        let bad = |x: f32, z: f32| grid.in_bounds_world(x, z) && grid.excluded(grid.cell_at(x, z));
        let exit_m = self.settings.pit_exit_m;
        if bad(point[0], point[2]) {
            let exit = vol.rim_exit([point[0], point[2]], exit_m);
            return Some([exit[0], point[1], exit[1]]);
        }
        let mid = [(pos[0] + point[0]) * 0.5, (pos[1] + point[2]) * 0.5];
        if bad(mid[0], mid[1]) {
            let exit = vol.rim_exit(mid, exit_m);
            return Some([exit[0], point[1], exit[1]]);
        }
        None
    }

    /// A neighbour already stands on this goal. Holding short of them is arrival.
    fn slot_taken(&self, id: u32, goal: [f32; 2]) -> bool {
        self.nb.units.iter().any(|other| {
            if other.id == id || other.soft {
                return false;
            }
            let dx = other.x - goal[0];
            let dz = other.z - goal[1];
            // The follow target stands on the goal. That contact is the job.
            if dx * dx + dz * dz < 4.0 {
                return false;
            }
            let reach = other.r + 1.5;
            dx * dx + dz * dz < reach * reach
        })
    }

    /// A friend is ahead and closer to the goal. This mover is queued, not wedged.
    fn traffic_ahead(&self, inp: &SteerInput, pos: [f32; 2], goal: [f32; 2]) -> bool {
        let dx = goal[0] - pos[0];
        let dz = goal[1] - pos[1];
        let self_d = det_max(math::sqrt(dx * dx + dz * dz), 1e-3);
        let h = [dx / self_d, dz / self_d];
        self.nb.units.iter().any(|other| {
            if other.id == inp.id || other.soft {
                return false;
            }
            let ox = other.x - pos[0];
            let oz = other.z - pos[1];
            let d = math::sqrt(ox * ox + oz * oz);
            if !(0.05..=18.0).contains(&d) {
                return false;
            }
            let ahead = (h[0] * ox + h[1] * oz) / d;
            if ahead < 0.45 {
                return false;
            }
            let odx = goal[0] - other.x;
            let odz = goal[1] - other.z;
            math::sqrt(odx * odx + odz * odz) + 1.0 < self_d
        })
    }

    /// A passable cell a few meters away, never a retreat off the route. Clear cells closer to
    /// the goal win. The mover id picks among the nearest so two movers do not claim one cell.
    /// A 6 m step if the grid has nothing in range.
    fn escape_cell(&self, inp: &SteerInput, pos: [f32; 2], world: &dyn DetAgentWorld) -> [f32; 2] {
        let here = dist2(pos, [inp.goal[0], inp.goal[2]]);
        let goal_d = |c: [f32; 2]| dist2(c, [inp.goal[0], inp.goal[2]]);
        let Some(grid) = self.grid.as_ref() else {
            return self.escape_step(inp, pos, None);
        };
        let from = grid.cell_at(pos[0], pos[1]);
        let (fx, fz) = grid.xz(from);
        let (fx, fz) = (fx as i32, fz as i32);
        let (w, h) = (grid.width() as i32, grid.height() as i32);
        let cliff = cliff_of(grid, inp.profile);
        let (min_m, max_m) = escape_reach(inp, pos, self.settings.stuck_move_m);
        let mut clear: Vec<(i32, u32, [f32; 2])> = Vec::new();
        let mut open: Vec<(i32, u32, [f32; 2])> = Vec::new();
        // 5 m cells: ring 4 is the last that can sit inside the slide's reach.
        for ring in 1..=4 {
            for dz in -ring..=ring {
                for dx in -ring..=ring {
                    let (dx, dz): (i32, i32) = (dx, dz);
                    if dx.abs() != ring && dz.abs() != ring {
                        continue;
                    }
                    let (x, z) = (fx + dx, fz + dz);
                    if x < 0 || z < 0 || x >= w || z >= h {
                        continue;
                    }
                    let cell = (z * w + x) as u32;
                    if !grid.passable(inp.profile, cell) {
                        continue;
                    }
                    let c = grid.center(cell);
                    let travelled = dist2(pos, c);
                    if travelled < min_m || travelled > max_m {
                        continue;
                    }
                    if goal_d(c) > here + ESCAPE_RETREAT_M {
                        continue;
                    }
                    // A cell closer in XZ can be the next terrace down; the mover cannot climb back.
                    let y0 = grid.surface_y(pos[0], pos[1]);
                    let y1 = grid.surface_y(c[0], c[1]);
                    if (y1 - y0).abs() > travelled * cliff + 1.5 {
                        continue;
                    }
                    if self.escape_reserved(c, inp.id) {
                        continue;
                    }
                    if Self::cell_clear(c, inp.id, inp.radius, world) {
                        clear.push((ring, cell, c));
                    } else {
                        open.push((Self::clearance_cm(c, inp.id, world), cell, c));
                    }
                }
            }
        }
        if !clear.is_empty() {
            let closer: Vec<(i32, u32, [f32; 2])> = clear
                .iter()
                .copied()
                .filter(|c| goal_d(c.2) + 1.0 < here)
                .collect();
            let mut pool = if closer.is_empty() { clear } else { closer };
            pool.sort_by(|a, b| {
                goal_d(a.2)
                    .total_cmp(&goal_d(b.2))
                    .then(a.0.cmp(&b.0))
                    .then(a.1.cmp(&b.1))
            });
            let k = pool.len().min(4);
            let i = (inp.id as usize) % k;
            return pool[i].2;
        }
        if !open.is_empty() {
            open.sort_by(|a, b| {
                goal_d(a.2)
                    .total_cmp(&goal_d(b.2))
                    .then(b.0.cmp(&a.0))
                    .then(a.1.cmp(&b.1))
            });
            return open[0].2;
        }
        self.escape_step(inp, pos, Some(grid))
    }

    /// 6 m toward the goal, or to the side if that cell is the one that is passable. Stays inside
    /// the retreat cap when any candidate does.
    fn escape_step(&self, inp: &SteerInput, pos: [f32; 2], grid: Option<&NavGrid>) -> [f32; 2] {
        let dx = inp.goal[0] - pos[0];
        let dz = inp.goal[2] - pos[1];
        let len = det_max(math::sqrt(dx * dx + dz * dz), 1e-3);
        let fx = dx / len;
        let fz = dz / len;
        let here = len;
        let mut best_pass: Option<(f32, [f32; 2])> = None;
        let mut best_any: Option<(f32, [f32; 2])> = None;
        // Toward the goal first so a tie does not sidestep off the road.
        for side in [0.0_f32, -1.0, 1.0] {
            let c = if side == 0.0 {
                [pos[0] + fx * 6.0, pos[1] + fz * 6.0]
            } else {
                [pos[0] + fz * side * 6.0, pos[1] - fx * side * 6.0]
            };
            let gd = dist2(c, [inp.goal[0], inp.goal[2]]);
            if best_any.is_none_or(|(d, _)| gd < d) {
                best_any = Some((gd, c));
            }
            let pass = grid.is_none_or(|g| {
                if !g.in_bounds_world(c[0], c[1]) || !g.passable(inp.profile, g.cell_at(c[0], c[1])) {
                    return false;
                }
                let y0 = g.surface_y(pos[0], pos[1]);
                let y1 = g.surface_y(c[0], c[1]);
                (y1 - y0).abs() <= 6.0 * cliff_of(g, inp.profile) + 1.5
            });
            if pass && gd <= here + ESCAPE_RETREAT_M && best_pass.is_none_or(|(d, _)| gd < d) {
                best_pass = Some((gd, c));
            }
        }
        best_pass.or(best_any).map(|(_, c)| c).unwrap_or(pos)
    }

    /// Another mover already claimed this cell for its slide.
    fn escape_reserved(&self, center: [f32; 2], self_id: u32) -> bool {
        self.agents.iter().any(|(id, agent)| {
            *id != self_id
                && agent.escape_to.is_some_and(|p| {
                    let dx = p[0] - center[0];
                    let dz = p[2] - center[1];
                    dx * dx + dz * dz < 10.0 * 10.0
                })
        })
    }

    /// Centimeters to the nearest other mover or obstacle. Higher is clearer.
    fn clearance_cm(center: [f32; 2], self_id: u32, world: &dyn DetAgentWorld) -> i32 {
        let mut best = i32::MAX;
        for id in world.ids() {
            if id == self_id {
                continue;
            }
            let Some(other) = world.body(id) else {
                continue;
            };
            if other.profile.is_none() && world.obstacle(id).is_none() {
                continue;
            }
            let p = other.pos;
            let dx = center[0] - p[0];
            let dz = center[1] - p[2];
            let cm = (math::sqrt(dx * dx + dz * dz) * 100.0) as i32;
            if cm < best {
                best = cm;
            }
        }
        best
    }

    fn cell_clear(center: [f32; 2], self_id: u32, self_r: f32, world: &dyn DetAgentWorld) -> bool {
        world.ids().into_iter().all(|id| {
            if id == self_id {
                return true;
            }
            let Some(other) = world.body(id) else {
                return true;
            };
            if other.profile.is_none() && world.obstacle(id).is_none() {
                return true;
            }
            let p = other.pos;
            let dx = center[0] - p[0];
            let dz = center[1] - p[2];
            let need = self_r + other.radius + 0.6;
            dx * dx + dz * dz >= need * need
        })
    }

    /// Contact separation plus the reciprocal velocity-obstacle solver
    /// ([`regia_detsim::avoid`]). Discs in contact push apart positionally; every other
    /// neighbour is solved by velocity.
    fn avoidance(&mut self, inp: &SteerInput, desired: [f32; 2], parked: bool) -> ([f32; 2], f32) {
        let nb = &self.nb;
        if !nb.valid || nb.units.is_empty() {
            return ([0.0, 0.0], 1.0);
        }
        let (bx, b_z) = nb.bucket(inp.pos[0], inp.pos[1]);
        let mut push = [0.0f32, 0.0f32];
        let h = if desired == [0.0, 0.0] {
            [math::sin(inp.yaw), math::cos(inp.yaw)]
        } else {
            desired
        };
        let gap = self.settings.separation_gap_m;
        let mut bodies: Vec<avoid::OrcaBody> = Vec::new();
        for dz in -1i32..=1 {
            for dx in -1i32..=1 {
                let (x, z) = (bx as i32 + dx, b_z as i32 + dz);
                if x < 0 || z < 0 || x >= nb.side as i32 || z >= nb.side as i32 {
                    continue;
                }
                let b = z as usize * nb.side + x as usize;
                for k in nb.start[b]..nb.start[b + 1] {
                    let other = &nb.units[nb.order[k as usize] as usize];
                    if other.id == inp.id || (inp.hull && other.soft) || (other.y - inp.y).abs() > 6.0 {
                        continue;
                    }
                    // The body being followed or attacked stands on the goal: drive up to it.
                    let gx = other.x - inp.goal[0];
                    let gz = other.z - inp.goal[2];
                    if gx * gx + gz * gz < 1.0 {
                        continue;
                    }
                    let ox = inp.pos[0] - other.x;
                    let oz = inp.pos[1] - other.z;
                    let d = math::sqrt(ox * ox + oz * oz);
                    let reach = inp.radius + other.r + gap;
                    if d < reach {
                        // In contact: push apart along the line of centers.
                        let n = if d > 1e-3 {
                            [ox / d, oz / d]
                        } else if inp.id < other.id {
                            [1.0, 0.0]
                        } else {
                            [-1.0, 0.0]
                        };
                        let w = (reach - d) / reach;
                        push[0] += n[0] * w * 1.5;
                        push[1] += n[1] * w * 1.5;
                        // A stopped mover pressed against a standing neighbour has no relative
                        // velocity for the solver to grip, so the push and the drive cancel. An
                        // en-route mover slides round the standing disc on its own right.
                        if !parked && !other.moving {
                            push[0] += n[1] * w * 0.8;
                            push[1] += -n[0] * w * 0.8;
                        }
                    }
                    if parked || d >= reach + avoid::ORCA_LOOK_M {
                        continue;
                    }
                    bodies.push(avoid::OrcaBody {
                        id: other.id,
                        x_mm: avoid::m_to_mm(other.x),
                        z_mm: avoid::m_to_mm(other.z),
                        vx_mm_s: avoid::m_to_mm(other.vx),
                        vz_mm_s: avoid::m_to_mm(other.vz),
                        r_mm: avoid::m_to_mm(other.r),
                    });
                }
            }
        }
        let mut scale = 1.0f32;
        if !bodies.is_empty() {
            let pref = [h[0] * inp.speed, h[1] * inp.speed];
            let q = [avoid::m_to_mm(pref[0]), avoid::m_to_mm(pref[1])];
            if q[0] * q[0] + q[1] * q[1] >= avoid::ORCA_MIN_PREFERRED_MM_S * avoid::ORCA_MIN_PREFERRED_MM_S {
                let me = avoid::OrcaBody {
                    id: inp.id,
                    x_mm: avoid::m_to_mm(inp.pos[0]),
                    z_mm: avoid::m_to_mm(inp.pos[1]),
                    vx_mm_s: avoid::m_to_mm(inp.vel[0]),
                    vz_mm_s: avoid::m_to_mm(inp.vel[1]),
                    r_mm: avoid::m_to_mm(inp.radius),
                };
                let v = avoid::orca_velocity(&me, q, &bodies);
                self.stats.orca_calls += 1;
                let sp = math::sqrt(pref[0] * pref[0] + pref[1] * pref[1]);
                if sp > 1e-3 {
                    let vx = avoid::mm_s_to_m_s(v[0]);
                    let vz = avoid::mm_s_to_m_s(v[1]);
                    let sn = math::sqrt(vx * vx + vz * vz);
                    scale = det_clamp(sn / sp, 0.25, 1.0);
                    push[0] += (vx - pref[0]) / sp;
                    push[1] += (vz - pref[1]) / sp;
                }
            }
        }
        let len = math::sqrt(push[0] * push[0] + push[1] * push[1]);
        if len > 1.5 {
            push = [push[0] / len * 1.5, push[1] / len * 1.5];
        }
        (push, scale)
    }
}

fn agent_hold_until(agents: &BTreeMap<u32, Agent>, id: u32) -> u64 {
    agents.get(&id).map(|a| a.hold_until).unwrap_or(0)
}

fn agent_backoff_until(agents: &BTreeMap<u32, Agent>, id: u32) -> u64 {
    agents.get(&id).map(|a| a.backoff_until).unwrap_or(0)
}

struct Follow {
    point: [f32; 2],
    remaining: f32,
    scale: f32,
    corner_dist: f32,
    corner_turn: f32,
    at_end: bool,
    off_path: bool,
}

/// Advance along the waypoints and pick the look-ahead point.
fn follow(agent: &mut Agent, grid: &NavGrid, inp: &SteerInput, tick: u64, off_path_m: f32) -> Follow {
    let pos = inp.pos;
    let n = agent.path.len();
    let reach = det_max(inp.radius + 1.0, 4.0);
    if agent.idx == 0 {
        agent.idx = 1;
    }
    while agent.idx < n - 1 {
        let w = agent.path[agent.idx];
        let prev = agent.path[agent.idx - 1];
        let seg = [w[0] - prev[0], w[1] - prev[1]];
        let len2 = seg[0] * seg[0] + seg[1] * seg[1];
        let t = if len2 > 1e-6 {
            ((pos[0] - prev[0]) * seg[0] + (pos[1] - prev[1]) * seg[1]) / len2
        } else {
            1.0
        };
        if dist2(pos, w) < reach || t >= 1.0 {
            agent.idx += 1;
        } else {
            break;
        }
    }
    // Skip ahead when a later waypoint is already in clear sight (after a push).
    if (tick + u64::from(inp.id)).is_multiple_of(10) {
        let here = grid.cell_at(pos[0], pos[1]);
        let mut hops = 0;
        while agent.idx < n - 1 && hops < 3 {
            let next = agent.path[agent.idx + 1];
            let cell = grid.cell_at(next[0], next[1]);
            if grid.passable(inp.profile, here) && grid.los(inp.profile, here, cell, 30, 0) {
                agent.idx += 1;
                hops += 1;
                // The new leg starts where the mover is.
                agent.path[agent.idx - 1] = pos;
            } else {
                break;
            }
        }
    }
    let idx = agent.idx.min(n - 1);
    let w = agent.path[idx];
    let prev = agent.path[idx - 1];
    // Closest point on the current leg.
    let seg = [w[0] - prev[0], w[1] - prev[1]];
    let len2 = seg[0] * seg[0] + seg[1] * seg[1];
    let t = if len2 > 1e-6 {
        det_clamp(
            ((pos[0] - prev[0]) * seg[0] + (pos[1] - prev[1]) * seg[1]) / len2,
            0.0,
            1.0,
        )
    } else {
        1.0
    };
    let q = [prev[0] + seg[0] * t, prev[1] + seg[1] * t];
    let off = dist2(pos, q);
    // Look ahead along the route from the closest point.
    let look = det_clamp(inp.speed * 0.5, 6.0, 20.0);
    let mut left = look;
    let mut point;
    let mut k = idx;
    let mut from = q;
    loop {
        let to = agent.path[k];
        let leg = dist2(from, to);
        if leg >= left {
            let f = if leg > 1e-6 { left / leg } else { 0.0 };
            point = [from[0] + (to[0] - from[0]) * f, from[1] + (to[1] - from[1]) * f];
            break;
        }
        left -= leg;
        point = to;
        from = to;
        if k + 1 >= n {
            break;
        }
        k += 1;
    }
    // Never aim across a corner the mover cannot see past.
    let here = grid.cell_at(pos[0], pos[1]);
    let aim = grid.cell_at(point[0], point[1]);
    if !(grid.passable(inp.profile, here) && grid.los(inp.profile, here, aim, 40, 0)) {
        point = w;
    }
    let remaining = dist2(pos, w) + agent.suffix[idx];
    // Slow into a corner.
    let mut scale = 1.0f32;
    let mut corner_turn = 0.0f32;
    let corner_dist = dist2(pos, w);
    if idx + 1 < n {
        let a = norm([w[0] - prev[0], w[1] - prev[1]]);
        let b = norm([agent.path[idx + 1][0] - w[0], agent.path[idx + 1][1] - w[1]]);
        let cos = a[0] * b[0] + a[1] * b[1];
        let cross = a[0] * b[1] - a[1] * b[0];
        corner_turn = math::atan2(cross, cos).abs();
        let window = det_max(inp.speed * 1.2, 15.0);
        if corner_dist < window {
            scale = 0.4 + 0.6 * (1.0 + cos) * 0.5;
        }
    }
    let at_end = idx == n - 1 && dist2(pos, w) < reach;
    Follow {
        point,
        remaining,
        scale,
        corner_dist,
        corner_turn,
        at_end,
        off_path: off > off_path_m,
    }
}

/// The default [`DetMover`]: the source's kinematic step. Full cruise after a short ramp, nose
/// along the travel, along the route instead of the straight line, slowed into corners, pushed
/// clear of neighbours, backing off when stuck. The wedge slide is not applied here: it lands in
/// [`DetAgents::commit_escapes`] after the host's collisions.
pub struct KinematicMover<'a, W: DetAgentWorldMut> {
    /// The agent layer.
    pub agents: &'a mut DetAgents,
    /// The host's world.
    pub world: &'a mut W,
    /// The current tick.
    pub tick: u64,
}

impl<'a, W: DetAgentWorldMut> KinematicMover<'a, W> {
    /// A mover for one tick.
    pub fn new(agents: &'a mut DetAgents, world: &'a mut W, tick: u64) -> Self {
        KinematicMover { agents, world, tick }
    }
}

impl<W: DetAgentWorldMut> DetMover for KinematicMover<'_, W> {
    fn move_kinematic(&mut self, id: u32, goal: [f32; 3], park: f32) -> bool {
        let tick = self.tick;
        let steer = self.agents.steer(id, goal, park, tick, &*self.world);
        let dt = self.agents.settings.tick_dt;
        let Some(body) = self.world.body(id) else {
            return false;
        };
        let pos = body.pos;
        let yaw0 = body.yaw;
        let vmax = body.cruise_m_s;
        if steer.arrived || vmax <= 0.0 {
            if let Some(agent) = self.agents.agents.get_mut(&id) {
                agent.speed = 0.0;
            }
            return steer.arrived;
        }
        let room = det_max(steer.remaining - park, 0.0);
        let (mut mx, mut mz, heading, speed);
        if steer.backoff {
            // Back away from what it is stuck on, sliding to one side.
            let (fx, fz) = (math::sin(yaw0), math::cos(yaw0));
            let back = vmax * 0.35 * dt;
            mx = -fx * back + fz * back * 0.5 * steer.backoff_side;
            mz = -fz * back - fx * back * 0.5 * steer.backoff_side;
            heading = [fx, fz];
            speed = 0.0;
        } else {
            let mut dir = norm([steer.point[0] - pos[0], steer.point[2] - pos[2]]);
            if dir == [0.0, 0.0] {
                dir = [math::sin(yaw0), math::cos(yaw0)];
            }
            let steered = norm([dir[0] + steer.avoid[0] * 0.5, dir[1] + steer.avoid[1] * 0.5]);
            if steered != [0.0, 0.0] {
                dir = steered;
            }
            // Cruise is the unboosted speed, and the mover ramps up from its last speed.
            let cruise = if vmax > KIN_TURBO_FOLDED_ABOVE_M_S {
                vmax * KIN_CRUISE_FRACTION
            } else {
                vmax
            };
            let prev = self.agents.agents.get(&id).map(|a| a.speed).unwrap_or(0.0);
            let ramp = det_max(cruise * KIN_ACCEL_PER_S, KIN_ACCEL_MIN_M_S2) * dt;
            let v = det_min(cruise * steer.speed_scale, det_max(prev, 0.0) + ramp);
            let step = det_min(v * dt, room);
            mx = dir[0] * step;
            mz = dir[1] * step;
            heading = dir;
            // The speed the neighbours' solver reads is the speed actually driven.
            speed = if dt > 0.0 { det_min(step / dt, v) } else { v };
        }
        let nudge = vmax * 0.25 * dt;
        mx += steer.avoid[0] * nudge;
        mz += steer.avoid[1] * nudge;
        // Never step deeper into the kill volume: a predicted step inside it (below the margin
        // plane) that does not leave it is refused outright.
        if let Some(vol) = self.agents.kill_volume
            && vol.claims(pos[0] + mx, pos[2] + mz, pos[1])
        {
            let (ox, oz) = (pos[0] - vol.center_x, pos[2] - vol.center_z);
            let (nx, nz) = (pos[0] + mx - vol.center_x, pos[2] + mz - vol.center_z);
            if nx * nx + nz * nz <= ox * ox + oz * oz + 1e-6 {
                if let Some(agent) = self.agents.agents.get_mut(&id) {
                    agent.speed = 0.0;
                }
                return false;
            }
        }
        self.agents.stats.kinematic_moves += 1;
        self.world.set_position(id, [pos[0] + mx, pos[1], pos[2] + mz]);
        if !steer.backoff && mx * mx + mz * mz > 1e-8 {
            self.world.set_yaw(id, math::atan2(heading[0], heading[1]));
        }
        if let Some(agent) = self.agents.agents.get_mut(&id) {
            agent.speed = speed;
            agent.heading = heading;
            agent.moved_tick = tick;
        }
        false
    }
}

#[cfg(test)]
mod tests;
