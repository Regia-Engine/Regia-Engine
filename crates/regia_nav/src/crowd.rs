// SPDX-License-Identifier: Zlib
// Portions ported from Recast & Detour (https://github.com/recastnavigation/recastnavigation,
// commit 9f4ce64458): DetourCrowd/Source/DetourCrowd.cpp (the update order, separation, the
// acceleration clamp, collision resolution, turn anticipation), DetourObstacleAvoidance.cpp (the
// adaptive velocity-sampling pattern and the sample penalty: desired/current velocity, side bias,
// time to impact) and DetourPathCorridor.cpp (corridor merging after a move and after a visibility
// shortcut). Rewritten in Rust for Regia Engine; the structure, names and comments are new.
// Copyright (c) 2009-2010 Mikko Mononen memon@inside.org, zlib licence.
// Altered from the original: this is not the original software.
//
// This software is provided 'as-is', without any express or implied
// warranty.  In no event will the authors be held liable for any damages
// arising from the use of this software.
// Permission is granted to anyone to use this software for any purpose,
// including commercial applications, and to alter it and redistribute it
// freely, subject to the following restrictions:
// 1. The origin of this software must not be misrepresented; you must not
//    claim that you wrote the original software. If you use this software
//    in a product, an acknowledgment in the product documentation would be
//    appreciated but is not required.
// 2. Altered source versions must be plainly marked as such, and must not be
//    misrepresented as being the original software.
// 3. This notice may not be removed or altered from any source distribution.

//! Crowd steering (`docs/specs/22-tier2.md` § T2.2, ADR-1484): many agents following navmesh
//! corridors with local avoidance, advanced by a deterministic fixed step.
//!
//! # One step
//!
//! [`NavCrowd::step`] runs these phases, each over every agent in id order before the next
//! phase starts (so the result never depends on iteration tricks or threads):
//!
//! 1. **Corridor check.** An agent's corridor is the polygon sequence from its current polygon to
//!    its goal. A corridor whose first polygons no longer exist or no longer connect (the mesh
//!    changed) is replanned with [`NavQuery::find_path`].
//! 2. **Neighbours and walls.** Each agent collects up to `MAX_CROWD_NEIGHBOURS` nearby agents
//!    (closest first) and up to 8 nearby wall segments (polygon edges without a portal).
//! 3. **Corners.** The corridor is string-pulled from the agent's position to find the next few
//!    corners. With `optimize_visibility`, a straight walk toward the corner after next that
//!    stays on the mesh replaces the start of the corridor (a visibility shortcut).
//! 4. **Steering.** The desired velocity points at the next corner (or swings wide before a turn
//!    with `anticipate_turns`), slows down near the goal, and is pushed away from close
//!    neighbours (separation).
//! 5. **Velocity planning.** With `avoidance`, candidate velocities are sampled in rings around
//!    the desired velocity and scored; the cheapest wins. A candidate costs more the further it
//!    strays from the desired and the current velocity, the sooner it would hit a neighbour or a
//!    wall within the time horizon, and the more it passes neighbours on the wrong side. The
//!    search is refined around the best candidate with a halving radius.
//! 6. **Integration.** The velocity moves toward the planned one, limited by the maximum
//!    acceleration, and the position advances by one fixed step.
//! 7. **Collision resolution.** Overlapping agents are pushed apart over a few iterations.
//! 8. **Surface constraint.** The new position is walked across the corridor's portals and
//!    clamped at walls, so agents never leave the mesh; the corridor start follows the walk.
//!
//! Off-mesh links (ADR-1821): once an agent is within `2.25 * radius` of a link's take-off point
//! it enters [`CrowdAgentState::OffMeshLink`] and moves to the take-off point, then in a straight
//! line to the landing, over whole fixed steps (the link's `duration`, or its length over the
//! agent's `max_speed`). It does not steer, push or get pushed meanwhile. [`NavCrowd::link_events`] reports each start and
//! finish so gameplay can play the jump, climb or teleport. [`NavLinkTraversalMode::Instant`]
//! crosses in the same step instead. An agent only plans over the links its
//! [`CrowdAgentParams::link_filter`] lets through. With [`NavCrowdConfig::link_arcs`] (ADR-2589),
//! Jump links are crossed along a ballistic arc and Drop links along a falling parabola, over the
//! arc's flight time; [`NavLinkTraversal::arc`] carries the path for animation.
//!
//! Areas (ADR-1934): [`CrowdAgentParams::link_filter`] also sets what the agent pays per area and
//! which areas it leaves out. A left-out area is a wall to the agent: avoidance sees its edge and
//! the surface constraint stops there. A visibility shortcut never crosses ground dearer than the
//! corridor it replaces.
//!
//! Streaming (spec 78 § 13.2): [`NavCrowd::sync_streaming`] puts an agent standing on a tile that
//! left the world in [`CrowdAgentState::Dormant`]: it keeps its target, and nothing moves or
//! pushes it until the tile is back, when it replans.
//!
//! The crowd keeps polygon indices, so after the mesh changes (a [`crate::NavTileCache`] update
//! that rebuilt tiles) call [`NavCrowd::sync_mesh_revision`] or [`NavCrowd::request_replan_all`];
//! [`NavCrowdPlugin`] does this for you.
//!
//! Formation followers (ADR-8358): an agent may carry a [`CrowdGroup`]. The leader still follows
//! its corridor. Each follower steers toward the leader plus a facing-relative slot offset,
//! projected onto the mesh, matching the leader's speed and catching up when the slot is ahead.
//! When that slot is off the mesh, or the leader's corridor is narrower than the shape, the
//! follower queues in a column behind the leader and returns to the slot once the corridor is
//! wide enough.

use regia_app::prelude::*;
use regia_core::math::{Vec3, det_atan2, det_sin_cos};
use regia_scene::streaming::OriginShifted;
use thiserror::Error;

use crate::agent::NavAgentState;
use crate::areas::area_bit;
use crate::links::{LinkTable, NavLinkArc, NavLinkArcConfig, NavLinkKind, NavQueryFilter};
use crate::mesh::{NavMesh, OffMeshLink};
use crate::query::{NavQuery, find_nearest_allowed, find_nearest_on, funnel_into, portal_between};
use crate::tile_cache::{NavPathOutcome, NavTileCache};

/// Catch-up gain for a formation follower: metres per second added per metre the slot is ahead.
pub const FORMATION_CATCH_UP: f32 = 1.0;
/// A projected slot farther than this from the mesh (horizontal) puts that follower in a column.
pub const FORMATION_OFF_MESH: f32 = 0.25;
/// Horizontal speed at which a leader's velocity becomes the formation heading.
pub const FORMATION_HEADING_SPEED: f32 = 0.15;

/// The most neighbours one agent considers for separation and avoidance.
pub const MAX_CROWD_NEIGHBOURS: usize = 6;
/// The most wall segments one agent considers for avoidance.
const MAX_LOCAL_SEGMENTS: usize = 8;
/// The corners kept per agent (the next corner, the one after it for turn anticipation and
/// visibility shortcuts, and one spare so the goal shows up early for slowing down).
const MAX_CORNERS: usize = 3;
/// How many corridor polygons the per-step validity check looks at.
const CORRIDOR_LOOKAHEAD: usize = 8;
/// The most polygons one surface walk may cross.
const MAX_WALK_POLYS: usize = 32;
/// Collision-resolution iterations per step.
const COLLISION_ITERATIONS: usize = 4;
/// Pairwise overlap-removal iterations after the soft collision pass.
const HARD_COLLISION_ITERATIONS: usize = 24;
/// How much of the overlap one collision iteration removes (shared between the two agents).
const COLLISION_RESOLVE_FACTOR: f32 = 0.7;
/// Corners closer than this to the agent are skipped (it is standing on them).
const MIN_CORNER_DISTANCE: f32 = 0.01;
/// The most agents a crowd may hold.
pub const MAX_CROWD_AGENTS: usize = 65_536;

/// The largest `max_steps_per_advance` a [`NavCrowdConfig`] (and a lane crowd's config) accepts:
/// one minute of 60 Hz steps, the agent protocol's per-call cap. A larger value would let one long
/// frame delta (a debugger break, a suspended host) run millions of full steps in one call.
pub const MAX_STEPS_PER_ADVANCE: u32 = 3600;

/// The movement parameters of one crowd agent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrowdAgentParams {
    /// Collision radius (metres, `> 0`).
    pub radius: f32,
    /// Height (metres, `> 0`); agents only interact when their vertical extents overlap.
    pub height: f32,
    /// Top speed (metres per second, `>= 0`).
    pub max_speed: f32,
    /// Largest velocity change per second (metres per second squared, `>= 0`).
    pub max_acceleration: f32,
    /// How far the agent looks for neighbours and walls (metres, `> 0`; default `12 * radius`).
    pub collision_query_range: f32,
    /// The longest visibility shortcut (metres, `>= 0`; default `30 * radius`).
    pub path_optimization_range: f32,
    /// Strength of the separation push (`>= 0`; default 2).
    pub separation_weight: f32,
    /// Sample velocities to avoid neighbours and walls.
    pub avoidance: bool,
    /// Push away from close neighbours.
    pub separation: bool,
    /// Take visibility shortcuts along the corridor.
    pub optimize_visibility: bool,
    /// Swing wide before turns instead of heading straight at the next corner.
    pub anticipate_turns: bool,
    /// What this agent may use and what it pays: the off-mesh links it may plan over and cross,
    /// and its area costs and left-out areas (default: everything, at the mesh's costs). Change it
    /// with [`NavCrowd::set_agent_params`], which replans the agent.
    pub link_filter: NavQueryFilter,
}

impl CrowdAgentParams {
    /// Parameters with the given size and movement limits and every behaviour enabled; the query
    /// and shortcut ranges scale with the radius.
    pub fn new(radius: f32, height: f32, max_speed: f32, max_acceleration: f32) -> Self {
        Self {
            radius,
            height,
            max_speed,
            max_acceleration,
            collision_query_range: radius * 12.0,
            path_optimization_range: radius * 30.0,
            separation_weight: 2.0,
            avoidance: true,
            separation: true,
            optimize_visibility: true,
            anticipate_turns: true,
            link_filter: NavQueryFilter::default(),
        }
    }

    /// Checks every field: finite, radius and height and query range positive, the rest
    /// non-negative.
    pub fn validate(&self) -> Result<(), NavCrowdError> {
        let all = [
            self.radius,
            self.height,
            self.max_speed,
            self.max_acceleration,
            self.collision_query_range,
            self.path_optimization_range,
            self.separation_weight,
        ];
        if !all.iter().all(|v| v.is_finite()) {
            return Err(NavCrowdError::InvalidAgentParams(
                "every agent parameter must be finite",
            ));
        }
        if self.radius <= 0.0 {
            return Err(NavCrowdError::InvalidAgentParams("radius must be positive"));
        }
        if self.height <= 0.0 {
            return Err(NavCrowdError::InvalidAgentParams("height must be positive"));
        }
        if self.collision_query_range <= 0.0 {
            return Err(NavCrowdError::InvalidAgentParams(
                "collision_query_range must be positive",
            ));
        }
        if self.max_speed < 0.0
            || self.max_acceleration < 0.0
            || self.path_optimization_range < 0.0
            || self.separation_weight < 0.0
        {
            return Err(NavCrowdError::InvalidAgentParams(
                "max_speed, max_acceleration, path_optimization_range and separation_weight must not be negative",
            ));
        }
        Ok(())
    }
}

impl Default for CrowdAgentParams {
    fn default() -> Self {
        Self::new(0.35, 1.6, 3.5, 8.0)
    }
}

/// Weights and sampling pattern of the velocity planner (shared by every agent of a crowd).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AvoidanceParams {
    /// How much of the desired velocity the sampling pattern is centred on (`0..=1`).
    pub velocity_bias: f32,
    /// Cost of straying from the desired velocity.
    pub weight_desired: f32,
    /// Cost of straying from the current velocity.
    pub weight_current: f32,
    /// Cost of passing a neighbour on the wrong side.
    pub weight_side: f32,
    /// Cost of an early impact.
    pub weight_impact: f32,
    /// How far ahead impacts count (seconds, `> 0`).
    pub horizon: f32,
    /// Samples per ring (`1..=32`).
    pub divisions: u32,
    /// Rings of samples (`1..=4`).
    pub rings: u32,
    /// Refinement passes, each halving the ring radius (`1..=8`).
    pub depth: u32,
}

impl Default for AvoidanceParams {
    fn default() -> Self {
        Self {
            velocity_bias: 0.4,
            weight_desired: 2.0,
            weight_current: 0.75,
            weight_side: 0.75,
            weight_impact: 2.5,
            horizon: 2.5,
            divisions: 7,
            rings: 2,
            depth: 5,
        }
    }
}

impl AvoidanceParams {
    fn validate(&self) -> Result<(), NavCrowdError> {
        let weights = [
            self.velocity_bias,
            self.weight_desired,
            self.weight_current,
            self.weight_side,
            self.weight_impact,
            self.horizon,
        ];
        if !weights.iter().all(|v| v.is_finite() && *v >= 0.0) {
            return Err(NavCrowdError::InvalidConfig(
                "avoidance weights must be finite and non-negative",
            ));
        }
        if self.horizon <= 0.0 {
            return Err(NavCrowdError::InvalidConfig("avoidance horizon must be positive"));
        }
        if self.velocity_bias > 1.0 {
            return Err(NavCrowdError::InvalidConfig("velocity_bias must be within 0..=1"));
        }
        if !(1..=32).contains(&self.divisions)
            || !(1..=4).contains(&self.rings)
            || !(1..=8).contains(&self.depth)
        {
            return Err(NavCrowdError::InvalidConfig(
                "avoidance pattern needs divisions 1..=32, rings 1..=4, depth 1..=8",
            ));
        }
        Ok(())
    }
}

/// Crowd-wide settings.
#[derive(Debug, Clone, PartialEq)]
pub struct NavCrowdConfig {
    /// The fixed step (seconds, `0 < step <= 1`). Every [`NavCrowd::step`] advances exactly this.
    pub step_seconds: f32,
    /// The most agents (`1..=MAX_CROWD_AGENTS`).
    pub max_agents: usize,
    /// The most fixed steps one [`NavCrowd::advance`] runs (`0..=MAX_STEPS_PER_ADVANCE`);
    /// leftover time is dropped.
    pub max_steps_per_advance: u32,
    /// Half-extents of the box used to snap agents and targets to the mesh.
    pub snap_extents: Vec3,
    /// The velocity planner.
    pub avoidance: AvoidanceParams,
    /// How agents cross off-mesh links (the runtime switch of ADR-1821).
    pub link_traversal: NavLinkTraversalMode,
    /// Link reservation (ADR-2420): one agent per link at a time, the others queue. Off by
    /// default, so a crowd without it steps exactly as before.
    pub link_queue: NavLinkQueueConfig,
    /// Ballistic Jump and Drop crossings (ADR-2589). Off by default, so a crowd without it steps
    /// exactly as before.
    pub link_arcs: NavLinkArcConfig,
    /// Stuck detection (NM-2): a walking agent that has not come `stuck_min_progress_m` closer
    /// to its next corner for this many fixed steps is stuck, and climbs the recovery ladder
    /// (re-path, nudge, report: [`NavStuckAction`]). 0 (the default) turns it off, so a crowd
    /// without it steps exactly as before.
    pub stuck_window_steps: u32,
    /// The approach toward the next corner (metres, finite, `> 0`; default 0.5) that counts as
    /// progress for [`Self::stuck_window_steps`]. Two agents jostling in a wedge swing about a
    /// quarter metre back and forth, so keep it well above that.
    pub stuck_min_progress_m: f32,
}

/// The default [`NavCrowdConfig::stuck_min_progress_m`].
pub const DEFAULT_STUCK_MIN_PROGRESS_M: f32 = 0.5;

/// Link reservation and queuing (ADR-2420). With `exclusive`, an off-mesh link carries one agent
/// at a time (a ladder, a narrow drop): an agent reaching a take-off point while another crosses
/// the link, or while others already wait for it, waits there as [`CrowdAgentState::LinkQueued`].
/// The queue is first come, first served: agents arriving in the same step queue in agent order.
/// Both directions of a bidirectional link share one queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NavLinkQueueConfig {
    /// One agent per link at a time.
    pub exclusive: bool,
    /// The most fixed steps an agent waits; then it gives up as
    /// [`CrowdAgentState::LinkTimedOut`] with a [`NavLinkPhase::TimedOut`] event (protocol code
    /// `nav_link_queue_timeout`). 0 waits for ever.
    pub timeout_steps: u32,
}

/// How crowd agents cross off-mesh links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NavLinkTraversalMode {
    /// Over whole fixed steps (the link's duration), as [`CrowdAgentState::OffMeshLink`].
    #[default]
    Timed,
    /// In the step the agent reaches the take-off point (a teleport).
    Instant,
}

/// The `link` index a crossing, a link event or a queue place reports once its link has left the
/// mesh (a streamed tile unloaded, or the link list was replaced without it). The crossing itself
/// goes on to its landing; nothing else can take that index.
pub const NAV_LINK_GONE: usize = usize::MAX;

/// The most fixed steps one link traversal may take (longer durations are clamped).
pub const MAX_LINK_TRAVERSAL_STEPS: u32 = 1 << 20;

/// One agent's crossing of an off-mesh link. With [`NavCrowdPlugin`] it is also a message: one
/// is written at every take-off (read it in `FixedUpdate` after [`NavCrowdStep`]) so gameplay can
/// start the jump, drop or climb animation from its arc and duration.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct NavLinkTraversal {
    /// The link's index in `NavMesh::off_mesh_links`. It follows the link when the list changes
    /// under the crossing (a streamed tile loading or unloading shifts later links), and is
    /// [`NAV_LINK_GONE`] once the link has left the mesh.
    pub link: usize,
    /// The link's user id.
    pub user_id: u64,
    /// The link's kind.
    pub kind: NavLinkKind,
    /// Where the agent stood when it started (within `2.25 * radius` of `start`).
    pub origin: Vec3,
    /// The link's take-off point.
    pub start: Vec3,
    /// The landing point, on the landing polygon.
    pub end: Vec3,
    /// Fixed steps done so far.
    pub steps: u32,
    /// Fixed steps the crossing takes (`>= 1`).
    pub total_steps: u32,
    /// The crowd's fixed step in seconds (so `elapsed` and `duration` need no config).
    pub step_seconds: f32,
    /// The ballistic path of a Jump or Drop crossing when [`NavCrowdConfig::link_arcs`] is on;
    /// `None` for a straight crossing.
    pub arc: Option<NavLinkArc>,
    /// For an arc crossing, the seconds of the short walk from `origin` to `start` before the
    /// take-off (at the agent's `max_speed`); 0 otherwise.
    pub walk_seconds: f32,
}

impl NavLinkTraversal {
    /// The share of the crossing done, `0..=1`.
    pub fn progress(&self) -> f32 {
        (self.steps as f32 / self.total_steps.max(1) as f32).clamp(0.0, 1.0)
    }

    /// Where the crossing puts the agent now: on the short walk from `origin` to `start`, then on
    /// the link, at one speed over the whole crossing. An arc crossing walks for `walk_seconds`
    /// and then follows the arc; the whole crossing is stretched onto its whole steps (by less
    /// than one step).
    pub fn position(&self) -> Vec3 {
        if let Some(arc) = &self.arc {
            let t = self.crossing_seconds() * self.progress();
            if t < self.walk_seconds && self.walk_seconds > 0.0 {
                return self.origin + (self.start - self.origin) * (t / self.walk_seconds);
            }
            return arc.point_at(t - self.walk_seconds);
        }
        let walk = (self.start - self.origin).length();
        let link = (self.end - self.start).length();
        let along = (walk + link) * self.progress();
        if along < walk {
            self.origin + (self.start - self.origin) * (along / walk)
        } else if link > 0.0 {
            self.start + (self.end - self.start) * ((along - walk) / link)
        } else {
            self.end
        }
    }

    /// Seconds since the `Started` event (the crossing began at the traversal origin).
    pub fn elapsed(&self) -> f32 {
        self.steps as f32 * self.step_seconds
    }

    /// The whole crossing in seconds (whole steps).
    pub fn duration(&self) -> f32 {
        self.total_steps as f32 * self.step_seconds
    }

    /// For an arc crossing, the seconds of walk plus flight the arc needs (at most
    /// [`Self::duration`]); otherwise [`Self::duration`].
    pub fn crossing_seconds(&self) -> f32 {
        self.arc
            .map_or_else(|| self.duration(), |a| self.walk_seconds + a.flight_seconds)
    }

    /// Seconds into the arc's flight now (0 on the walk to the take-off point); `None` for a
    /// straight crossing.
    pub fn flight_time(&self) -> Option<f32> {
        self.arc.map(|a| {
            (self.crossing_seconds() * self.progress() - self.walk_seconds).clamp(0.0, a.flight_seconds)
        })
    }
}

/// Whether a [`NavLinkEvent`] is a take-off or a landing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavLinkPhase {
    /// The crossing began: recorded at the traversal origin, where the agent was when it committed,
    /// which can be up to `2.25 * radius` short of the link's start point.
    Started,
    /// The agent reached the landing.
    Finished,
    /// The agent reached the take-off point of a reserved link and joined its queue
    /// ([`NavLinkQueueConfig`]).
    Queued,
    /// The agent waited `timeout_steps` in a link's queue and gave up.
    TimedOut,
    /// The crossing was called off because its landing tile left the world (ADR-2589,
    /// [`NavCrowd::sync_streaming`]): the agent is back at the take-off point.
    Cancelled,
}

impl NavLinkPhase {
    /// The lowercase name used by the agent protocol.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Finished => "finished",
            Self::Queued => "queued",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
        }
    }
}

/// A link take-off or landing during the last [`NavCrowd::step`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavLinkEvent {
    /// The agent.
    pub agent: CrowdAgentId,
    /// The link's index in `NavMesh::off_mesh_links` at the event ([`NAV_LINK_GONE`] for a
    /// crossing whose link left the mesh before it landed).
    pub link: usize,
    /// The link's user id.
    pub user_id: u64,
    /// The link's kind.
    pub kind: NavLinkKind,
    /// Take-off or landing.
    pub phase: NavLinkPhase,
    /// The agent's position at the event.
    pub position: Vec3,
}

impl Default for NavCrowdConfig {
    fn default() -> Self {
        Self {
            step_seconds: 1.0 / 60.0,
            max_agents: 1024,
            max_steps_per_advance: 8,
            snap_extents: Vec3::new(2.0, 2.0, 2.0),
            avoidance: AvoidanceParams::default(),
            link_traversal: NavLinkTraversalMode::Timed,
            link_queue: NavLinkQueueConfig::default(),
            link_arcs: NavLinkArcConfig::default(),
            stuck_window_steps: 0,
            stuck_min_progress_m: DEFAULT_STUCK_MIN_PROGRESS_M,
        }
    }
}

/// The stuck settings check shared by [`NavCrowdConfig`] and [`NavCrowd::set_stuck_detection`].
fn validate_stuck(min_progress_m: f32) -> Result<(), NavCrowdError> {
    if min_progress_m.is_finite() && min_progress_m > 0.0 {
        Ok(())
    } else {
        Err(NavCrowdError::InvalidConfig(
            "stuck_min_progress_m must be finite and positive",
        ))
    }
}

impl NavCrowdConfig {
    fn validate(&self) -> Result<(), NavCrowdError> {
        validate_stuck(self.stuck_min_progress_m)?;
        if !(self.step_seconds.is_finite() && self.step_seconds > 0.0 && self.step_seconds <= 1.0) {
            return Err(NavCrowdError::InvalidConfig("step_seconds must be within (0, 1]"));
        }
        if self.max_agents == 0 || self.max_agents > MAX_CROWD_AGENTS {
            return Err(NavCrowdError::InvalidConfig(
                "max_agents must be within 1..=65536",
            ));
        }
        if !(self.snap_extents.is_finite() && self.snap_extents.cmpge(Vec3::ZERO).all()) {
            return Err(NavCrowdError::InvalidConfig(
                "snap_extents must be finite and non-negative",
            ));
        }
        if self.max_steps_per_advance > MAX_STEPS_PER_ADVANCE {
            return Err(NavCrowdError::InvalidConfig(
                "max_steps_per_advance must be within 0..=3600",
            ));
        }
        if !self.link_arcs.is_valid() {
            return Err(NavCrowdError::InvalidConfig(
                "link_arcs needs a finite positive gravity and max_jump_speed and a non-negative apex_clearance",
            ));
        }
        self.avoidance.validate()
    }
}

/// The id of an agent in a [`NavCrowd`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CrowdAgentId(pub u64);

/// Where one crowd agent stands in a formation (ADR-8358).
///
/// The leader follows its corridor. A follower steers toward
/// [`formation_offset_world`] of this offset, or queues behind the leader when the corridor is
/// too narrow for the shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrowdGroup {
    /// The agent the formation follows.
    pub leader: CrowdAgentId,
    /// Slot offset in the leader's frame: x to the right, z backward.
    pub offset: Vec3,
}

/// Heading a formation uses. The leader's horizontal velocity when it is at least
/// [`FORMATION_HEADING_SPEED`], otherwise the direction toward `corner` when that is at least
/// 5 cm, otherwise world -Z. A crowd keeps the previous facing when this would fall back, so a
/// group that stops does not spin.
pub fn formation_heading(velocity: Vec3, position: Vec3, corner: Option<Vec3>) -> Vec3 {
    let flat_v = Vec3::new(velocity.x, 0.0, velocity.z);
    if flat_v.length() >= FORMATION_HEADING_SPEED {
        return flat_v.normalize_or(Vec3::NEG_Z);
    }
    if let Some(corner) = corner {
        let to = Vec3::new(corner.x - position.x, 0.0, corner.z - position.z);
        if to.length() >= 0.05 {
            return to.normalize_or(Vec3::NEG_Z);
        }
    }
    Vec3::NEG_Z
}

/// `anchor + right * offset.x + up * offset.y - heading * offset.z`, with `heading` flattened
/// to the ground. `offset` is x right, z backward, in the anchor's frame.
pub fn formation_offset_world(anchor: Vec3, heading: Vec3, offset: Vec3) -> Vec3 {
    let flat = Vec3::new(heading.x, 0.0, heading.z).normalize_or(Vec3::NEG_Z);
    let right = Vec3::new(-flat.z, 0.0, flat.x);
    anchor + right * offset.x + Vec3::Y * offset.y - flat * offset.z
}

impl std::fmt::Display for CrowdAgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What an agent is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrowdAgentState {
    /// No target: the agent stands still (it can still be pushed).
    Idle,
    /// Following its corridor toward the target.
    Walking,
    /// Within `max(radius / 4, 0.01)` metres of its goal in XZ. It stays arrived until that
    /// horizontal distance exceeds `max(radius, max(radius / 4, 0.01))`, so a radius under the
    /// entry floor does not leave and re-enter on the next tick. The test ignores Y.
    Arrived,
    /// The target is not reachable from where the agent stands.
    NoPath,
    /// Crossing an off-mesh link ([`CrowdAgent::link_traversal`]).
    OffMeshLink,
    /// Standing on a tile that is not loaded: frozen until it returns ([`NavCrowd::sync_streaming`]).
    Dormant,
    /// At the end of a partial path, beside an unloaded tile, waiting for it to load
    /// ([`NavCrowd::set_target_streamed`]). It replans when the tile is ready.
    AwaitingTile,
    /// Waiting at a take-off point for a reserved link ([`NavLinkQueueConfig`],
    /// [`CrowdAgent::link_wait`]). It stands still and still counts for avoidance.
    LinkQueued,
    /// Waited `timeout_steps` for a link and gave up: it stands, keeps its target, and walks again
    /// on the next [`NavCrowd::set_target`] or replan (a mesh change) (protocol code
    /// `nav_link_queue_timeout`).
    LinkTimedOut,
    /// Made no progress through the whole recovery ladder ([`NavCrowdConfig::stuck_window_steps`],
    /// [`NavStuckAction::Reported`]): it stands, keeps its target, and walks again on the next
    /// [`NavCrowd::set_target`] or replan (a mesh change). Only with stuck detection on.
    Stuck,
}

impl CrowdAgentState {
    /// The lowercase name used by the agent protocol.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Walking => "walking",
            Self::Arrived => "arrived",
            Self::NoPath => "no_path",
            Self::OffMeshLink => "off_mesh_link",
            Self::Dormant => "dormant",
            Self::AwaitingTile => "awaiting_tile",
            Self::LinkQueued => "link_queued",
            Self::LinkTimedOut => "link_timeout",
            Self::Stuck => "stuck",
        }
    }
}

/// Why a crowd operation failed. [`Self::code`] is the stable agent-protocol reason.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum NavCrowdError {
    /// An agent parameter is invalid (non-finite, zero radius or height, negative speed...).
    #[error("invalid crowd agent parameters: {0}")]
    InvalidAgentParams(&'static str),
    /// A crowd setting is invalid.
    #[error("invalid crowd config: {0}")]
    InvalidConfig(&'static str),
    /// A position, target or time step is not finite (or a time step is negative).
    #[error("invalid input: {0}")]
    InvalidInput(&'static str),
    /// The position or target is not within the snap box of any walkable polygon.
    #[error("the point is not on the navmesh")]
    OffMesh,
    /// No agent has this id.
    #[error("no crowd agent with id {0}")]
    UnknownAgent(CrowdAgentId),
    /// The crowd already holds `max_agents` agents.
    #[error("the crowd is full")]
    TooManyAgents,
}

impl NavCrowdError {
    /// The stable lowercase reason code (agent protocol `data.reason`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidAgentParams(_) => "nav_invalid_agent_params",
            Self::InvalidConfig(_) => "nav_invalid_crowd_config",
            Self::InvalidInput(_) => "nav_invalid_input",
            Self::OffMesh => "nav_off_mesh",
            Self::UnknownAgent(_) => "nav_unknown_agent",
            Self::TooManyAgents => "nav_too_many_agents",
        }
    }
}

/// A wall segment near an agent, with its outward normal (pointing off the walkable side).
#[derive(Debug, Clone, Copy, PartialEq)]
struct WallSegment {
    a: Vec3,
    b: Vec3,
    normal: Vec3,
}

/// One crowd agent.
#[derive(Debug, Clone)]
pub struct CrowdAgent {
    id: CrowdAgentId,
    params: CrowdAgentParams,
    position: Vec3,
    velocity: Vec3,
    desired_velocity: Vec3,
    planned_velocity: Vec3,
    desired_speed: f32,
    state: CrowdAgentState,
    target: Option<Vec3>,
    goal: Vec3,
    corridor: Vec<u32>,
    corners: Vec<Vec3>,
    /// Whether the last corner is the goal (so the agent slows down for it).
    corners_reach_goal: bool,
    /// The off-mesh link ahead when the last corner is a link take-off.
    link_landing: Option<LinkLanding>,
    /// The link being crossed.
    traversal: Option<NavLinkTraversal>,
    neighbours: Vec<usize>,
    walls: Vec<WallSegment>,
    walls_center: Vec3,
    walls_valid: bool,
    needs_replan: bool,
    /// The target was set with [`NavCrowd::set_target_streamed`]: plans go through the tile cache.
    streamed: bool,
    /// For a partial streamed path, the unloaded tile beside its end.
    awaiting_tile: Option<(u32, u32)>,
    /// The reserved link this agent queues for.
    link_wait: Option<NavLinkWait>,
    /// Formation membership. `None` is an ordinary agent.
    group: Option<CrowdGroup>,
    /// The last step put this follower in the column fallback.
    column: bool,
    /// The world point this follower steered toward on the last step.
    slot_target: Option<Vec3>,
    /// Last horizontal facing used for this agent's formation. Kept when the agent stops,
    /// so a group does not spin to world -Z on arrival.
    facing: Vec3,
    /// Progress toward the next corner, for stuck detection.
    stuck: StuckTracker,
}

/// One agent's progress record for stuck detection (NM-2).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct StuckTracker {
    /// The corner being approached; `None` when not tracking.
    corner: Option<Vec3>,
    /// The closest the agent has come to `corner` (XZ metres).
    best: f32,
    /// Walking steps since `best` last improved by `stuck_min_progress_m`.
    steps: u32,
    /// The ladder rung reached: 0 none, 1 re-pathed, 2 nudged, 3 or more reported.
    stage: u32,
}

/// A rung of the stuck-recovery ladder ([`NavCrowdConfig::stuck_window_steps`]): each window
/// without progress climbs one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavStuckAction {
    /// First: the agent plans a new corridor from where it stands.
    Repath,
    /// Second: it is moved up to one radius toward the center of the polygon it stands on (it
    /// stays on that convex polygon) and plans again.
    Nudge,
    /// Last: it gives up as [`CrowdAgentState::Stuck`]; the owner decides what to do.
    Reported,
}

impl NavStuckAction {
    /// The lowercase name used by the agent protocol.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Repath => "repath",
            Self::Nudge => "nudge",
            Self::Reported => "reported",
        }
    }
}

/// A rung of the stuck-recovery ladder taken in the last [`NavCrowd::step`]. With
/// [`NavCrowdPlugin`] it is also a message (read it in `FixedUpdate` after [`NavCrowdStep`]).
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct NavStuckEvent {
    /// The agent.
    pub agent: CrowdAgentId,
    /// What the crowd did.
    pub action: NavStuckAction,
    /// The agent's position when it was found stuck (before a nudge).
    pub position: Vec3,
    /// The agent's goal.
    pub goal: Vec3,
}

/// An agent's place in a reserved link's queue ([`NavLinkQueueConfig`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavLinkWait {
    /// The link's index in `NavMesh::off_mesh_links`.
    pub link: usize,
    /// The link's user id.
    pub user_id: u64,
    /// The queue ticket: lower tickets cross first.
    pub ticket: u64,
    /// Fixed steps waited so far.
    pub steps: u32,
}

/// The off-mesh link at the end of an agent's current corners.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LinkLanding {
    /// The landing point.
    to: Vec3,
    /// The corridor index of the landing polygon.
    index: usize,
    /// The link's index in `NavMesh::off_mesh_links`.
    link: u32,
}

impl CrowdAgent {
    /// Whether the agent takes no part in the step's steering, pushing and surface walk: it is
    /// crossing a link or dormant.
    fn parked(&self) -> bool {
        self.traversal.is_some() || self.state == CrowdAgentState::Dormant
    }

    /// The link the agent is crossing, if any.
    pub fn link_traversal(&self) -> Option<&NavLinkTraversal> {
        self.traversal.as_ref()
    }

    /// The reserved link the agent queues for, if any ([`CrowdAgentState::LinkQueued`]).
    pub fn link_wait(&self) -> Option<&NavLinkWait> {
        self.link_wait.as_ref()
    }

    /// The agent's id.
    pub fn id(&self) -> CrowdAgentId {
        self.id
    }
    /// The movement parameters.
    pub fn params(&self) -> &CrowdAgentParams {
        &self.params
    }
    /// The position. On the mesh surface while `Walking`, `Arrived`, `Idle` or `Dormant`; during
    /// `OffMeshLink` it is the traversal segment from the origin through the start to the end,
    /// which leaves the mesh across the gap.
    pub fn position(&self) -> Vec3 {
        self.position
    }
    /// The velocity after the last step.
    pub fn velocity(&self) -> Vec3 {
        self.velocity
    }
    /// The desired velocity of the last step (before avoidance).
    pub fn desired_velocity(&self) -> Vec3 {
        self.desired_velocity
    }
    /// What the agent is doing.
    pub fn state(&self) -> CrowdAgentState {
        self.state
    }
    /// The requested target, if any.
    pub fn target(&self) -> Option<Vec3> {
        self.target
    }
    /// The target snapped onto the mesh (meaningful while a target is set).
    pub fn goal(&self) -> Vec3 {
        self.goal
    }
    /// The corridor: polygon indices from the current polygon to the goal polygon.
    pub fn corridor(&self) -> &[u32] {
        &self.corridor
    }
    /// For a target set with [`NavCrowd::set_target_streamed`] whose path stops at the loaded
    /// surface: the unloaded tile `(x, z)` beside the path's end. `None` for a complete path.
    pub fn awaiting_tile(&self) -> Option<(u32, u32)> {
        self.awaiting_tile
    }
    /// Whether the target was set with [`NavCrowd::set_target_streamed`].
    pub fn streamed_target(&self) -> bool {
        self.streamed
    }
    /// The next steering corners (last step).
    pub fn corners(&self) -> &[Vec3] {
        &self.corners
    }
    /// Formation membership, when this agent was given one.
    pub fn group(&self) -> Option<CrowdGroup> {
        self.group
    }
    /// Whether the last step queued this follower in a column behind its leader.
    pub fn column(&self) -> bool {
        self.column
    }
    /// The world point this follower steered toward on the last step.
    pub fn slot_target(&self) -> Option<Vec3> {
        self.slot_target
    }

    /// Whether the next step will plan a new corridor.
    ///
    /// Set when the mesh under the corridor changed, the corridor stopped connecting, or the
    /// target was stored while the agent could not plan. Cleared by a plan.
    pub fn needs_replan(&self) -> bool {
        self.needs_replan
    }
}

/// Counters over a crowd's lifetime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CrowdStats {
    /// Fixed steps run.
    pub steps: u64,
    /// Paths planned (initial plans and replans).
    pub replans: u64,
    /// Plans that found no route.
    pub path_failures: u64,
    /// Candidate velocities scored by the planner.
    pub velocity_samples: u64,
    /// Visibility shortcuts applied to corridors.
    pub shortcuts: u64,
    /// Off-mesh link crossings finished.
    pub link_traversals: u64,
    /// Times an agent went dormant.
    pub dormant_entries: u64,
    /// Times an agent reached the end of a partial streamed path and began to wait for a tile.
    pub tile_waits: u64,
    /// Times an agent joined a reserved link's queue (ADR-2420).
    pub link_queue_waits: u64,
    /// Times an agent gave up waiting for a reserved link.
    pub link_queue_timeouts: u64,
    /// Crossings called off because their landing tile left the world (ADR-2589).
    pub link_cancellations: u64,
    /// Windows without progress (each climbs one rung of the stuck ladder, NM-2).
    pub stuck_detections: u64,
    /// Stuck agents re-pathed ([`NavStuckAction::Repath`]).
    pub stuck_repaths: u64,
    /// Stuck agents nudged ([`NavStuckAction::Nudge`]).
    pub stuck_nudges: u64,
    /// Stuck agents given up on ([`NavStuckAction::Reported`]).
    pub stuck_reports: u64,
    /// Agents that made progress again after a rung.
    pub stuck_recoveries: u64,
}

/// A crowd of navmesh agents (see the module docs). Insert it as a resource with
/// [`NavCrowdPlugin`] to step it in `FixedUpdate`, or call [`Self::step`] yourself.
#[derive(Resource, Debug, Clone)]
pub struct NavCrowd {
    config: NavCrowdConfig,
    agents: Vec<CrowdAgent>,
    next_id: u64,
    accumulator: f32,
    stats: CrowdStats,
    mesh_revision: Option<u64>,
    link_events: Vec<NavLinkEvent>,
    /// The crossings that took off in the last step or advance (ADR-2589).
    traversals_started: Vec<NavLinkTraversal>,
    /// Link events raised between steps ([`Self::sync_streaming`] cancelling a crossing); they
    /// open the next step's events.
    pending_link_events: Vec<NavLinkEvent>,
    /// The agent crossing each reserved link (link index → agent).
    link_holders: std::collections::BTreeMap<usize, CrowdAgentId>,
    /// The mesh link list the stored link indices (holders, crossings, queue places, landings)
    /// point into; when the mesh's list differs they are moved to the same links in it.
    seen_links: Vec<OffMeshLink>,
    /// The next queue ticket.
    next_ticket: u64,
    /// The stuck-ladder rungs of the last step or advance (NM-2).
    stuck_events: Vec<NavStuckEvent>,
    /// Buffers reused by every step. Taken out for the step so the agents and the buffers can
    /// be borrowed together.
    scratch: CrowdScratch,
}

impl NavCrowd {
    /// An empty crowd. Errors: an invalid config.
    pub fn new(config: NavCrowdConfig) -> Result<Self, NavCrowdError> {
        config.validate()?;
        Ok(Self {
            config,
            agents: Vec::new(),
            next_id: 0,
            accumulator: 0.0,
            stats: CrowdStats::default(),
            mesh_revision: None,
            link_events: Vec::new(),
            traversals_started: Vec::new(),
            pending_link_events: Vec::new(),
            link_holders: std::collections::BTreeMap::new(),
            seen_links: Vec::new(),
            next_ticket: 0,
            stuck_events: Vec::new(),
            scratch: CrowdScratch::default(),
        })
    }

    /// Changes link reservation (ADR-2420). Turning it off releases every queued agent, which
    /// walks on and crosses as before; turning it on affects take-offs from the next step, and an
    /// agent already crossing holds its link.
    pub fn set_link_queue(&mut self, queue: NavLinkQueueConfig) {
        self.config.link_queue = queue;
        if queue.exclusive {
            for agent in &self.agents {
                if let Some(t) = agent.traversal
                    && t.link != NAV_LINK_GONE
                {
                    self.link_holders.entry(t.link).or_insert(agent.id);
                }
            }
        } else {
            for agent in &mut self.agents {
                if agent.link_wait.take().is_some() && agent.state == CrowdAgentState::LinkQueued {
                    agent.state = CrowdAgentState::Walking;
                }
            }
            self.link_holders.clear();
        }
    }

    /// Changes the ballistic crossings (ADR-2589). Crossings under way keep the path they took off
    /// on; the next take-offs use the new settings. Errors: a value out of range (nothing
    /// changes).
    pub fn set_link_arcs(&mut self, arcs: NavLinkArcConfig) -> Result<(), NavCrowdError> {
        if !arcs.is_valid() {
            return Err(NavCrowdError::InvalidConfig(
                "link_arcs needs a finite positive gravity and max_jump_speed and a non-negative apex_clearance",
            ));
        }
        self.config.link_arcs = arcs;
        Ok(())
    }

    /// The crossings that took off in the last [`Self::step`] or [`Self::advance`], in event
    /// order: what [`NavCrowdPlugin`] writes as [`NavLinkTraversal`] messages.
    pub fn link_takeoffs(&self) -> &[NavLinkTraversal] {
        &self.traversals_started
    }

    /// The agents on a link now with their crossing, in agent order (the `nav.link_traversals`
    /// agent method).
    pub fn link_traversals(&self) -> Vec<(CrowdAgentId, NavLinkTraversal)> {
        self.agents
            .iter()
            .filter_map(|a| a.traversal.map(|t| (a.id, t)))
            .collect()
    }

    /// The agent crossing each reserved link now, as `(link index, agent)` in link order. Empty
    /// unless [`NavLinkQueueConfig::exclusive`].
    pub fn link_holders(&self) -> Vec<(usize, CrowdAgentId)> {
        self.link_holders.iter().map(|(&l, &a)| (l, a)).collect()
    }

    /// The agents queued for `link` (an index into `NavMesh::off_mesh_links`), first in line
    /// first.
    pub fn link_queue(&self, link: usize) -> Vec<CrowdAgentId> {
        let mut queued: Vec<(u64, CrowdAgentId)> = self
            .agents
            .iter()
            .filter_map(|a| a.link_wait.filter(|w| w.link == link).map(|w| (w.ticket, a.id)))
            .collect();
        queued.sort_unstable();
        queued.into_iter().map(|(_, id)| id).collect()
    }

    /// The crowd settings.
    pub fn config(&self) -> &NavCrowdConfig {
        &self.config
    }

    /// The lifetime counters.
    pub fn stats(&self) -> CrowdStats {
        self.stats
    }

    /// The off-mesh link take-offs and landings of the last [`Self::step`] or [`Self::advance`],
    /// step by step: in each step first the landings of crossings already under way, then the
    /// take-offs, each in agent order. An agent removed during a crossing reports no landing.
    /// With [`NavCrowdPlugin`], read them in `FixedUpdate` `.after(NavCrowdStep)`: every step
    /// replaces them, and a frame can run several steps or none.
    pub fn link_events(&self) -> &[NavLinkEvent] {
        &self.link_events
    }

    /// The stuck-ladder rungs taken in the last [`Self::step`] or [`Self::advance`], in step and
    /// agent order (NM-2). Empty while [`NavCrowdConfig::stuck_window_steps`] is 0.
    pub fn stuck_events(&self) -> &[NavStuckEvent] {
        &self.stuck_events
    }

    /// How many agents are [`CrowdAgentState::Stuck`] now.
    pub fn stuck_count(&self) -> usize {
        self.agents
            .iter()
            .filter(|a| a.state == CrowdAgentState::Stuck)
            .count()
    }

    /// Changes stuck detection (NM-2): `window_steps` 0 turns it off. Every agent's progress
    /// record starts over. Errors: a `min_progress_m` that is not finite and positive (nothing
    /// changes).
    pub fn set_stuck_detection(
        &mut self,
        window_steps: u32,
        min_progress_m: f32,
    ) -> Result<(), NavCrowdError> {
        validate_stuck(min_progress_m)?;
        self.config.stuck_window_steps = window_steps;
        self.config.stuck_min_progress_m = min_progress_m;
        for agent in &mut self.agents {
            agent.stuck = StuckTracker::default();
        }
        Ok(())
    }

    /// Every agent, in id order.
    pub fn agents(&self) -> &[CrowdAgent] {
        &self.agents
    }

    /// The agent with `id`.
    pub fn agent(&self, id: CrowdAgentId) -> Option<&CrowdAgent> {
        self.index_of(id).map(|i| &self.agents[i])
    }

    /// How many agents the crowd holds.
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    /// Whether the crowd has no agents.
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    fn index_of(&self, id: CrowdAgentId) -> Option<usize> {
        self.agents.binary_search_by_key(&id, |a| a.id).ok()
    }

    /// Adds an agent at `position` (snapped onto the mesh). Errors: invalid parameters, a
    /// non-finite position, a position off the mesh, or a full crowd.
    pub fn add_agent(
        &mut self,
        mesh: &NavMesh,
        position: Vec3,
        params: CrowdAgentParams,
    ) -> Result<CrowdAgentId, NavCrowdError> {
        params.validate()?;
        if !position.is_finite() {
            return Err(NavCrowdError::InvalidInput("agent position must be finite"));
        }
        if self.agents.len() >= self.config.max_agents {
            return Err(NavCrowdError::TooManyAgents);
        }
        let (poly, snapped) =
            find_nearest_on(mesh, position, self.config.snap_extents).ok_or(NavCrowdError::OffMesh)?;
        let id = CrowdAgentId(self.next_id);
        self.next_id += 1;
        self.agents.push(CrowdAgent {
            id,
            params,
            position: snapped,
            velocity: Vec3::ZERO,
            desired_velocity: Vec3::ZERO,
            planned_velocity: Vec3::ZERO,
            desired_speed: 0.0,
            state: CrowdAgentState::Idle,
            target: None,
            goal: snapped,
            corridor: vec![poly],
            corners: Vec::new(),
            corners_reach_goal: false,
            link_landing: None,
            traversal: None,
            neighbours: Vec::new(),
            walls: Vec::new(),
            walls_center: snapped,
            walls_valid: false,
            needs_replan: false,
            streamed: false,
            awaiting_tile: None,
            link_wait: None,
            group: None,
            column: false,
            slot_target: None,
            facing: Vec3::NEG_Z,
            stuck: StuckTracker::default(),
        });
        Ok(id)
    }

    /// Puts `id` in a formation behind `group.leader`. The leader follows its own corridor.
    /// Errors: a non-finite offset, or an unknown agent.
    pub fn set_agent_group(&mut self, id: CrowdAgentId, group: CrowdGroup) -> Result<(), NavCrowdError> {
        if !group.offset.is_finite() {
            return Err(NavCrowdError::InvalidInput("formation offset must be finite"));
        }
        if self.index_of(group.leader).is_none() {
            return Err(NavCrowdError::UnknownAgent(group.leader));
        }
        let Some(i) = self.index_of(id) else {
            return Err(NavCrowdError::UnknownAgent(id));
        };
        self.agents[i].group = Some(group);
        Ok(())
    }

    /// Drops formation membership. The agent steers on its own corridor again.
    /// Errors: an unknown id.
    pub fn clear_agent_group(&mut self, id: CrowdAgentId) -> Result<(), NavCrowdError> {
        let Some(i) = self.index_of(id) else {
            return Err(NavCrowdError::UnknownAgent(id));
        };
        self.agents[i].group = None;
        self.agents[i].column = false;
        self.agents[i].slot_target = None;
        Ok(())
    }

    /// The formation membership of `id`. Errors: an unknown id.
    pub fn agent_group(&self, id: CrowdAgentId) -> Result<Option<CrowdGroup>, NavCrowdError> {
        self.agent(id)
            .map(|agent| agent.group)
            .ok_or(NavCrowdError::UnknownAgent(id))
    }

    /// Whether `id` was in the column fallback on the last step. Errors: an unknown id.
    pub fn agent_column(&self, id: CrowdAgentId) -> Result<bool, NavCrowdError> {
        self.agent(id)
            .map(|agent| agent.column)
            .ok_or(NavCrowdError::UnknownAgent(id))
    }

    /// The slot point `id` steered toward on the last step. Errors: an unknown id.
    pub fn agent_slot_target(&self, id: CrowdAgentId) -> Result<Option<Vec3>, NavCrowdError> {
        self.agent(id)
            .map(|agent| agent.slot_target)
            .ok_or(NavCrowdError::UnknownAgent(id))
    }

    /// Removes an agent. Errors: an unknown id. A link it held or queued for is released.
    pub fn remove_agent(&mut self, id: CrowdAgentId) -> Result<(), NavCrowdError> {
        let i = self.index_of(id).ok_or(NavCrowdError::UnknownAgent(id))?;
        self.agents.remove(i);
        self.link_holders.retain(|_, holder| *holder != id);
        Ok(())
    }

    /// Changes an agent's parameters. Errors: invalid parameters or an unknown id.
    pub fn set_agent_params(
        &mut self,
        id: CrowdAgentId,
        params: CrowdAgentParams,
    ) -> Result<(), NavCrowdError> {
        params.validate()?;
        let i = self.index_of(id).ok_or(NavCrowdError::UnknownAgent(id))?;
        let agent = &mut self.agents[i];
        // Another filter means other routes and other walls.
        agent.needs_replan |= agent.params.link_filter != params.link_filter && agent.target.is_some();
        agent.params = params;
        agent.walls_valid = false;
        Ok(())
    }

    /// Sends an agent toward `target` and plans its corridor now. An unreachable target is not an
    /// error: the agent's state becomes [`CrowdAgentState::NoPath`]. Errors: a non-finite target,
    /// a target off the mesh, or an unknown id.
    pub fn set_target(
        &mut self,
        mesh: &NavMesh,
        id: CrowdAgentId,
        target: Vec3,
    ) -> Result<(), NavCrowdError> {
        if !target.is_finite() {
            return Err(NavCrowdError::InvalidInput("target must be finite"));
        }
        let i = self.index_of(id).ok_or(NavCrowdError::UnknownAgent(id))?;
        let excluded = self.agents[i].params.link_filter.excluded_areas;
        let (_, goal) = find_nearest_allowed(mesh, target, self.config.snap_extents, excluded)
            .ok_or(NavCrowdError::OffMesh)?;
        let agent = &mut self.agents[i];
        agent.target = Some(target);
        agent.goal = goal;
        agent.streamed = false;
        agent.awaiting_tile = None;
        agent.stuck = StuckTracker::default();
        if agent.parked() {
            // Mid-link, plan from the landing; dormant, plan when the tile is back.
            agent.needs_replan = true;
            return Ok(());
        }
        agent.state = CrowdAgentState::Walking;
        self.plan(mesh, i);
        Ok(())
    }

    /// [`Self::set_target`] for a streamed world (spec 78 § 13.2): the target may lie on a tile
    /// that is not loaded. The agent plans with [`NavTileCache::find_path_streamed_filtered`]: over
    /// the loaded tiles to the target when it can, otherwise to the point of the loaded surface
    /// beside the unloaded tile closest to the target, where it waits as
    /// [`CrowdAgentState::AwaitingTile`]. [`Self::sync_streaming`] replans it whenever the mesh
    /// changes (a tile on its route left: it re-paths around it or to the new edge) and when the
    /// tile it waits for is ready, so a host must call it before each step ([`NavCrowdPlugin`]
    /// does). A cache that is not streamed plans as [`Self::set_target`] does.
    ///
    /// Returns how the plan ended, or `None` when the agent is crossing a link or dormant and plans
    /// later. [`NavPathOutcome::NoPath`] leaves the agent in [`CrowdAgentState::NoPath`], as for an
    /// unreachable target. Errors: a non-finite target or an unknown id (a target off the loaded
    /// mesh is not an error).
    pub fn set_target_streamed(
        &mut self,
        cache: &NavTileCache,
        id: CrowdAgentId,
        target: Vec3,
    ) -> Result<Option<NavPathOutcome>, NavCrowdError> {
        if !target.is_finite() {
            return Err(NavCrowdError::InvalidInput("target must be finite"));
        }
        let i = self.index_of(id).ok_or(NavCrowdError::UnknownAgent(id))?;
        let agent = &mut self.agents[i];
        agent.target = Some(target);
        agent.goal = target;
        agent.streamed = true;
        agent.awaiting_tile = None;
        agent.stuck = StuckTracker::default();
        if agent.parked() {
            // Mid-link, plan from the landing; dormant, plan when the tile is back.
            agent.needs_replan = true;
            return Ok(None);
        }
        agent.state = CrowdAgentState::Walking;
        Ok(Some(self.plan_streamed(cache, i)))
    }

    /// Plans streamed agent `i` through `cache` and returns the outcome.
    fn plan_streamed(&mut self, cache: &NavTileCache, i: usize) -> NavPathOutcome {
        let extents = self.config.snap_extents;
        let agent = &mut self.agents[i];
        agent.needs_replan = false;
        agent.walls_valid = false;
        agent.corners.clear();
        agent.link_landing = None;
        // A new plan gives up a link queue place, as `plan` does: the agent queues again with a
        // fresh place (and wait count) if the new route takes the same link.
        agent.link_wait = None;
        let Some(target) = agent.target else {
            agent.awaiting_tile = None;
            return NavPathOutcome::NoPath;
        };
        self.stats.replans += 1;
        let result =
            cache.find_path_streamed_filtered(agent.position, target, extents, &agent.params.link_filter);
        match result.path {
            Some(path) => {
                agent.corridor = path.polys;
                if let Some(&goal) = path.waypoints.last() {
                    agent.goal = goal;
                }
                agent.awaiting_tile = match result.outcome {
                    NavPathOutcome::Partial => result.unloaded_tile,
                    _ => None,
                };
                let arrived = dist_xz_sq(agent.position, agent.goal) <= arrive_radius(&agent.params).powi(2);
                agent.state = match (arrived, agent.awaiting_tile.is_some()) {
                    (false, _) => CrowdAgentState::Walking,
                    (true, false) => CrowdAgentState::Arrived,
                    (true, true) => {
                        if agent.state != CrowdAgentState::AwaitingTile {
                            self.stats.tile_waits += 1;
                        }
                        CrowdAgentState::AwaitingTile
                    }
                };
            }
            None => {
                self.stats.path_failures += 1;
                agent.state = CrowdAgentState::NoPath;
                agent.awaiting_tile = None;
                agent.corridor = find_nearest_on(cache.mesh(), agent.position, extents)
                    .map(|(poly, _)| vec![poly])
                    .unwrap_or_default();
            }
        }
        result.outcome
    }

    /// Clears an agent's target; it decelerates and stands still (after landing, when it is on
    /// an off-mesh link). Errors: an unknown id.
    pub fn clear_target(&mut self, id: CrowdAgentId) -> Result<(), NavCrowdError> {
        let i = self.index_of(id).ok_or(NavCrowdError::UnknownAgent(id))?;
        let agent = &mut self.agents[i];
        agent.target = None;
        agent.streamed = false;
        agent.awaiting_tile = None;
        agent.link_wait = None;
        agent.stuck = StuckTracker::default();
        if !agent.parked() {
            agent.state = CrowdAgentState::Idle;
        }
        agent.corridor.truncate(1);
        agent.corners.clear();
        agent.link_landing = None;
        agent.needs_replan = false;
        Ok(())
    }

    /// Marks every corridor and wall cache stale: targets are replanned and positions re-snapped
    /// on the next step. Call it after the mesh's polygons changed.
    pub fn request_replan_all(&mut self) {
        for agent in &mut self.agents {
            agent.needs_replan = true;
            agent.walls_valid = false;
        }
    }

    /// Replans everything when `revision` differs from the last one seen (pass
    /// [`NavTileCache::revision`]).
    pub fn sync_mesh_revision(&mut self, revision: u64) {
        if self.mesh_revision != Some(revision) {
            if self.mesh_revision.is_some() {
                self.request_replan_all();
            }
            self.mesh_revision = Some(revision);
        }
    }

    /// Applies one tile rebuild. Agents whose corridor intersects `effect.replaced` replan.
    /// Every other corridor is renumbered by `effect.shifts` and keeps its plan.
    ///
    /// The first call only records the revision, same as [`Self::sync_mesh_revision`]. A repeat
    /// of the same revision does nothing. A patch that does not follow the last revision seen
    /// (another change moved the revision in between) replans every agent. A stranded agent
    /// ([`CrowdAgentState::NoPath`] with a target) replans on every patch.
    pub fn sync_tile_revision(&mut self, effect: &crate::tile_cache::NavTileRevision) {
        if self.mesh_revision == Some(effect.revision) {
            return;
        }
        // The patch describes one revision step. When the crowd last saw an older revision, the
        // steps between (area costs, links, a grow or an origin shift) carried no patch, so the
        // patch alone would miss them: replan everything instead.
        if self.mesh_revision.is_some() && self.mesh_revision != Some(effect.revision.wrapping_sub(1)) {
            self.request_replan_all();
            self.mesh_revision = Some(effect.revision);
            return;
        }
        if self.mesh_revision.is_some() {
            for agent in &mut self.agents {
                let hit = agent
                    .corridor
                    .iter()
                    .any(|p| effect.replaced.iter().any(|range| range.contains(p)));
                // A stranded agent's corridor is only the polygon it stands on, so a rebuild
                // elsewhere never touches it: it tries again on every tile patch, as it did on a
                // full replan.
                let stranded = agent.state == CrowdAgentState::NoPath && agent.target.is_some();
                if hit || stranded {
                    agent.needs_replan = true;
                    agent.walls_valid = false;
                } else {
                    for p in &mut agent.corridor {
                        let mut delta = 0i32;
                        for &(start, d) in &effect.shifts {
                            if *p >= start {
                                delta += d;
                            }
                        }
                        if delta != 0 {
                            *p = (*p as i32 + delta) as u32;
                        }
                    }
                }
            }
        }
        self.mesh_revision = Some(effect.revision);
    }

    /// Puts every agent standing on a tile that is not ready to sleep and wakes the others (the
    /// hook of spec 78 § 13.2). Call it before each step of a streamed world; [`NavCrowdPlugin`]
    /// does. A tile is ready once it is in the mesh with its region edits replayed. A dormant agent
    /// keeps its target and replans when it wakes. An agent on a link lands first. A cache that is
    /// not streamed changes nothing.
    pub fn sync_streaming(&mut self, cache: &NavTileCache) {
        self.follow_links(&cache.mesh().off_mesh_links);
        if !cache.is_streamed() {
            self.plan_streamed_agents(cache);
            return;
        }
        self.cancel_crossings_into_unloaded(cache);
        for agent in &mut self.agents {
            if agent.traversal.is_some() {
                continue;
            }
            let ready = cache
                .tile_at(agent.position)
                .is_none_or(|(tx, tz)| cache.tile_ready(tx, tz));
            let dormant = agent.state == CrowdAgentState::Dormant;
            if !ready && !dormant {
                agent.state = CrowdAgentState::Dormant;
                agent.velocity = Vec3::ZERO;
                agent.desired_velocity = Vec3::ZERO;
                agent.planned_velocity = Vec3::ZERO;
                agent.desired_speed = 0.0;
                agent.corners.clear();
                agent.link_landing = None;
                agent.neighbours.clear();
                agent.walls_valid = false;
                self.stats.dormant_entries += 1;
            } else if ready && dormant {
                agent.state = if agent.target.is_some() {
                    CrowdAgentState::Walking
                } else {
                    CrowdAgentState::Idle
                };
                agent.needs_replan = true;
                agent.walls_valid = false;
            }
        }
        self.plan_streamed_agents(cache);
    }

    /// The mid-link rule of ADR-2589: a crossing whose landing tile is not ready is called off. The
    /// agent goes back to its take-off point (it never stands on an unloaded tile) and replans
    /// from there; a [`NavLinkPhase::Cancelled`] event opens the next step's events. A crossing
    /// whose take-off tile left finishes (the agent is leaving it), and so does one onto a tile
    /// that only waits for a region replay.
    fn cancel_crossings_into_unloaded(&mut self, cache: &NavTileCache) {
        for i in 0..self.agents.len() {
            let Some(t) = self.agents[i].traversal else {
                continue;
            };
            let landing_gone = cache
                .tile_at(t.end)
                .is_some_and(|(tx, tz)| cache.tile_loaded(tx, tz) == Some(false));
            if !landing_gone {
                continue;
            }
            let agent = &mut self.agents[i];
            agent.traversal = None;
            agent.position = t.start;
            agent.velocity = Vec3::ZERO;
            agent.corners.clear();
            agent.link_landing = None;
            agent.walls_valid = false;
            agent.needs_replan = true;
            agent.state = if agent.target.is_some() {
                CrowdAgentState::Walking
            } else {
                agent.corridor.clear();
                CrowdAgentState::Idle
            };
            let id = agent.id;
            if self.link_holders.get(&t.link) == Some(&id) {
                self.link_holders.remove(&t.link);
            }
            self.stats.link_cancellations += 1;
            self.pending_link_events.push(NavLinkEvent {
                agent: id,
                link: t.link,
                user_id: t.user_id,
                kind: t.kind,
                phase: NavLinkPhase::Cancelled,
                position: t.start,
            });
        }
    }

    /// Replans the streamed agents that need it: after a mesh change, on waking, and when the tile
    /// one waits for is ready.
    fn plan_streamed_agents(&mut self, cache: &NavTileCache) {
        for i in 0..self.agents.len() {
            let agent = &self.agents[i];
            if !agent.streamed || agent.target.is_none() || agent.parked() {
                continue;
            }
            let tile_back = agent
                .awaiting_tile
                .is_some_and(|(tx, tz)| cache.tile_ready(tx, tz));
            if agent.needs_replan || tile_back {
                self.plan_streamed(cache, i);
                // A re-snapped position: onto the (new) first polygon, as the step does.
                let agent = &mut self.agents[i];
                if let Some(poly) = agent
                    .corridor
                    .first()
                    .and_then(|&p| cache.mesh().polys.get(p as usize))
                {
                    agent.position = poly.closest_point_xz(agent.position.x, agent.position.z);
                }
            }
        }
    }

    /// How many agents are dormant.
    pub fn dormant_count(&self) -> usize {
        self.agents
            .iter()
            .filter(|a| a.state == CrowdAgentState::Dormant)
            .count()
    }

    /// Moves every agent by `delta` (the floating-origin step of spec 78 § 12.2; call it with
    /// [`NavTileCache::shift_origin`]). Velocities and link progress stay; everything that holds a
    /// world position moves. Polygon numbers change with the mesh, so the agents replan.
    pub fn shift_origin(&mut self, delta: Vec3) {
        if !delta.is_finite() || delta == Vec3::ZERO {
            return;
        }
        for agent in &mut self.agents {
            agent.position += delta;
            agent.goal += delta;
            agent.walls_center += delta;
            if let Some(target) = &mut agent.target {
                *target += delta;
            }
            for corner in &mut agent.corners {
                *corner += delta;
            }
            if let Some(landing) = &mut agent.link_landing {
                landing.to += delta;
            }
            if let Some(t) = &mut agent.traversal {
                t.origin += delta;
                t.start += delta;
                t.end += delta;
                if let Some(arc) = &mut t.arc {
                    arc.start += delta;
                    arc.end += delta;
                    arc.apex += delta;
                }
            }
            agent.needs_replan = true;
            agent.walls_valid = false;
        }
        // The tile cache moves its links by the same delta: the stored indices still match.
        for link in &mut self.seen_links {
            link.from += delta;
            link.to += delta;
        }
    }

    /// Runs as many fixed steps as fit in `elapsed` seconds plus the carried remainder (at most
    /// `max_steps_per_advance`; time beyond that is dropped). Returns the steps run. Errors: a
    /// negative or non-finite `elapsed`.
    pub fn advance(&mut self, mesh: &NavMesh, elapsed: f32) -> Result<u32, NavCrowdError> {
        if !(elapsed.is_finite() && elapsed >= 0.0) {
            return Err(NavCrowdError::InvalidInput(
                "elapsed time must be finite and non-negative",
            ));
        }
        self.begin_events();
        self.accumulator += elapsed;
        let step = self.config.step_seconds;
        let mut steps = 0;
        while self.accumulator >= step && steps < self.config.max_steps_per_advance {
            self.run_step(mesh);
            self.accumulator -= step;
            steps += 1;
        }
        if self.accumulator >= step {
            self.accumulator %= step;
        }
        Ok(steps)
    }

    /// A 64-bit FNV-1a digest of every agent's id, state, position and velocity bits: two runs
    /// with the same inputs give the same digest.
    pub fn digest(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |bytes: &[u8]| {
            for &b in bytes {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        for a in &self.agents {
            eat(&a.id.0.to_le_bytes());
            eat(&[a.state as u8]);
            for v in [a.position, a.velocity] {
                for c in [v.x, v.y, v.z] {
                    eat(&c.to_bits().to_le_bytes());
                }
            }
        }
        h
    }

    /// Plans agent `i`'s corridor from its position to its target.
    fn plan(&mut self, mesh: &NavMesh, i: usize) {
        let extents = self.config.snap_extents;
        let agent = &mut self.agents[i];
        if agent.streamed && agent.target.is_some() {
            // A streamed target plans through the cache, in `sync_streaming` before the next step.
            agent.needs_replan = true;
            return;
        }
        agent.needs_replan = false;
        agent.walls_valid = false;
        agent.link_wait = None;
        let Some(target) = agent.target else {
            // No target: only re-snap the agent onto the (possibly changed) mesh.
            agent.corridor = find_nearest_on(mesh, agent.position, extents)
                .map(|(poly, _)| vec![poly])
                .unwrap_or_default();
            return;
        };
        self.stats.replans += 1;
        let query = NavQuery {
            mesh,
            obstacles: None,
        };
        match query.find_path_filtered(agent.position, target, extents, &agent.params.link_filter) {
            Some(path) => {
                agent.corridor = path.polys;
                if let Some(&goal) = path.waypoints.last() {
                    agent.goal = goal;
                }
                let arrived = dist_xz_sq(agent.position, agent.goal) <= arrive_radius(&agent.params).powi(2);
                agent.state = if arrived {
                    CrowdAgentState::Arrived
                } else {
                    CrowdAgentState::Walking
                };
            }
            None => {
                self.stats.path_failures += 1;
                agent.state = CrowdAgentState::NoPath;
                agent.corridor = find_nearest_on(mesh, agent.position, extents)
                    .map(|(poly, _)| vec![poly])
                    .unwrap_or_default();
            }
        }
        agent.corners.clear();
        agent.link_landing = None;
    }

    /// Whether agent `i`'s corridor still fits `mesh` (polygons exist, the agent stands on the
    /// first one, the first `CORRIDOR_LOOKAHEAD` steps still connect).
    fn corridor_valid(&self, mesh: &NavMesh, links: &LinkTable, i: usize) -> bool {
        let agent = &self.agents[i];
        let Some(&first) = agent.corridor.first() else {
            return false;
        };
        let Some(poly) = mesh.polys.get(first as usize) else {
            return false;
        };
        if poly.distance_sq_xz(agent.position.x, agent.position.z) > 1e-4 {
            return false;
        }
        let n = agent.corridor.len().min(CORRIDOR_LOOKAHEAD + 1);
        for pair in agent.corridor[..n].windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let Some(pa) = mesh.polys.get(a as usize) else {
                return false;
            };
            if (b as usize) >= mesh.polys.len() {
                return false;
            }
            let linked = pa.portals.iter().any(|p| p.neighbor == b)
                || (!links.is_empty()
                    && links
                        .between(a, b, &agent.params.link_filter, mesh.area_costs())
                        .is_some());
            if !linked {
                return false;
            }
        }
        agent.corridor.iter().all(|&p| (p as usize) < mesh.polys.len())
    }

    /// Advances the crowd by one fixed step (`config.step_seconds`) on `mesh`.
    pub fn step(&mut self, mesh: &NavMesh) {
        self.begin_events();
        self.run_step(mesh);
    }

    /// Clears the last step's link events and take-offs, and opens the new ones with the events
    /// raised since.
    fn begin_events(&mut self) {
        self.link_events.clear();
        self.traversals_started.clear();
        self.stuck_events.clear();
        self.link_events.append(&mut self.pending_link_events);
    }

    /// One fixed step; its link events add to the ones already in `link_events`.
    fn run_step(&mut self, mesh: &NavMesh) {
        let mut scratch = std::mem::take(&mut self.scratch);
        self.run_step_with(mesh, &mut scratch);
        self.scratch = scratch;
    }

    /// One fixed step using `scratch` for the vectors a step would otherwise allocate per agent.
    fn run_step_with(&mut self, mesh: &NavMesh, scratch: &mut CrowdScratch) {
        let _span = tracing::info_span!("nav_crowd_step", agents = self.agents.len()).entered();
        self.follow_links(&mesh.off_mesh_links);
        let dt = self.config.step_seconds;
        self.stats.steps += 1;
        let n = self.agents.len();
        if n == 0 {
            return;
        }
        let links = mesh.link_table();

        // 0. Off-mesh link crossings advance by one step; a finished one lands the agent.
        for i in 0..n {
            self.advance_traversal(mesh, i);
        }

        // 1. Corridor check and replanning (not while on a link).
        for i in 0..n {
            if !self.agents[i].parked()
                && !self.agents[i].needs_replan
                && !self.corridor_valid(mesh, &links, i)
            {
                self.agents[i].needs_replan = true;
            }
        }
        for i in 0..n {
            if self.agents[i].needs_replan && !self.agents[i].parked() {
                self.plan(mesh, i);
                // A re-snapped position: move the agent onto its (new) first polygon.
                let agent = &mut self.agents[i];
                if let Some(poly) = agent.corridor.first().and_then(|&p| mesh.polys.get(p as usize)) {
                    agent.position = poly.closest_point_xz(agent.position.x, agent.position.z);
                }
            }
        }

        // 2. Neighbours and walls.
        scratch.positions.clear();
        scratch.positions.extend(self.agents.iter().map(|a| a.position));
        let max_range = self
            .agents
            .iter()
            .map(|a| a.params.collision_query_range)
            .fold(0.0f32, f32::max);
        let grid_storage = std::mem::take(&mut scratch.grid);
        let grid = ProximityGrid::build_in(&scratch.positions, max_range.max(0.1), grid_storage);
        for i in 0..n {
            if self.agents[i].parked() {
                self.agents[i].neighbours.clear();
                continue;
            }
            let (pos, range, height) = {
                let a = &self.agents[i];
                (a.position, a.params.collision_query_range, a.params.height)
            };
            grid.query(pos, range, &mut scratch.candidates);
            scratch.found.clear();
            let candidate_count = scratch.candidates.len();
            for k in 0..candidate_count {
                let j = scratch.candidates[k] as usize;
                if j == i || self.agents[j].parked() {
                    continue;
                }
                let other = &self.agents[j];
                let d = dist_xz_sq(pos, other.position);
                let overlap_y =
                    other.position.y < pos.y + height && pos.y < other.position.y + other.params.height;
                if d <= range * range && overlap_y {
                    scratch.found.push((d, j));
                }
            }
            scratch
                .found
                .sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            scratch.found.truncate(MAX_CROWD_NEIGHBOURS);
            let agent = &mut self.agents[i];
            agent.neighbours.clear();
            agent.neighbours.extend(scratch.found.iter().map(|(_, j)| *j));
            let moved = dist_xz_sq(agent.position, agent.walls_center) > (range * 0.25).powi(2);
            if (moved || !agent.walls_valid) && agent.params.avoidance {
                let excluded = agent.params.link_filter.excluded_areas;
                let position = agent.position;
                collect_walls_into(
                    mesh,
                    position,
                    range,
                    height,
                    excluded,
                    &mut WallScratch {
                        candidates: &mut scratch.wall_candidates,
                        found: &mut scratch.wall_found,
                        covered: &mut scratch.wall_covered,
                        pieces: &mut scratch.wall_pieces,
                        segments: &mut scratch.wall_segments,
                    },
                );
                let agent = &mut self.agents[i];
                agent.walls.clear();
                agent.walls.append(&mut scratch.wall_segments);
                agent.walls_center = position;
                agent.walls_valid = true;
            }
        }
        scratch.grid = grid.into_storage();

        // 3. Corners, visibility shortcuts and off-mesh links. Reserved links first serve their
        // queues (ADR-2420): a waiting agent keeps its corners.
        if self.config.link_queue.exclusive {
            self.serve_link_queues(mesh, &links, scratch);
        }
        for i in 0..n {
            if self.agents[i].state == CrowdAgentState::LinkQueued {
                continue;
            }
            if self.agents[i].state != CrowdAgentState::Walking {
                self.agents[i].corners.clear();
                continue;
            }
            self.find_corners(mesh, &links, i, scratch);
            let agent = &self.agents[i];
            if agent.params.optimize_visibility && !agent.corners.is_empty() {
                let next = agent.corners[1.min(agent.corners.len() - 1)];
                if self.shortcut(mesh, i, next, scratch) {
                    self.stats.shortcuts += 1;
                }
            }
            let agent = &self.agents[i];
            if let Some(landing) = agent.link_landing
                && agent.corners.len() == 1
                && dist_xz_sq(agent.position, agent.corners[0]) <= (agent.params.radius * 2.25).powi(2)
            {
                if self.config.link_queue.exclusive && self.link_taken(landing.link as usize) {
                    self.join_link_queue(mesh, i, landing);
                } else {
                    self.start_traversal(mesh, i, landing);
                }
            }
        }

        // 4. Steering and separation. Formation followers get a slot or a column point first.
        self.update_formation_targets(mesh);
        for i in 0..n {
            let dvel = self.steer(i);
            let agent = &mut self.agents[i];
            agent.desired_velocity = dvel;
        }

        // 5. Velocity planning.
        let avoidance = self.config.avoidance;
        for i in 0..n {
            let planned = if self.agents[i].params.avoidance && self.agents[i].desired_speed > 0.0 {
                scratch.neighbour_ids.clear();
                scratch
                    .neighbour_ids
                    .extend_from_slice(&self.agents[i].neighbours);
                scratch.circles.clear();
                for &j in &scratch.neighbour_ids {
                    let o = &self.agents[j];
                    scratch.circles.push(Circle {
                        position: o.position,
                        radius: o.params.radius,
                        velocity: o.velocity,
                        desired_velocity: o.desired_velocity,
                    });
                }
                let agent = &self.agents[i];
                scratch.wall_filter.clear();
                for w in &agent.walls {
                    if (agent.position - w.a).dot(w.normal) <= 0.0 {
                        scratch.wall_filter.push(*w);
                    }
                }
                fill_prepared(
                    agent.position,
                    agent.desired_velocity,
                    &scratch.circles,
                    &mut scratch.prepared,
                );
                fill_touching(agent.position, &scratch.wall_filter, &mut scratch.touching);
                let mut base =
                    Vec3::new(agent.desired_velocity.x, 0.0, agent.desired_velocity.z).normalize_or_zero();
                if base == Vec3::ZERO {
                    base = Vec3::X;
                }
                let base_angle = det_atan2(base.z, base.x);
                sample_pattern_into(
                    base_angle,
                    avoidance.divisions,
                    avoidance.rings,
                    &mut scratch.pattern,
                );
                let pattern = std::mem::take(&mut scratch.pattern);
                let (v, samples) = {
                    let mut sampler = Sampler::from_parts(
                        &avoidance,
                        agent,
                        &scratch.prepared,
                        &scratch.wall_filter,
                        &scratch.touching,
                    );
                    let v = sampler.sample_adaptive(&pattern);
                    (v, sampler.samples)
                };
                scratch.pattern = pattern;
                self.stats.velocity_samples += samples;
                v
            } else {
                self.agents[i].desired_velocity
            };
            self.agents[i].planned_velocity = planned;
        }

        // 6. Integration (acceleration-limited).
        scratch.npos.clear();
        for agent in &mut self.agents {
            if agent.parked() {
                agent.velocity = Vec3::ZERO;
                scratch.npos.push(agent.position);
                continue;
            }
            let max_delta = agent.params.max_acceleration * dt;
            let mut dv = agent.planned_velocity - agent.velocity;
            dv.y = 0.0;
            let len = dv.length();
            if len > max_delta && len > 0.0 {
                dv *= max_delta / len;
            }
            agent.velocity += dv;
            let mut p = agent.position;
            if agent.velocity.length() > 1e-4 {
                p += agent.velocity * dt;
            } else {
                agent.velocity = Vec3::ZERO;
            }
            scratch.npos.push(p);
        }

        // 7. Collision resolution (Jacobi: every displacement is computed from the same
        // positions, then applied together, so agent order cannot matter).
        let max_radius = self.agents.iter().map(|a| a.params.radius).fold(0.0f32, f32::max);
        let grid_storage = std::mem::take(&mut scratch.grid);
        let contact_grid = ProximityGrid::build_in(&scratch.npos, (max_radius * 2.0).max(0.1), grid_storage);
        scratch.contact_ids.clear();
        scratch.contact_ranges.clear();
        for i in 0..n {
            let start = scratch.contact_ids.len();
            let a = &self.agents[i];
            if !a.parked() {
                let pos = scratch.npos[i];
                let radius = (a.params.radius + max_radius) * 1.5;
                let height = a.params.height;
                let y = a.position.y;
                contact_grid.query(pos, radius, &mut scratch.candidates);
                let count = scratch.candidates.len();
                for k in 0..count {
                    let j = scratch.candidates[k] as usize;
                    let o = &self.agents[j];
                    if j != i
                        && !o.parked()
                        && o.position.y < y + height
                        && y < o.position.y + o.params.height
                    {
                        scratch.contact_ids.push(j);
                    }
                }
            }
            scratch.contact_ranges.push((start, scratch.contact_ids.len()));
        }
        scratch.grid = contact_grid.into_storage();
        scratch.disp.clear();
        scratch.disp.resize(n, Vec3::ZERO);
        for _ in 0..COLLISION_ITERATIONS {
            for i in 0..n {
                let a = &self.agents[i];
                let mut sum = Vec3::ZERO;
                let mut count = 0.0f32;
                let (c0, c1) = scratch.contact_ranges[i];
                for k in c0..c1 {
                    let j = scratch.contact_ids[k];
                    let o = &self.agents[j];
                    let mut diff = scratch.npos[i] - scratch.npos[j];
                    diff.y = 0.0;
                    let reach = a.params.radius + o.params.radius;
                    let dist_sq = diff.length_squared();
                    if dist_sq > reach * reach {
                        continue;
                    }
                    let dist = dist_sq.sqrt();
                    if dist < 1e-4 {
                        // Exactly on top of each other: split sideways to the desired motion,
                        // in opposite directions decided by agent order.
                        let mut side = Vec3::new(-a.desired_velocity.z, 0.0, a.desired_velocity.x);
                        if side.length_squared() < 1e-8 {
                            side = Vec3::X;
                        }
                        let side = if i > j {
                            -side.normalize()
                        } else {
                            side.normalize()
                        };
                        sum += side * 0.01;
                    } else {
                        let push = (reach - dist) * 0.5 * COLLISION_RESOLVE_FACTOR;
                        sum += diff * (push / dist);
                    }
                    count += 1.0;
                }
                scratch.disp[i] = if count > 0.0 { sum / count } else { Vec3::ZERO };
            }
            for i in 0..n {
                let d = scratch.disp[i];
                scratch.npos[i] += d;
            }
        }
        // Hard pass: remove the remaining overlap pair by pair, in agent order (Gauss-Seidel;
        // the order is fixed, so the result is still deterministic).
        for _ in 0..HARD_COLLISION_ITERATIONS {
            let mut worst = 0.0f32;
            for i in 0..n {
                let (c0, c1) = scratch.contact_ranges[i];
                for k in c0..c1 {
                    let j = scratch.contact_ids[k];
                    if j <= i {
                        continue;
                    }
                    let reach = self.agents[i].params.radius + self.agents[j].params.radius;
                    let mut diff = scratch.npos[i] - scratch.npos[j];
                    diff.y = 0.0;
                    let dist_sq = diff.length_squared();
                    if dist_sq >= reach * reach {
                        continue;
                    }
                    let dist = dist_sq.sqrt();
                    let normal = if dist > 1e-4 { diff / dist } else { Vec3::X };
                    let half = (reach - dist) * 0.5;
                    worst = worst.max(reach - dist);
                    scratch.npos[i] += normal * half;
                    scratch.npos[j] -= normal * half;
                }
            }
            if worst <= 1e-4 {
                break;
            }
        }

        // 8. Surface constraint: walk to the new position across portals, clamp at walls.
        for i in 0..n {
            let target = scratch.npos[i];
            if self.agents[i].parked() {
                continue;
            }
            let Some(&start_poly) = self.agents[i].corridor.first() else {
                continue;
            };
            if (start_poly as usize) >= mesh.polys.len() {
                continue;
            }
            let origin = self.agents[i].position;
            let excluded = self.agents[i].params.link_filter.excluded_areas;
            let position =
                move_along_surface_into(mesh, start_poly, origin, target, excluded, &mut scratch.visited);
            merge_moved_into(
                &mut self.agents[i].corridor,
                &scratch.visited,
                &mut scratch.merged,
            );
            self.agents[i].position = position;
        }

        // 9. Arrival. Horizontal only: a goal one floor up with the same XZ still counts.
        for agent in &mut self.agents {
            let d = dist_xz_sq(agent.position, agent.goal);
            let next = arrival_state(agent.state, d, agent.params.radius, agent.awaiting_tile.is_some());
            if agent.state == CrowdAgentState::Walking && next == CrowdAgentState::AwaitingTile {
                self.stats.tile_waits += 1;
            }
            agent.state = next;
        }

        // 10. Stuck detection and recovery (NM-2), only when switched on.
        if self.config.stuck_window_steps > 0 {
            self.track_stuck(mesh);
        }
    }

    /// Updates every walking agent's progress toward its next corner and climbs the recovery
    /// ladder for the ones that made none for a whole window (NM-2).
    fn track_stuck(&mut self, mesh: &NavMesh) {
        let window = self.config.stuck_window_steps;
        let min_progress = self.config.stuck_min_progress_m;
        for i in 0..self.agents.len() {
            let agent = &mut self.agents[i];
            if agent.state != CrowdAgentState::Walking {
                // Not walking: nothing to measure, and the ladder starts over. A stuck agent keeps
                // its rung, so being stuck again after a replan reports after one window.
                if agent.state != CrowdAgentState::Stuck {
                    agent.stuck = StuckTracker::default();
                }
                continue;
            }
            let Some(&corner) = agent.corners.first() else {
                continue;
            };
            let pos = agent.position;
            let d = dist_xz_sq(pos, corner).sqrt();
            let t = &mut agent.stuck;
            match t.corner {
                Some(c) if dist_xz_sq(c, corner) <= 1e-6 => {
                    if t.best - d >= min_progress {
                        t.best = d;
                        t.steps = 0;
                        if t.stage > 0 {
                            t.stage = 0;
                            self.stats.stuck_recoveries += 1;
                        }
                    } else {
                        t.steps += 1;
                    }
                }
                Some(_) => {
                    // A new corner. The agent reached the old one (it came within a radius plus
                    // the progress step): that is progress. Otherwise a new plan, a nudge or a
                    // flapping corner: measure from here, but keep the count and the rung, so
                    // neither the ladder's own re-plan nor a nudge reads as a recovery.
                    let reached = t.best <= agent.params.radius + min_progress;
                    t.corner = Some(corner);
                    t.best = d;
                    if reached {
                        t.steps = 0;
                        if t.stage > 0 {
                            t.stage = 0;
                            self.stats.stuck_recoveries += 1;
                        }
                    } else {
                        t.steps += 1;
                    }
                }
                None => {
                    t.corner = Some(corner);
                    t.best = d;
                    t.steps = 0;
                }
            }
            if t.steps < window {
                continue;
            }
            t.steps = 0;
            t.stage = t.stage.saturating_add(1);
            let stage = t.stage;
            self.stats.stuck_detections += 1;
            let action = match stage {
                1 => NavStuckAction::Repath,
                2 => NavStuckAction::Nudge,
                _ => NavStuckAction::Reported,
            };
            self.stuck_events.push(NavStuckEvent {
                agent: agent.id,
                action,
                position: pos,
                goal: agent.goal,
            });
            match action {
                NavStuckAction::Repath => {
                    self.stats.stuck_repaths += 1;
                    agent.needs_replan = true;
                }
                NavStuckAction::Nudge => {
                    self.stats.stuck_nudges += 1;
                    if let Some(poly) = agent.corridor.first().and_then(|&p| mesh.polys.get(p as usize)) {
                        let c = poly.center_xz();
                        let to = Vec3::new(c.x - pos.x, 0.0, c.z - pos.z);
                        let len = to.length();
                        let step = len.min(agent.params.radius);
                        if len > 1e-6 {
                            let moved = pos + to * (step / len);
                            agent.position = Vec3::new(moved.x, poly.height(), moved.z);
                        }
                    }
                    agent.velocity = Vec3::ZERO;
                    agent.walls_valid = false;
                    agent.needs_replan = true;
                }
                NavStuckAction::Reported => {
                    self.stats.stuck_reports += 1;
                    agent.state = CrowdAgentState::Stuck;
                    agent.corners.clear();
                    agent.velocity = Vec3::ZERO;
                }
            }
        }
    }

    /// Agent `i` reached the take-off of `landing`'s link: it starts crossing (or, in instant
    /// mode, crosses now).
    fn start_traversal(&mut self, mesh: &NavMesh, i: usize, landing: LinkLanding) {
        let dt = self.config.step_seconds;
        let instant = self.config.link_traversal == NavLinkTraversalMode::Instant;
        let link = mesh.off_mesh_links.get(landing.link as usize).copied();
        let agent = &mut self.agents[i];
        let origin = agent.position;
        let start = agent.corners.first().copied().unwrap_or(origin);
        // On the polygon the agent walks on from, so it does not hop when it lands.
        let end = agent
            .corridor
            .get(landing.index)
            .and_then(|&p| mesh.polys.get(p as usize))
            .map_or(landing.to, |poly| {
                poly.closest_point_xz(landing.to.x, landing.to.z)
            });
        let (user_id, kind, authored) =
            link.map(|l| (l.user_id, l.kind, l.duration))
                .unwrap_or((0, NavLinkKind::Jump, 0.0));
        let length = (landing.to - start).length();
        let arc = if instant {
            None
        } else {
            self.config.link_arcs.arc(kind, start, end)
        };
        let walk_seconds = match arc {
            Some(_) if agent.params.max_speed > 0.0 => (start - origin).length() / agent.params.max_speed,
            _ => 0.0,
        };
        // An authored duration counts only when it is finite and positive: a NaN would cross in
        // no step and an infinite one would park the agent for hours (finding 9 of
        // sprint-deep-zai-nav-ai; a mesh's links can be edited in place without validation).
        let authored = if authored.is_finite() && authored > 0.0 {
            authored
        } else {
            0.0
        };
        let seconds = if let Some(arc) = &arc {
            // The arc's flight time wins over an authored duration (ADR-2589).
            walk_seconds + arc.flight_seconds
        } else if authored > 0.0 {
            authored
        } else if kind == NavLinkKind::Teleport {
            0.0
        } else if agent.params.max_speed > 0.0 {
            length / agent.params.max_speed
        } else {
            0.0
        };
        // `clamp` passes a NaN through and `NaN as u32` is 0: the crossing still takes one step.
        let total_steps = (((seconds / dt).ceil().clamp(1.0, MAX_LINK_TRAVERSAL_STEPS as f32)) as u32).max(1);
        agent
            .corridor
            .drain(..landing.index.min(agent.corridor.len().saturating_sub(1)));
        agent.velocity = Vec3::ZERO;
        agent.corners.clear();
        agent.link_landing = None;
        agent.walls_valid = false;
        agent.state = CrowdAgentState::OffMeshLink;
        agent.link_wait = None;
        if self.config.link_queue.exclusive {
            self.link_holders.insert(landing.link as usize, agent.id);
        }
        agent.traversal = Some(NavLinkTraversal {
            link: landing.link as usize,
            user_id,
            kind,
            origin,
            start,
            end,
            steps: 0,
            total_steps,
            step_seconds: dt,
            arc,
            walk_seconds,
        });
        if let Some(t) = agent.traversal {
            self.traversals_started.push(t);
        }
        self.link_events.push(NavLinkEvent {
            agent: agent.id,
            link: landing.link as usize,
            user_id,
            kind,
            phase: NavLinkPhase::Started,
            position: origin,
        });
        if instant {
            if let Some(t) = &mut agent.traversal {
                t.steps = t.total_steps.saturating_sub(1);
            }
            self.advance_traversal(mesh, i);
        }
    }

    /// Moves agent `i` one step along its link; on the last step it lands and walks on (or
    /// stands, when its target was cleared meanwhile).
    fn advance_traversal(&mut self, mesh: &NavMesh, i: usize) {
        let extents = self.config.snap_extents;
        let agent = &mut self.agents[i];
        let Some(mut t) = agent.traversal else {
            return;
        };
        t.steps = (t.steps + 1).min(t.total_steps);
        if t.steps < t.total_steps {
            agent.position = t.position();
            agent.traversal = Some(t);
            return;
        }
        // A replan is due: the mesh may have changed under the crossing (polygon numbers do not
        // survive a tile rebuild), so the landing is found again.
        agent.position = if agent.needs_replan {
            find_nearest_on(mesh, t.end, extents).map_or(t.end, |(_, p)| p)
        } else {
            t.end
        };
        agent.traversal = None;
        agent.walls_valid = false;
        let id = agent.id;
        if self.link_holders.get(&t.link) == Some(&id) {
            self.link_holders.remove(&t.link);
        }
        let agent = &mut self.agents[i];
        agent.state = if agent.target.is_some() {
            CrowdAgentState::Walking
        } else {
            agent.corridor.truncate(1);
            CrowdAgentState::Idle
        };
        self.stats.link_traversals += 1;
        self.link_events.push(NavLinkEvent {
            agent: agent.id,
            link: t.link,
            user_id: t.user_id,
            kind: t.kind,
            phase: NavLinkPhase::Finished,
            position: agent.position,
        });
    }

    /// Moves every stored link index (holders, crossings, queue places, landings) to the same link
    /// in `links` when the list changed since the last call (finding 4 of
    /// sprint-deep-zai-nav-ai): a streamed tile loading or unloading, or a replaced list, shifts
    /// the indices of later links. A link is the same when its endpoints, direction, user id and
    /// kind are; among such copies an unchanged copy pairs first, and a copy in use keeps a link
    /// before an unused one does (`pair_links`). A crossing whose link is gone goes on and
    /// reports [`NAV_LINK_GONE`]; a queue place or a landing on a gone link is dropped and the
    /// agent replans.
    fn follow_links(&mut self, links: &[OffMeshLink]) {
        if self.seen_links.len() == links.len()
            && self
                .seen_links
                .iter()
                .zip(links)
                .all(|(a, b)| same_link_bits(a, b))
        {
            return;
        }
        let _span = tracing::trace_span!("nav_crowd_follow_links", links = links.len()).entered();
        let old = std::mem::replace(&mut self.seen_links, links.to_vec());
        // Every stored index with its rank on a shortage of copies (`pair_links`): a crossing or a
        // reservation first (it is in the air), then a queue place, then a planned landing.
        let mut used: std::collections::BTreeMap<usize, u8> = std::collections::BTreeMap::new();
        let mut note = |index: usize, rank: u8| {
            let r = used.entry(index).or_insert(rank);
            *r = (*r).min(rank);
        };
        for &index in self.link_holders.keys() {
            note(index, 0);
        }
        for a in &self.agents {
            if let Some(t) = a.traversal {
                note(t.link, 0);
            }
            if let Some(w) = a.link_wait {
                note(w.link, 1);
            }
            if let Some(l) = a.link_landing {
                note(l.link as usize, 2);
            }
        }
        if used.is_empty() {
            return;
        }
        let moved: std::collections::BTreeMap<usize, Option<usize>> =
            pair_links(&old, links, &used).into_iter().collect();
        let follow = |index: usize| moved.get(&index).copied().flatten();
        self.link_holders = std::mem::take(&mut self.link_holders)
            .into_iter()
            .filter_map(|(link, id)| follow(link).map(|l| (l, id)))
            .collect();
        for agent in &mut self.agents {
            if let Some(t) = &mut agent.traversal {
                t.link = follow(t.link).unwrap_or(NAV_LINK_GONE);
            }
            if let Some(w) = &mut agent.link_wait {
                match follow(w.link) {
                    Some(link) => w.link = link,
                    None => {
                        agent.link_wait = None;
                        if agent.state == CrowdAgentState::LinkQueued {
                            agent.state = CrowdAgentState::Walking;
                        }
                        agent.needs_replan = true;
                    }
                }
            }
            if let Some(landing) = &mut agent.link_landing {
                match follow(landing.link as usize).and_then(|l| u32::try_from(l).ok()) {
                    Some(link) => landing.link = link,
                    None => {
                        agent.link_landing = None;
                        agent.corners.clear();
                        agent.needs_replan = true;
                    }
                }
            }
        }
    }

    /// Whether a reserved `link` is crossed now or already has a queue.
    fn link_taken(&self, link: usize) -> bool {
        self.link_holders.contains_key(&link)
            || self
                .agents
                .iter()
                .any(|a| a.link_wait.is_some_and(|w| w.link == link))
    }

    /// Agent `i` reached the take-off of a reserved link that is taken: it waits there.
    fn join_link_queue(&mut self, mesh: &NavMesh, i: usize, landing: LinkLanding) {
        let (user_id, kind) = mesh
            .off_mesh_links
            .get(landing.link as usize)
            .map_or((0, NavLinkKind::Jump), |l| (l.user_id, l.kind));
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        self.stats.link_queue_waits += 1;
        let agent = &mut self.agents[i];
        agent.state = CrowdAgentState::LinkQueued;
        agent.link_landing = Some(landing);
        agent.link_wait = Some(NavLinkWait {
            link: landing.link as usize,
            user_id,
            ticket,
            steps: 0,
        });
        self.link_events.push(NavLinkEvent {
            agent: agent.id,
            link: landing.link as usize,
            user_id,
            kind,
            phase: NavLinkPhase::Queued,
            position: agent.position,
        });
    }

    /// One step of the reserved links' queues: holders that left are dropped, waits that ran out
    /// time out (in agent order), and each free link lets its first agent take off (in link
    /// order, so the result depends on nothing but the queue).
    fn serve_link_queues(&mut self, mesh: &NavMesh, links: &LinkTable, scratch: &mut CrowdScratch) {
        let _span = tracing::trace_span!("nav_crowd_link_queues").entered();
        // A holder that was removed, or no longer crosses its link, frees it.
        let agents = &self.agents;
        self.link_holders.retain(|&link, id| {
            agents
                .binary_search_by_key(id, |a| a.id)
                .ok()
                .and_then(|i| agents[i].traversal)
                .is_some_and(|t| t.link == link)
        });
        let timeout = self.config.link_queue.timeout_steps;
        let mut queued: Vec<(usize, u64, usize)> = Vec::new();
        for i in 0..self.agents.len() {
            let agent = &mut self.agents[i];
            let Some(mut wait) = agent.link_wait else {
                continue;
            };
            if agent.state != CrowdAgentState::LinkQueued || agent.link_landing.is_none() {
                agent.link_wait = None;
                continue;
            }
            wait.steps = wait.steps.saturating_add(1);
            agent.link_wait = Some(wait);
            if timeout > 0 && wait.steps > timeout {
                agent.link_wait = None;
                agent.link_landing = None;
                agent.corners.clear();
                agent.state = CrowdAgentState::LinkTimedOut;
                self.stats.link_queue_timeouts += 1;
                let kind = mesh
                    .off_mesh_links
                    .get(wait.link)
                    .map_or(NavLinkKind::Jump, |l| l.kind);
                self.link_events.push(NavLinkEvent {
                    agent: agent.id,
                    link: wait.link,
                    user_id: wait.user_id,
                    kind,
                    phase: NavLinkPhase::TimedOut,
                    position: agent.position,
                });
                continue;
            }
            queued.push((wait.link, wait.ticket, i));
        }
        queued.sort_unstable();
        let mut served = None;
        for (link, _, i) in queued {
            if served == Some(link) || self.link_holders.contains_key(&link) {
                continue;
            }
            served = Some(link);
            // Pushing may have moved the agent along its corridor while it waited: the corners
            // are found again, so the landing's corridor index is current.
            self.find_corners(mesh, links, i, scratch);
            match self.agents[i].link_landing {
                Some(landing) if landing.link as usize == link => self.start_traversal(mesh, i, landing),
                _ => {
                    // Pushed off its route over the link: it walks on, and queues again if needed.
                    let agent = &mut self.agents[i];
                    agent.link_wait = None;
                    agent.state = CrowdAgentState::Walking;
                }
            }
        }
    }

    /// Finds agent `i`'s next corners by string-pulling its corridor from its position.
    fn find_corners(&mut self, mesh: &NavMesh, links: &LinkTable, i: usize, scratch: &mut CrowdScratch) {
        let agent = &mut self.agents[i];
        agent.link_landing = None;
        scratch.portals.clear();
        let mut end = agent.goal;
        let mut reaches_goal = true;
        for (k, pair) in agent.corridor.windows(2).enumerate() {
            if let Some(portal) = portal_between(mesh, pair[0], pair[1]) {
                scratch.portals.push(portal);
                continue;
            }
            match links.between(pair[0], pair[1], &agent.params.link_filter, mesh.area_costs()) {
                Some(edge) => {
                    end = edge.from;
                    agent.link_landing = Some(LinkLanding {
                        to: edge.to,
                        index: k + 1,
                        link: edge.link,
                    });
                }
                None => agent.needs_replan = true,
            }
            reaches_goal = false;
            break;
        }
        let position = agent.position;
        let landing = agent.link_landing;
        funnel_into(
            position,
            end,
            &scratch.portals,
            &mut scratch.oriented,
            &mut scratch.funnel_pts,
            &mut scratch.funnel_result,
        );
        let pulled = std::mem::take(&mut scratch.funnel_result);
        scratch.corners.clear();
        scratch.corners.extend(pulled.iter().skip(1).copied());
        scratch.funnel_result = pulled;
        while scratch.corners.len() > 1
            && dist_xz_sq(scratch.corners[0], position) < MIN_CORNER_DISTANCE * MIN_CORNER_DISTANCE
        {
            scratch.corners.remove(0);
        }
        // Standing on the take-off point (a cancelled crossing puts the agent back there,
        // ADR-2589) leaves the funnel with no corner; the take-off point is still the corner
        // the link starts from.
        if scratch.corners.is_empty() && landing.is_some() {
            scratch.corners.push(end);
        }
        let total = scratch.corners.len();
        scratch.corners.truncate(MAX_CORNERS);
        let agent = &mut self.agents[i];
        agent.corners_reach_goal = reaches_goal && total <= MAX_CORNERS;
        if total > MAX_CORNERS {
            agent.link_landing = None;
        }
        agent.corners.clear();
        agent.corners.extend_from_slice(&scratch.corners);
    }

    /// Tries a visibility shortcut from agent `i` toward `next`; returns whether the corridor
    /// changed.
    fn shortcut(&mut self, mesh: &NavMesh, i: usize, next: Vec3, scratch: &mut CrowdScratch) -> bool {
        let agent = &self.agents[i];
        let delta = Vec3::new(next.x - agent.position.x, 0.0, next.z - agent.position.z);
        let dist = delta.length();
        if dist < MIN_CORNER_DISTANCE || agent.params.path_optimization_range <= 0.0 {
            return false;
        }
        // Aim slightly past the corner (so the walk lands inside the polygon beyond it), but
        // never further than the optimisation range.
        let reach = (dist + 0.01).min(agent.params.path_optimization_range);
        let end = agent.position + delta * (reach / dist);
        let Some(&first) = agent.corridor.first() else {
            return false;
        };
        let position = agent.position;
        let excluded = agent.params.link_filter.excluded_areas;
        let (reached, _) = walk_segment_into(mesh, first, position, end, excluded, &mut scratch.visited);
        if !reached || scratch.visited.len() < 2 {
            return false;
        }
        if !shortcut_stays_as_cheap(
            mesh,
            &self.agents[i].params.link_filter,
            &self.agents[i].corridor,
            &scratch.visited,
        ) {
            return false;
        }
        merge_shortcut_into(
            &mut self.agents[i].corridor,
            &scratch.visited,
            &mut scratch.merged,
        )
    }

    /// Agent `i`'s desired velocity: toward the next corner, slowed near the goal, plus
    /// separation from neighbours. Also stores the desired speed.
    ///
    /// A formation follower with a slot target steers to that point instead, including while
    /// idle: the leader owns the corridor.
    fn steer(&mut self, i: usize) -> Vec3 {
        if self.agents[i].slot_target.is_some()
            && self.agents[i]
                .group
                .is_some_and(|group| group.leader != self.agents[i].id)
        {
            return self.steer_formation(i);
        }
        let agent = &self.agents[i];
        if agent.state != CrowdAgentState::Walking || agent.corners.is_empty() {
            self.agents[i].desired_speed = 0.0;
            return Vec3::ZERO;
        }
        let pos = agent.position;
        let dir = if agent.params.anticipate_turns {
            let p0 = agent.corners[0];
            let p1 = agent.corners[1.min(agent.corners.len() - 1)];
            let d0 = Vec3::new(p0.x - pos.x, 0.0, p0.z - pos.z);
            let d1 = Vec3::new(p1.x - pos.x, 0.0, p1.z - pos.z);
            let len0 = d0.length();
            let d1 = d1.normalize_or_zero();
            (d0 - d1 * (len0 * 0.5)).normalize_or_zero()
        } else {
            let p0 = agent.corners[0];
            Vec3::new(p0.x - pos.x, 0.0, p0.z - pos.z).normalize_or_zero()
        };
        // Slow down over the last two radii before the goal.
        let slow_radius = agent.params.radius * 2.0;
        let scale = if agent.corners_reach_goal {
            let last = agent.corners[agent.corners.len() - 1];
            dist_xz_sq(pos, last).sqrt().min(slow_radius) / slow_radius
        } else {
            1.0
        };
        let speed = agent.params.max_speed;
        let mut dvel = dir * (speed * scale);

        if agent.params.separation && agent.params.separation_weight > 0.0 {
            let range = agent.params.collision_query_range;
            let mut push = Vec3::ZERO;
            let mut count = 0.0f32;
            for &j in &agent.neighbours {
                let o = &self.agents[j];
                let diff = Vec3::new(pos.x - o.position.x, 0.0, pos.z - o.position.z);
                let d_sq = diff.length_squared();
                if d_sq < 1e-5 || d_sq > range * range {
                    continue;
                }
                let d = d_sq.sqrt();
                let falloff = 1.0 - (d / range).powi(2);
                push += diff * (agent.params.separation_weight * falloff / d);
                count += 1.0;
            }
            if count > 0.0 {
                // Fade separation with the goal slow-down, so near the goal it never outweighs
                // the pull toward it (the agent would hover short of its goal).
                dvel += push * (scale / count);
                let len = dvel.length();
                if len > speed && len > 0.0 {
                    dvel *= speed / len;
                }
            }
        }
        // The slowed speed is the sampler's cap too: at full `max_speed` an escape sample would
        // let an agent near its goal still move at up to `max_speed * (1 - velocity_bias)`.
        self.agents[i].desired_speed = (speed * scale).max(dvel.length());
        dvel
    }

    /// Follower `i` matches its leader and pulls toward the slot (or the column point).
    fn steer_formation(&mut self, i: usize) -> Vec3 {
        let Some(target) = self.agents[i].slot_target else {
            self.agents[i].desired_speed = 0.0;
            return Vec3::ZERO;
        };
        let Some(group) = self.agents[i].group else {
            self.agents[i].desired_speed = 0.0;
            return Vec3::ZERO;
        };
        let Some(leader_i) = self.index_of(group.leader) else {
            self.agents[i].desired_speed = 0.0;
            return Vec3::ZERO;
        };
        let leader_vel = self.agents[leader_i].velocity;
        let leader_pos = self.agents[leader_i].position;
        let leader_corner = self.agents[leader_i].corners.first().copied();
        let pos = self.agents[i].position;
        let max_speed = self.agents[i].params.max_speed;
        let heading = formation_heading(leader_vel, leader_pos, leader_corner);
        let leader_speed = Vec3::new(leader_vel.x, 0.0, leader_vel.z).length();
        let to = Vec3::new(target.x - pos.x, 0.0, target.z - pos.z);
        let dist = to.length();
        if dist < 0.05 {
            let vel = heading * leader_speed.min(max_speed);
            self.agents[i].desired_speed = vel.length();
            return vel;
        }
        let along = dot_xz(to, heading);
        let lateral = Vec3::new(to.x - heading.x * along, 0.0, to.z - heading.z * along);
        let lateral_len = lateral.length();
        let lateral_dir = if lateral_len > 1e-4 {
            lateral / lateral_len
        } else {
            Vec3::ZERO
        };
        // Ahead of the slot: drop the speed match and wait. Behind: match, plus catch-up.
        let match_speed = if along < 0.0 { 0.0 } else { leader_speed };
        let along_speed = (match_speed + FORMATION_CATCH_UP * along).clamp(0.0, max_speed);
        let lateral_speed = (FORMATION_CATCH_UP * lateral_len).min(max_speed);
        let mut vel = heading * along_speed + lateral_dir * lateral_speed;
        let len = vel.length();
        if len > max_speed && len > 0.0 {
            vel *= max_speed / len;
        }
        self.agents[i].desired_speed = vel.length();
        vel
    }

    /// Writes each follower's slot point or column point for this step.
    fn update_formation_targets(&mut self, mesh: &NavMesh) {
        if !self.agents.iter().any(|agent| agent.group.is_some()) {
            for agent in &mut self.agents {
                agent.column = false;
                agent.slot_target = None;
            }
            return;
        }
        let _span = tracing::debug_span!(target: "regia_nav", "nav_crowd_formation").entered();
        for agent in &mut self.agents {
            agent.facing = kept_heading(
                agent.facing,
                agent.velocity,
                agent.position,
                agent.corners.first().copied(),
            );
        }
        let snaps: Vec<FormSnap> = self
            .agents
            .iter()
            .map(|agent| FormSnap {
                id: agent.id,
                position: agent.position,
                corridor: agent.corridor.clone(),
                group: agent.group,
                radius: agent.params.radius,
                facing: agent.facing,
            })
            .collect();
        let mut widths: Vec<(CrowdAgentId, f32)> = Vec::new();
        for snap in &snaps {
            let Some(group) = snap.group else {
                continue;
            };
            if group.leader == snap.id {
                continue;
            }
            let half = group.offset.x.abs();
            if let Some(row) = widths.iter_mut().find(|row| row.0 == group.leader) {
                row.1 = row.1.max(half);
            } else {
                widths.push((group.leader, half));
            }
        }
        let extents = self.config.snap_extents;
        let mut choices: Vec<FormChoice> = Vec::with_capacity(snaps.len());
        for snap in &snaps {
            let Some(group) = snap.group else {
                choices.push(FormChoice::plain(snap.id));
                continue;
            };
            if group.leader == snap.id {
                choices.push(FormChoice::plain(snap.id));
                continue;
            }
            let Some(leader) = snaps.iter().find(|row| row.id == group.leader) else {
                choices.push(FormChoice::plain(snap.id));
                continue;
            };
            let heading = leader.facing;
            let shape_half = widths
                .iter()
                .find(|row| row.0 == group.leader)
                .map(|row| row.1)
                .unwrap_or(0.0);
            let shape_width = shape_half * 2.0;
            let corridor_w = corridor_width(mesh, &leader.corridor, heading);
            let wanted = formation_offset_world(leader.position, heading, group.offset);
            let snapped = find_nearest_on(mesh, wanted, extents);
            let off = match snapped {
                None => true,
                Some((_, point)) => dist_xz_sq(point, wanted) > FORMATION_OFF_MESH * FORMATION_OFF_MESH,
            };
            let narrow = shape_width > 0.05 && corridor_w < shape_width;
            let column = off || narrow;
            let target = if column {
                None
            } else {
                Some(snapped.map(|(_, point)| point).unwrap_or(wanted))
            };
            choices.push(FormChoice {
                column,
                target,
                leader: Some(group.leader),
                offset_z: group.offset.z,
                id: snap.id,
            });
        }
        let mut ranked: Vec<usize> = (0..choices.len())
            .filter(|&index| choices[index].column)
            .collect();
        ranked.sort_by(|&a, &b| {
            let la = choices[a].leader.map(|id| id.0).unwrap_or(0);
            let lb = choices[b].leader.map(|id| id.0).unwrap_or(0);
            la.cmp(&lb)
                .then(choices[a].offset_z.total_cmp(&choices[b].offset_z))
                .then(choices[a].id.0.cmp(&choices[b].id.0))
        });
        let mut last_leader = None;
        let mut rank = 0u32;
        for index in ranked {
            let leader_id = choices[index].leader;
            if last_leader != leader_id {
                rank = 0;
                last_leader = leader_id;
            }
            if let Some(leader_id) = leader_id
                && let Some(leader) = snaps.iter().find(|row| row.id == leader_id)
            {
                let heading = leader.facing;
                let follower_radius = snaps
                    .iter()
                    .find(|row| row.id == choices[index].id)
                    .map(|row| row.radius)
                    .unwrap_or(leader.radius);
                let gap = (follower_radius.max(leader.radius) * 2.2).max(0.5);
                let raw = leader.position - heading * (gap * (rank + 1) as f32);
                let point = find_nearest_on(mesh, raw, extents)
                    .map(|(_, point)| point)
                    .unwrap_or(leader.position);
                choices[index].target = Some(point);
            }
            rank += 1;
        }
        for (agent, choice) in self.agents.iter_mut().zip(choices) {
            agent.column = choice.column;
            agent.slot_target = choice.target;
        }
    }
}

/// One agent's formation inputs, copied so the step can read every agent while writing targets.
struct FormSnap {
    id: CrowdAgentId,
    position: Vec3,
    corridor: Vec<u32>,
    group: Option<CrowdGroup>,
    radius: f32,
    facing: Vec3,
}

/// Facing for a formation step. A moving leader, or one with a corner ahead, takes a fresh
/// heading. A leader that has stopped keeps `previous`, so the group does not spin to world -Z.
fn kept_heading(previous: Vec3, velocity: Vec3, position: Vec3, corner: Option<Vec3>) -> Vec3 {
    let flat_v = Vec3::new(velocity.x, 0.0, velocity.z);
    if flat_v.length() >= FORMATION_HEADING_SPEED {
        return flat_v.normalize_or(Vec3::NEG_Z);
    }
    if let Some(corner) = corner {
        let to = Vec3::new(corner.x - position.x, 0.0, corner.z - position.z);
        if to.length() >= 0.05 {
            return to.normalize_or(Vec3::NEG_Z);
        }
    }
    let flat = Vec3::new(previous.x, 0.0, previous.z);
    if flat.length() >= 0.5 {
        flat.normalize_or(Vec3::NEG_Z)
    } else {
        Vec3::NEG_Z
    }
}

/// The slot or column decision for one agent this step.
struct FormChoice {
    column: bool,
    target: Option<Vec3>,
    leader: Option<CrowdAgentId>,
    offset_z: f32,
    id: CrowdAgentId,
}

impl FormChoice {
    fn plain(id: CrowdAgentId) -> Self {
        Self {
            column: false,
            target: None,
            leader: None,
            offset_z: 0.0,
            id,
        }
    }
}

/// How wide the upcoming corridor is across `heading`: the narrower of the polygon extent and
/// the portal length. An empty corridor is treated as wide, so a shape is not forced into a column.
fn corridor_width(mesh: &NavMesh, corridor: &[u32], heading: Vec3) -> f32 {
    if corridor.is_empty() {
        return f32::INFINITY;
    }
    let flat = Vec3::new(heading.x, 0.0, heading.z).normalize_or(Vec3::NEG_Z);
    let right = Vec3::new(-flat.z, 0.0, flat.x);
    let mut min_w = f32::INFINITY;
    for &poly_index in corridor.iter().take(8) {
        let Some(poly) = mesh.polys.get(poly_index as usize) else {
            continue;
        };
        let mut min_r = f32::INFINITY;
        let mut max_r = f32::NEG_INFINITY;
        for vertex in &poly.vertices {
            let along = vertex.x * right.x + vertex.z * right.z;
            min_r = min_r.min(along);
            max_r = max_r.max(along);
        }
        if min_r.is_finite() {
            min_w = min_w.min(max_r - min_r);
        }
    }
    for pair in corridor.windows(2).take(7) {
        if let Some((a, b)) = portal_between(mesh, pair[0], pair[1]) {
            let len = Vec3::new(b.x - a.x, 0.0, b.z - a.z).length();
            min_w = min_w.min(len);
        }
    }
    if min_w.is_finite() { min_w } else { f32::INFINITY }
}

/// Within this horizontal distance of the goal a walking agent counts as arrived.
fn arrive_radius(params: &CrowdAgentParams) -> f32 {
    (params.radius * 0.25).max(0.01)
}

/// The arrival transition for one agent. `dist_xz_sq` is the squared horizontal distance to the
/// goal (Y is ignored). Entry is `max(radius / 4, 0.01)`. An arrived or tile-waiting agent stays
/// until that distance exceeds `max(radius, entry)`, so the hold is never tighter than the entry.
fn arrival_state(
    state: CrowdAgentState,
    dist_xz_sq: f32,
    radius: f32,
    awaiting_tile: bool,
) -> CrowdAgentState {
    let reach = (radius * 0.25).max(0.01);
    // A radius under the 0.01 m floor would arrive inside `reach` and leave inside `radius`.
    let hold = radius.max(reach);
    match state {
        CrowdAgentState::Walking if dist_xz_sq <= reach * reach && awaiting_tile => {
            CrowdAgentState::AwaitingTile
        }
        CrowdAgentState::Walking if dist_xz_sq <= reach * reach => CrowdAgentState::Arrived,
        CrowdAgentState::Arrived | CrowdAgentState::AwaitingTile if dist_xz_sq > hold * hold => {
            CrowdAgentState::Walking
        }
        other => other,
    }
}

/// Squared XZ distance.
fn dist_xz_sq(a: Vec3, b: Vec3) -> f32 {
    let dx = a.x - b.x;
    let dz = a.z - b.z;
    dx * dx + dz * dz
}

/// XZ dot product.
fn dot_xz(a: Vec3, b: Vec3) -> f32 {
    a.x * b.x + a.z * b.z
}

/// XZ cross product (`a.x * b.z - a.z * b.x`).
fn cross_xz(a: Vec3, b: Vec3) -> f32 {
    a.x * b.z - a.z * b.x
}

/// Squared XZ distance from `p` to the segment `a-b`.
fn dist_point_segment_sq(p: Vec3, a: Vec3, b: Vec3) -> f32 {
    let ab = Vec3::new(b.x - a.x, 0.0, b.z - a.z);
    let ap = Vec3::new(p.x - a.x, 0.0, p.z - a.z);
    let len_sq = ab.length_squared();
    let t = if len_sq > 0.0 {
        (dot_xz(ap, ab) / len_sq).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let q = Vec3::new(a.x + ab.x * t, 0.0, a.z + ab.z * t);
    dist_xz_sq(p, q)
}

/// Flat buffers a proximity grid reuses. One list of agent indices, not a vector per cell, so a
/// shift of the grid origin does not allocate.
#[derive(Clone, Debug, Default)]
struct GridStorage {
    /// Scratch counts, then the write cursor, while a grid is filled. Length `width * height`.
    counts: Vec<u32>,
    /// First index of each cell in `indices`, plus a final total. Length `width * height + 1`.
    offsets: Vec<u32>,
    /// Agent indices packed by cell.
    indices: Vec<u32>,
}

/// A uniform XZ bucket grid over agent positions, rebuilt every step.
pub(crate) struct ProximityGrid {
    min_x: f32,
    min_z: f32,
    cell: f32,
    width: usize,
    height: usize,
    storage: GridStorage,
}

impl ProximityGrid {
    /// The most buckets; a wider spread enlarges the cells instead.
    const MAX_BUCKETS: usize = 64 * 64;

    /// Builds a grid over `positions` at `cell` metres, doubling the cell until the bucket cap holds.
    pub(crate) fn build(positions: &[Vec3], cell: f32) -> Self {
        Self::build_in(positions, cell, GridStorage::default())
    }

    /// [`Self::build`] reusing `storage` from an earlier grid.
    ///
    /// The index list and the per-cell offset table keep their capacity. A later step allocates
    /// only when the crowd's spread needs more cells than the stored tables hold.
    fn build_in(positions: &[Vec3], cell: f32, mut storage: GridStorage) -> Self {
        let (mut min_x, mut min_z) = (f32::INFINITY, f32::INFINITY);
        let (mut max_x, mut max_z) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
        for p in positions {
            min_x = min_x.min(p.x);
            min_z = min_z.min(p.z);
            max_x = max_x.max(p.x);
            max_z = max_z.max(p.z);
        }
        if positions.is_empty() {
            (min_x, min_z, max_x, max_z) = (0.0, 0.0, 0.0, 0.0);
        }
        let mut cell = cell;
        let dims = |cell: f32| {
            (
                (((max_x - min_x) / cell).floor() as usize).saturating_add(1),
                (((max_z - min_z) / cell).floor() as usize).saturating_add(1),
            )
        };
        let (mut width, mut height) = dims(cell);
        while width.saturating_mul(height) > Self::MAX_BUCKETS {
            cell *= 2.0;
            (width, height) = dims(cell);
        }
        let n = width * height;
        let cell_of = |p: Vec3| -> usize {
            let x = (((p.x - min_x) / cell).floor() as usize).min(width - 1);
            let z = (((p.z - min_z) / cell).floor() as usize).min(height - 1);
            z * width + x
        };
        if storage.counts.len() < n {
            storage.counts.resize(n, 0);
        }
        storage.counts[..n].fill(0);
        for p in positions {
            storage.counts[cell_of(*p)] += 1;
        }
        if storage.offsets.len() < n + 1 {
            storage.offsets.resize(n + 1, 0);
        }
        let mut sum = 0u32;
        let counts = &mut storage.counts[..n];
        let offsets = &mut storage.offsets[..=n];
        for (i, count) in counts.iter_mut().enumerate() {
            offsets[i] = sum;
            sum += *count;
            *count = offsets[i];
        }
        offsets[n] = sum;
        if storage.indices.len() < positions.len() {
            storage.indices.resize(positions.len(), 0);
        }
        for (i, p) in positions.iter().enumerate() {
            let bucket = cell_of(*p);
            let slot = storage.counts[bucket] as usize;
            storage.indices[slot] = i as u32;
            storage.counts[bucket] += 1;
        }
        Self {
            min_x,
            min_z,
            cell,
            width,
            height,
            storage,
        }
    }

    /// The buffers, kept so the next [`Self::build_in`] does not allocate them again.
    fn into_storage(self) -> GridStorage {
        self.storage
    }

    /// Every agent in a bucket the XZ box `center ± radius` touches, sorted by index.
    pub(crate) fn query(&self, center: Vec3, radius: f32, out: &mut Vec<u32>) {
        out.clear();
        let bucket = |v: f32, origin: f32, last: usize| -> usize {
            let f = ((v - origin) / self.cell).floor();
            if f <= 0.0 { 0 } else { (f as usize).min(last) }
        };
        let x0 = bucket(center.x - radius, self.min_x, self.width - 1);
        let x1 = bucket(center.x + radius, self.min_x, self.width - 1);
        let z0 = bucket(center.z - radius, self.min_z, self.height - 1);
        let z1 = bucket(center.z + radius, self.min_z, self.height - 1);
        for z in z0..=z1 {
            for x in x0..=x1 {
                let bucket = z * self.width + x;
                let start = self.storage.offsets[bucket] as usize;
                let end = self.storage.offsets[bucket + 1] as usize;
                out.extend_from_slice(&self.storage.indices[start..end]);
            }
        }
        out.sort_unstable();
    }
}

/// The five buffers [`collect_walls_into`] clears and fills.
struct WallScratch<'a> {
    candidates: &'a mut Vec<u32>,
    found: &'a mut Vec<(f32, WallSegment)>,
    covered: &'a mut Vec<(f32, f32)>,
    pieces: &'a mut Vec<(f32, f32)>,
    segments: &'a mut Vec<WallSegment>,
}

/// The walkable polygon edges (minus their portals) near `pos`: the closest
/// `MAX_LOCAL_SEGMENTS` within `range`, each with its outward normal.
#[cfg(test)]
fn collect_walls(mesh: &NavMesh, pos: Vec3, range: f32, height: f32, excluded: u64) -> Vec<WallSegment> {
    let mut candidates = Vec::new();
    let mut found = Vec::new();
    let mut covered = Vec::new();
    let mut pieces = Vec::new();
    let mut out = Vec::new();
    collect_walls_into(
        mesh,
        pos,
        range,
        height,
        excluded,
        &mut WallScratch {
            candidates: &mut candidates,
            found: &mut found,
            covered: &mut covered,
            pieces: &mut pieces,
            segments: &mut out,
        },
    );
    out
}

/// [`collect_walls`] writing into caller-owned buffers. Each buffer is cleared first.
fn collect_walls_into(
    mesh: &NavMesh,
    pos: Vec3,
    range: f32,
    height: f32,
    excluded: u64,
    walls: &mut WallScratch<'_>,
) {
    let WallScratch {
        candidates,
        found,
        covered,
        pieces,
        segments: out,
    } = walls;
    candidates.clear();
    mesh.candidate_polys(pos.x, pos.z, range, range, candidates);
    found.clear();
    for &pi in candidates.iter() {
        let Some(poly) = mesh.polys.get(pi as usize) else {
            continue;
        };
        if (poly.height() - pos.y).abs() > height || excluded & area_bit(poly.area) != 0 {
            continue;
        }
        let n = poly.vertices.len();
        if n < 3 {
            continue;
        }
        let centre = poly.center_xz();
        for e in 0..n {
            let a = poly.vertices[e];
            let b = poly.vertices[(e + 1) % n];
            let edge = Vec3::new(b.x - a.x, 0.0, b.z - a.z);
            let len_sq = edge.length_squared();
            if len_sq <= 0.0 {
                continue;
            }
            let mut normal = Vec3::new(edge.z, 0.0, -edge.x).normalize_or_zero();
            if dot_xz(normal, Vec3::new(centre.x - a.x, 0.0, centre.z - a.z)) > 0.0 {
                normal = -normal;
            }
            // The parts of the edge covered by portals, as parameter intervals along a → b.
            covered.clear();
            for portal in &poly.portals {
                // A portal into a left-out area is a wall to this agent.
                let barred = mesh
                    .polys
                    .get(portal.neighbor as usize)
                    .is_some_and(|next| excluded & area_bit(next.area) != 0);
                if barred {
                    continue;
                }
                let on_line = |p: Vec3| {
                    cross_xz(edge, Vec3::new(p.x - a.x, 0.0, p.z - a.z)).abs() <= 1e-4 * len_sq.sqrt()
                };
                if !(on_line(portal.a) && on_line(portal.b)) {
                    continue;
                }
                let t = |p: Vec3| dot_xz(Vec3::new(p.x - a.x, 0.0, p.z - a.z), edge) / len_sq;
                let (t0, t1) = (t(portal.a), t(portal.b));
                covered.push((t0.min(t1), t0.max(t1)));
            }
            covered.sort_by(|x, y| x.0.total_cmp(&y.0));
            let mut start = 0.0f32;
            pieces.clear();
            for &(c0, c1) in covered.iter() {
                if c0 > start + 1e-5 {
                    pieces.push((start, c0.min(1.0)));
                }
                start = start.max(c1);
            }
            if start < 1.0 - 1e-5 {
                pieces.push((start, 1.0));
            }
            for &(t0, t1) in pieces.iter() {
                if t1 - t0 <= 1e-5 {
                    continue;
                }
                let sa = a + (b - a) * t0;
                let sb = a + (b - a) * t1;
                let d = dist_point_segment_sq(pos, sa, sb);
                if d <= range * range {
                    found.push((d, WallSegment { a: sa, b: sb, normal }));
                }
            }
        }
    }
    found.sort_by(|x, y| {
        x.0.total_cmp(&y.0)
            .then(x.1.a.x.total_cmp(&y.1.a.x))
            .then(x.1.a.z.total_cmp(&y.1.a.z))
            .then(x.1.b.x.total_cmp(&y.1.b.x))
            .then(x.1.b.z.total_cmp(&y.1.b.z))
    });
    found.truncate(MAX_LOCAL_SEGMENTS);
    out.clear();
    out.extend(found.iter().map(|(_, w)| *w));
}

/// The result of walking a segment across the mesh.
#[cfg(test)]
struct Walk {
    /// Polygons entered, in order (the start polygon first).
    visited: Vec<u32>,
    /// Whether the walk reached the segment's end without hitting a wall.
    reached: bool,
    /// Where the walk stopped (the end, or the wall hit point), at the last polygon's height.
    position: Vec3,
}

/// Walks the straight XZ segment `start → end` from polygon `poly` (which holds `start`) across
/// portals until it reaches `end` or leaves through an edge that is not a portal, or a portal into
/// an area bit in `excluded`.
#[cfg(test)]
fn walk_segment(mesh: &NavMesh, poly: u32, start: Vec3, end: Vec3, excluded: u64) -> Walk {
    let mut visited = Vec::new();
    let (reached, position) = walk_segment_into(mesh, poly, start, end, excluded, &mut visited);
    Walk {
        visited,
        reached,
        position,
    }
}

fn walk_segment_into(
    mesh: &NavMesh,
    poly: u32,
    start: Vec3,
    end: Vec3,
    excluded: u64,
    visited: &mut Vec<u32>,
) -> (bool, Vec3) {
    visited.clear();
    visited.push(poly);
    let mut current = poly;
    let dir = Vec3::new(end.x - start.x, 0.0, end.z - start.z);
    let mut t_progress = 0.0f32;
    loop {
        let Some(p) = mesh.polys.get(current as usize) else {
            return (false, start);
        };
        if p.contains_xz(end.x, end.z) {
            return (true, Vec3::new(end.x, p.height(), end.z));
        }
        // Where the segment's line leaves this convex polygon: the smallest crossing parameter
        // over the edges it moves outward through.
        let centre = p.center_xz();
        let n = p.vertices.len();
        let mut t_exit = f32::INFINITY;
        let mut exit_edge = None;
        for e in 0..n {
            let a = p.vertices[e];
            let b = p.vertices[(e + 1) % n];
            let edge = Vec3::new(b.x - a.x, 0.0, b.z - a.z);
            let mut normal = Vec3::new(edge.z, 0.0, -edge.x);
            if dot_xz(normal, Vec3::new(centre.x - a.x, 0.0, centre.z - a.z)) > 0.0 {
                normal = -normal;
            }
            let denom = dot_xz(normal, dir);
            if denom <= 0.0 {
                continue;
            }
            let t = dot_xz(normal, Vec3::new(a.x - start.x, 0.0, a.z - start.z)) / denom;
            if t < t_exit {
                t_exit = t;
                exit_edge = Some((a, b));
            }
        }
        let t_exit = t_exit.clamp(t_progress, 1.0);
        let hit = Vec3::new(start.x + dir.x * t_exit, p.height(), start.z + dir.z * t_exit);
        let next = exit_edge.and_then(|(a, b)| {
            p.portals.iter().find_map(|portal| {
                let edge = Vec3::new(b.x - a.x, 0.0, b.z - a.z);
                let len = edge.length();
                if len <= 0.0 {
                    return None;
                }
                let tol = 1e-4 * len.max(1.0);
                let on_edge =
                    |q: Vec3| (cross_xz(edge, Vec3::new(q.x - a.x, 0.0, q.z - a.z)) / len).abs() <= tol;
                if !(on_edge(portal.a) && on_edge(portal.b)) {
                    return None;
                }
                let along = |q: Vec3| dot_xz(Vec3::new(q.x - a.x, 0.0, q.z - a.z), edge) / len;
                let (pa, pb) = (along(portal.a), along(portal.b));
                let h = along(hit);
                (h >= pa.min(pb) - tol && h <= pa.max(pb) + tol).then_some(portal.neighbor)
            })
        });
        // A portal naming a polygon the mesh does not hold is a wall, as in the A* search
        // (finding 12 of sprint-deep-zai-nav-ai).
        let next = next.filter(|&neighbor| {
            mesh.polys
                .get(neighbor as usize)
                .is_some_and(|poly| excluded & area_bit(poly.area) == 0)
        });
        match next {
            Some(neighbor) if !visited.contains(&neighbor) && visited.len() < MAX_WALK_POLYS => {
                visited.push(neighbor);
                current = neighbor;
                t_progress = t_exit;
            }
            _ => {
                return (false, hit);
            }
        }
    }
}

/// Whether two links are bit for bit the same (so a NaN compares equal to itself).
fn same_link_bits(a: &OffMeshLink, b: &OffMeshLink) -> bool {
    same_crossing(a, b)
        && a.area == b.area
        && a.radius.to_bits() == b.radius.to_bits()
        && a.cost_scale.to_bits() == b.cost_scale.to_bits()
        && a.flags == b.flags
        && a.enabled == b.enabled
        && a.duration.to_bits() == b.duration.to_bits()
}

/// Whether two links are the same crossing: endpoints (bits), direction, user id and kind. A link
/// switched off or re-priced is still the same link.
fn same_crossing(a: &OffMeshLink, b: &OffMeshLink) -> bool {
    a.from.to_array().map(f32::to_bits) == b.from.to_array().map(f32::to_bits)
        && a.to.to_array().map(f32::to_bits) == b.to.to_array().map(f32::to_bits)
        && a.bidirectional == b.bidirectional
        && a.user_id == b.user_id
        && a.kind == b.kind
}

/// Pairs each `used` index of `old` with the same link in `new` (`None` when it is gone).
///
/// Links with the same endpoints, direction, user id and kind are copies of one crossing
/// (`same_crossing`); a link has no other id. Within a group of copies, copies equal in every
/// field pair first (a copy that only moved in the list keeps its link, even beside a
/// switched-off or re-priced copy of it), then the copies left (switched on or off, re-priced),
/// each in list order. When the new list holds fewer copies than the old one, only the copies in
/// use take part, by rank (`used` maps an index to it: crossings and reservations 0, queue places
/// 1, planned landings 2) and then list order, so a crossing keeps its link while a copy is left
/// (identical copies cannot be told apart). The result is in index order.
fn pair_links(
    old: &[OffMeshLink],
    new: &[OffMeshLink],
    used: &std::collections::BTreeMap<usize, u8>,
) -> Vec<(usize, Option<usize>)> {
    let mut out: Vec<(usize, Option<usize>)> = Vec::with_capacity(used.len());
    let mut done = std::collections::BTreeSet::new();
    for &index in used.keys() {
        if done.contains(&index) {
            continue;
        }
        let Some(link) = old.get(index) else {
            out.push((index, None));
            continue;
        };
        let mut olds: Vec<usize> = (0..old.len()).filter(|&i| same_crossing(&old[i], link)).collect();
        let news: Vec<usize> = (0..new.len()).filter(|&i| same_crossing(&new[i], link)).collect();
        if news.len() < olds.len() {
            olds.retain(|o| used.contains_key(o));
            olds.sort_by_key(|o| (used.get(o).copied().unwrap_or(u8::MAX), *o));
        }
        let mut paired: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
        let mut taken = vec![false; news.len()];
        // Exact copies, one bit pattern at a time (in the order the old list first shows it).
        let mut grouped = vec![false; olds.len()];
        for g in 0..olds.len() {
            if grouped[g] {
                continue;
            }
            let probe = &old[olds[g]];
            let group: Vec<usize> = (g..olds.len())
                .filter(|&k| !grouped[k] && same_link_bits(&old[olds[k]], probe))
                .collect();
            let free = (0..news.len()).filter(|&k| !taken[k] && same_link_bits(&new[news[k]], probe));
            for (&k, n) in group.iter().zip(free.collect::<Vec<_>>()) {
                taken[n] = true;
                paired.insert(olds[k], news[n]);
            }
            for k in group {
                grouped[k] = true;
            }
        }
        // Edited copies with the new copies left.
        let rest = olds
            .iter()
            .filter(|o| !paired.contains_key(o))
            .copied()
            .collect::<Vec<_>>();
        let free = (0..news.len()).filter(|&k| !taken[k]).collect::<Vec<_>>();
        for (o, n) in rest.into_iter().zip(free) {
            paired.insert(o, news[n]);
        }
        for &o in &olds {
            if used.contains_key(&o) {
                out.push((o, paired.get(&o).copied()));
                done.insert(o);
            }
        }
    }
    out.sort_unstable_by_key(|&(index, _)| index);
    out
}

/// Moves from `start` toward `target` on the surface: the walk's end when it gets there, else
/// the point of the last polygon closest to `target` (so the agent slides along the wall).
#[cfg(test)]
fn move_along_surface(mesh: &NavMesh, poly: u32, start: Vec3, target: Vec3, excluded: u64) -> Walk {
    let mut visited = Vec::new();
    let (reached, mut position) = walk_segment_into(mesh, poly, start, target, excluded, &mut visited);
    if !reached && let Some(&last) = visited.last() {
        position = mesh.polys[last as usize].closest_point_xz(target.x, target.z);
    }
    Walk {
        visited,
        reached,
        position,
    }
}

fn move_along_surface_into(
    mesh: &NavMesh,
    poly: u32,
    start: Vec3,
    target: Vec3,
    excluded: u64,
    visited: &mut Vec<u32>,
) -> Vec3 {
    let (reached, mut position) = walk_segment_into(mesh, poly, start, target, excluded, visited);
    if !reached && let Some(&last) = visited.last() {
        position = mesh.polys[last as usize].closest_point_xz(target.x, target.z);
    }
    position
}

/// Whether a shortcut through `visited` pays no more per metre than the corridor stretch it
/// replaces: a straight line across the mud must not undo the route around it.
fn shortcut_stays_as_cheap(
    mesh: &NavMesh,
    filter: &NavQueryFilter,
    corridor: &[u32],
    visited: &[u32],
) -> bool {
    let Some((rejoin, _)) = furthest_common(corridor, visited) else {
        return true;
    };
    let cost = |p: &u32| {
        let area = mesh.polys.get(*p as usize).map_or(0, |poly| poly.area);
        mesh.area_costs().get(area) * filter.area_costs.get(area)
    };
    let limit = corridor[..=rejoin].iter().map(cost).fold(0.0f32, f32::max);
    visited.iter().map(cost).all(|c| c <= limit)
}

/// Updates a corridor after its agent walked through `visited` (start polygon first): the new
/// corridor starts at the polygon the walk ended in, steps back through the visited polygons
/// until it meets the furthest corridor polygon the walk touched, and continues from there.
#[cfg(test)]
fn merge_moved(corridor: &mut Vec<u32>, visited: &[u32]) {
    let mut merged = Vec::new();
    merge_moved_into(corridor, visited, &mut merged);
}

fn merge_moved_into(corridor: &mut Vec<u32>, visited: &[u32], merged: &mut Vec<u32>) {
    let Some((ci, vi)) = furthest_common(corridor, visited) else {
        return;
    };
    merged.clear();
    merged.extend(visited[vi..].iter().rev().copied());
    merged.extend_from_slice(&corridor[ci + 1..]);
    corridor.clear();
    corridor.extend_from_slice(merged);
}

/// Splices a visibility shortcut into a corridor: the walked polygons up to the furthest
/// corridor polygon they reach replace the corridor's start. Returns whether it changed.
#[cfg(test)]
fn merge_shortcut(corridor: &mut Vec<u32>, visited: &[u32]) -> bool {
    let Some((ci, vi)) = furthest_common(corridor, visited) else {
        return false;
    };
    if vi == 0 || ci <= vi {
        // The shortcut rejoins at the start, or is no shorter than the corridor it replaces.
        return false;
    }
    merge_shortcut_into(corridor, visited, &mut Vec::new())
}

fn merge_shortcut_into(corridor: &mut Vec<u32>, visited: &[u32], merged: &mut Vec<u32>) -> bool {
    let Some((ci, vi)) = furthest_common(corridor, visited) else {
        return false;
    };
    if vi == 0 || ci <= vi {
        // The shortcut rejoins at the start, or is no shorter than the corridor it replaces.
        return false;
    }
    merged.clear();
    merged.extend_from_slice(&visited[..vi]);
    merged.extend_from_slice(&corridor[ci..]);
    corridor.clear();
    corridor.extend_from_slice(merged);
    true
}

/// The largest corridor index whose polygon was visited, with its index in `visited`.
fn furthest_common(corridor: &[u32], visited: &[u32]) -> Option<(usize, usize)> {
    corridor
        .iter()
        .enumerate()
        .rev()
        .find_map(|(ci, p)| visited.iter().rposition(|v| v == p).map(|vi| (ci, vi)))
}

/// A neighbour as the velocity planner sees it.
#[derive(Clone, Copy, Debug)]
struct Circle {
    position: Vec3,
    radius: f32,
    velocity: Vec3,
    desired_velocity: Vec3,
}

/// A neighbour plus the per-sample geometry precomputed once per agent.
#[derive(Clone, Copy, Debug)]
struct PreparedCircle {
    circle: Circle,
    /// Unit direction from the agent to the neighbour.
    toward: Vec3,
    /// The side the agent should pass on (perpendicular to `toward`).
    pass_side: Vec3,
}

/// Buffers one crowd step reuses, including the proximity grids' flat index storage.
#[derive(Clone, Debug, Default)]
struct CrowdScratch {
    positions: Vec<Vec3>,
    /// Index and offset storage for the neighbour grid and the contact grid, used one at a time.
    grid: GridStorage,
    candidates: Vec<u32>,
    found: Vec<(f32, usize)>,
    neighbour_ids: Vec<usize>,
    circles: Vec<Circle>,
    wall_filter: Vec<WallSegment>,
    prepared: Vec<PreparedCircle>,
    touching: Vec<bool>,
    pattern: Vec<(f32, f32)>,
    npos: Vec<Vec3>,
    contact_ids: Vec<usize>,
    contact_ranges: Vec<(usize, usize)>,
    disp: Vec<Vec3>,
    visited: Vec<u32>,
    merged: Vec<u32>,
    wall_candidates: Vec<u32>,
    wall_found: Vec<(f32, WallSegment)>,
    wall_covered: Vec<(f32, f32)>,
    wall_pieces: Vec<(f32, f32)>,
    wall_segments: Vec<WallSegment>,
    portals: Vec<(Vec3, Vec3)>,
    oriented: Vec<(Vec3, Vec3)>,
    funnel_pts: Vec<Vec3>,
    funnel_result: Vec<Vec3>,
    corners: Vec<Vec3>,
}

fn fill_prepared(position: Vec3, desired: Vec3, circles: &[Circle], prepared: &mut Vec<PreparedCircle>) {
    prepared.clear();
    for c in circles {
        let toward = Vec3::new(c.position.x - position.x, 0.0, c.position.z - position.z).normalize_or_zero();
        let relative = c.desired_velocity - desired;
        // Pass on the side the relative motion already favours; default to the left.
        let pass_side = if cross_xz(relative, toward) < 0.01 {
            Vec3::new(-toward.z, 0.0, toward.x)
        } else {
            Vec3::new(toward.z, 0.0, -toward.x)
        };
        prepared.push(PreparedCircle {
            circle: *c,
            toward,
            pass_side,
        });
    }
}

fn fill_touching(position: Vec3, walls: &[WallSegment], touching: &mut Vec<bool>) {
    touching.clear();
    for w in walls {
        touching.push(dist_point_segment_sq(position, w.a, w.b) < 0.01 * 0.01);
    }
}

/// The velocity planner of one agent for one step.
struct Sampler<'a> {
    params: &'a AvoidanceParams,
    position: Vec3,
    radius: f32,
    velocity: Vec3,
    desired: Vec3,
    max_speed: f32,
    inv_max_speed: f32,
    circles: &'a [PreparedCircle],
    walls: &'a [WallSegment],
    /// Wall indices the agent is touching (within 1 cm).
    touching: &'a [bool],
    samples: u64,
}

impl<'a> Sampler<'a> {
    fn from_parts(
        params: &'a AvoidanceParams,
        agent: &CrowdAgent,
        circles: &'a [PreparedCircle],
        walls: &'a [WallSegment],
        touching: &'a [bool],
    ) -> Self {
        let max_speed = agent.desired_speed;
        Self {
            params,
            position: agent.position,
            radius: agent.params.radius,
            velocity: agent.velocity,
            desired: agent.desired_velocity,
            max_speed,
            inv_max_speed: if max_speed > 0.0 {
                1.0 / max_speed
            } else {
                f32::MAX
            },
            circles,
            walls,
            touching,
            samples: 0,
        }
    }

    /// The cost of candidate velocity `v`, or `best` when it provably cannot beat `best`.
    fn score(&mut self, v: Vec3, best: f32) -> f32 {
        self.samples += 1;
        let p = self.params;
        let desired_cost = p.weight_desired * (v - self.desired).length() * self.inv_max_speed;
        let current_cost = p.weight_current * (v - self.velocity).length() * self.inv_max_speed;
        // The impact cost is `weight_impact / (0.1 + t / horizon)`: any impact sooner than this
        // threshold would already cost more than `best`, so stop scanning.
        let headroom = best - desired_cost - current_cost;
        let t_cutoff = (p.weight_impact / headroom - 0.1) * p.horizon;
        if t_cutoff - p.horizon > -f32::EPSILON {
            return best;
        }
        let mut t_min = p.horizon;
        let mut side = 0.0f32;
        for c in self.circles {
            // Reciprocal velocity: assume the neighbour takes half of the avoidance.
            let rel = v * 2.0 - self.velocity - c.circle.velocity;
            side += (dot_xz(c.toward, rel) * 0.5 + 0.5)
                .min(dot_xz(c.pass_side, rel) * 2.0)
                .clamp(0.0, 1.0);
            let Some((t_enter, t_leave)) = sweep_circles(
                self.position,
                self.radius,
                rel,
                c.circle.position,
                c.circle.radius,
            ) else {
                continue;
            };
            // Already overlapping: the deeper inside, the sooner the "impact".
            let t = if t_enter < 0.0 && t_leave > 0.0 {
                -t_enter * 0.5
            } else {
                t_enter
            };
            if t >= 0.0 && t < t_min {
                t_min = t;
                if t_min < t_cutoff {
                    return best;
                }
            }
        }
        for (k, w) in self.walls.iter().enumerate() {
            let t = if self.touching[k] {
                // Touching a wall: moving into it is an immediate impact, moving away is free.
                if dot_xz(w.normal, v) <= 0.0 {
                    continue;
                }
                0.0
            } else {
                match ray_segment(self.position, v, w.a, w.b) {
                    Some(t) => t,
                    None => continue,
                }
            };
            // Walls count half as urgent as agents.
            let t = t * 2.0;
            if t < t_min {
                t_min = t;
                if t_min < t_cutoff {
                    return best;
                }
            }
        }
        if !self.circles.is_empty() {
            side /= self.circles.len() as f32;
        }
        let side_cost = p.weight_side * side;
        let impact_cost = p.weight_impact / (0.1 + t_min / p.horizon);
        desired_cost + current_cost + side_cost + impact_cost
    }

    /// Adaptive sampling: rings of candidates around the biased desired velocity, refined around
    /// the best candidate with a halving radius.
    fn sample_adaptive(&mut self, pattern: &[(f32, f32)]) -> Vec3 {
        let p = *self.params;
        let mut radius = self.max_speed * (1.0 - p.velocity_bias);
        let mut centre = Vec3::new(
            self.desired.x * p.velocity_bias,
            0.0,
            self.desired.z * p.velocity_bias,
        );
        let limit = (self.max_speed + 0.001).powi(2);
        for _ in 0..p.depth {
            let mut best = f32::MAX;
            let mut best_v = Vec3::ZERO;
            for &(px, pz) in pattern {
                let v = Vec3::new(centre.x + px * radius, 0.0, centre.z + pz * radius);
                if v.x * v.x + v.z * v.z > limit {
                    continue;
                }
                let cost = self.score(v, best);
                if cost < best {
                    best = cost;
                    best_v = v;
                }
            }
            centre = best_v;
            radius *= 0.5;
        }
        centre
    }
}

/// When a circle at `c0` (radius `r0`) moving at `v` touches a static circle at `c1` (radius
/// `r1`): `(t_enter, t_leave)` in seconds, or `None` when it never does (or `v` is ~zero).
fn sweep_circles(c0: Vec3, r0: f32, v: Vec3, c1: Vec3, r1: f32) -> Option<(f32, f32)> {
    let s = Vec3::new(c1.x - c0.x, 0.0, c1.z - c0.z);
    let r = r0 + r1;
    let c = dot_xz(s, s) - r * r;
    let a = dot_xz(v, v);
    if a < 1e-4 {
        return None;
    }
    let b = dot_xz(v, s);
    let disc = b * b - a * c;
    if disc < 0.0 {
        return None;
    }
    let root = disc.sqrt();
    Some(((b - root) / a, (b + root) / a))
}

/// Where the ray `origin + v * t` (`t` in `0..=1`) crosses the segment `a-b`, if it does.
fn ray_segment(origin: Vec3, v: Vec3, a: Vec3, b: Vec3) -> Option<f32> {
    let seg = Vec3::new(b.x - a.x, 0.0, b.z - a.z);
    let w = Vec3::new(origin.x - a.x, 0.0, origin.z - a.z);
    let d = cross_xz(v, seg);
    if d.abs() < 1e-6 {
        return None;
    }
    let t = cross_xz(seg, w) / d;
    let s = cross_xz(v, w) / d;
    ((0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&s)).then_some(t)
}

/// The unit sample pattern of [`Sampler::sample_adaptive`]: the origin, then each ring's points
/// (alternate rings rotated by half a step so the rings interleave), all from the software trig
/// so the bits are the same on every target (audit sprint-audit-g-nav-determinism H1).
#[cfg(test)]
fn sample_pattern(base_angle: f32, divisions: u32, rings: u32) -> Vec<(f32, f32)> {
    let mut pattern = Vec::new();
    sample_pattern_into(base_angle, divisions, rings, &mut pattern);
    pattern
}

fn sample_pattern_into(base_angle: f32, divisions: u32, rings: u32, pattern: &mut Vec<(f32, f32)>) {
    let divisions = divisions as usize;
    let rings = rings as usize;
    let step = std::f32::consts::TAU / divisions as f32;
    pattern.clear();
    pattern.push((0.0f32, 0.0f32));
    for ring in 0..rings {
        let r = (rings - ring) as f32 / rings as f32;
        let offset = if ring % 2 == 1 { step * 0.5 } else { 0.0 };
        for k in 0..divisions {
            let a = base_angle + offset + k as f32 * step;
            let (sin, cos) = det_sin_cos(a);
            pattern.push((cos * r, sin * r));
        }
    }
}

/// Steps a [`NavCrowd`] resource once per `FixedUpdate` (in [`NavCrowdStep`]). When a
/// [`NavTileCache`] resource exists, its queued tiles are rebuilt first (by the
/// [`crate::NavRegionPlugin`] step and its budget, when the app has one), and the crowd follows its
/// mesh (replanning after a rebuild) and its streaming; otherwise the crowd walks the [`NavMesh`]
/// resource.
#[derive(Default)]
pub struct NavCrowdPlugin;

impl Plugin for NavCrowdPlugin {
    fn build(&self, app: &mut App) {
        crate::origin::ensure(app);
        app.add_message::<NavLinkTraversal>();
        app.add_message::<NavStuckEvent>();
        app.add_systems(FixedUpdate, step_nav_crowd.in_set(NavCrowdStep));
    }
}

/// The crowd step of [`NavCrowdPlugin`]. Systems that set targets order themselves
/// `.before(NavCrowdStep)`; systems that read agent positions order themselves
/// `.after(NavCrowdStep)`.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NavCrowdStep;

/// `FixedUpdate`: one crowd step per fixed tick (the crowd's own `step_seconds`, so a replay is
/// independent of the frame rate). Applies any pending origin shift first, through the shared
/// cursor, and moves every [`crate::NavAgentState`] by those deltas. This system may be the one
/// that consumes the cursor, including when no crowd resource is present, so the waypoints still
/// move. The crowd never steps between the rebase and the shift (audit
/// sprint-audit-g-nav-determinism H2).
pub(crate) fn step_nav_crowd(
    mut stores: crate::origin::NavStores,
    mut origin_cursor: Option<ResMut<crate::origin::NavOriginShiftCursor>>,
    origin_messages: Option<Res<Messages<OriginShifted>>>,
    mut agents: Query<&mut NavAgentState>,
    mut takeoffs: MessageWriter<NavLinkTraversal>,
    stuck: Option<ResMut<Messages<NavStuckEvent>>>,
) {
    if let (Some(cursor), Some(messages)) = (origin_cursor.as_deref_mut(), origin_messages.as_deref()) {
        let deltas = crate::origin::apply_origin_shifts(&mut cursor.0, messages, &mut stores);
        crate::origin::shift_agent_states(&deltas, &mut agents);
    }
    let Some(mut crowd) = stores.crowd else {
        return;
    };
    if let Some(mut cache) = stores.cache {
        // A region queue rebuilds tiles in its own step, to its budget. Without one, this step
        // uses that same budget and replans only the corridors the rebuilt tiles touched.
        if stores.regions.is_none() {
            let budget = crate::region_queue::NavRegionConfig::default().max_tiles_per_step;
            cache.update_limited(budget);
        }
        if let Some(effect) = cache.take_partial() {
            crowd.sync_tile_revision(&effect);
        } else {
            crowd.sync_mesh_revision(cache.revision());
        }
        crowd.sync_streaming(&cache);
        crowd.step(cache.mesh());
    } else if let Some(mesh) = stores.mesh {
        crowd.step(&mesh);
    } else {
        return;
    }
    takeoffs.write_batch(crowd.link_takeoffs().iter().copied());
    // Optional, so a host stepping the crowd without the plugin's messages still runs.
    if let Some(mut stuck) = stuck {
        stuck.write_batch(crowd.stuck_events().iter().copied());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NavMeshBuilder;

    /// H1 (audit sprint-audit-g-nav-determinism): the avoidance sample pattern must come from the
    /// software trig, not the platform libm. At this base angle the platform libm builds a default
    /// pattern whose 9th point's sine is one ulp above the kernel's (`0x3e61bee4`), so the pin
    /// fails under `f32::sin_cos` on this host.
    #[test]
    fn the_sample_pattern_uses_the_software_trig() {
        let base = f32::from_bits(0x3c00_0111);
        let pattern = sample_pattern(base, 7, 2);
        assert_eq!(pattern.len(), 1 + 7 * 2);
        assert_eq!(pattern[0], (0.0, 0.0));
        assert_eq!(pattern[8].0.to_bits(), 0x3ee5_c5f1);
        assert_eq!(
            pattern[8].1.to_bits(),
            0x3e61_bee4,
            "sin must be the kernel's bit"
        );

        // The base angle the sampler derives with `det_atan2` is pinned too (one ulp below this
        // host's glibc `atan2f` at the same inputs).
        let base_angle = det_atan2(f32::from_bits(0x3d8d_6b23), 1.0);
        assert_eq!(base_angle.to_bits(), 0x3d8d_31c2);
        let pattern = sample_pattern(base_angle, 7, 2);
        let step = std::f32::consts::TAU / 7.0;
        for ring in 0..2 {
            let r = (2 - ring) as f32 / 2.0;
            let offset = if ring % 2 == 1 { step * 0.5 } else { 0.0 };
            for k in 0..7 {
                let a = base_angle + offset + k as f32 * step;
                let (sin, cos) = det_sin_cos(a);
                assert_eq!(pattern[1 + ring * 7 + k], (cos * r, sin * r));
            }
        }
    }

    /// H1: one avoiding agent stepped once is digest-stable run to run (the digest feeds replay
    /// checksums, so a single avoided step must not wobble).
    #[test]
    fn one_avoided_step_is_digest_stable() {
        let mesh = {
            let tris = vec![
                [
                    Vec3::new(-8.0, 0.0, -8.0),
                    Vec3::new(8.0, 0.0, -8.0),
                    Vec3::new(8.0, 0.0, 8.0),
                ],
                [
                    Vec3::new(-8.0, 0.0, -8.0),
                    Vec3::new(8.0, 0.0, 8.0),
                    Vec3::new(-8.0, 0.0, 8.0),
                ],
            ];
            let mut params = crate::NavMeshBuildParams {
                cell_size: 0.5,
                ..Default::default()
            };
            params.agent.radius = 0.0;
            NavMeshBuilder::new(params).build(&tris).expect("builds")
        };
        let build = || {
            let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
            let a = crowd
                .add_agent(
                    &mesh,
                    Vec3::new(-2.0, 0.0, 0.0),
                    crate::CrowdAgentParams::default(),
                )
                .expect("adds");
            crowd
                .set_target(&mesh, a, Vec3::new(4.0, 0.0, 0.0))
                .expect("target");
            let b = crowd
                .add_agent(
                    &mesh,
                    Vec3::new(0.0, 0.0, 0.5),
                    crate::CrowdAgentParams::default(),
                )
                .expect("adds");
            crowd
                .set_target(&mesh, b, Vec3::new(0.0, 0.0, -4.0))
                .expect("target");
            crowd.step(&mesh);
            crowd.digest()
        };
        let d1 = build();
        let d2 = build();
        assert_eq!(d1, d2, "the same avoided step gives the same digest");
        assert_ne!(d1, 0);
    }

    /// A floor with area 3 painted over `x >= 2`.
    fn floor_with_area() -> NavMesh {
        let tris = vec![
            [
                Vec3::new(-8.0, 0.0, -8.0),
                Vec3::new(8.0, 0.0, -8.0),
                Vec3::new(8.0, 0.0, 8.0),
            ],
            [
                Vec3::new(-8.0, 0.0, -8.0),
                Vec3::new(8.0, 0.0, 8.0),
                Vec3::new(-8.0, 0.0, 8.0),
            ],
        ];
        let mut params = crate::NavMeshBuildParams {
            cell_size: 0.5,
            ..Default::default()
        };
        params.agent.radius = 0.0;
        let volume = crate::NavAreaVolume::new(
            crate::NavObstacleShape::Box {
                min: Vec3::new(2.0, -1.0, -9.0),
                max: Vec3::new(9.0, 1.0, 9.0),
            },
            3,
        );
        NavMeshBuilder::new(params)
            .build_with_areas(&tris, &[volume])
            .expect("builds")
    }

    #[test]
    fn a_walk_stops_at_a_left_out_area_and_passes_otherwise() {
        let mesh = floor_with_area();
        let (start_poly, start) =
            find_nearest_on(&mesh, Vec3::new(0.0, 0.0, 0.0), Vec3::splat(1.0)).expect("on the mesh");
        let end = Vec3::new(5.0, 0.0, 0.0);
        let open = move_along_surface(&mesh, start_poly, start, end, 0);
        assert!(open.reached);
        assert_eq!(open.position.x, 5.0);
        let barred = move_along_surface(&mesh, start_poly, start, end, 1 << 3);
        assert!(!barred.reached);
        assert!(
            barred.position.x <= 2.0,
            "stopped at the edge: {:?}",
            barred.position
        );
    }

    #[test]
    fn the_edge_of_a_left_out_area_is_a_wall() {
        let mesh = floor_with_area();
        let near = Vec3::new(1.5, 0.0, 0.0);
        let on_edge = |walls: &[WallSegment]| {
            walls
                .iter()
                .any(|w| (w.a.x - 2.0).abs() < 1e-4 && (w.b.x - 2.0).abs() < 1e-4)
        };
        assert!(!on_edge(&collect_walls(&mesh, near, 1.0, 2.0, 0)));
        assert!(on_edge(&collect_walls(&mesh, near, 1.0, 2.0, 1 << 3)));
    }

    /// A flat floor `[-half, half]²` with 1 m cells and a zero-radius build agent.
    fn floor_mesh(half: f32) -> NavMesh {
        let tris = vec![
            [
                Vec3::new(-half, 0.0, -half),
                Vec3::new(half, 0.0, -half),
                Vec3::new(half, 0.0, half),
            ],
            [
                Vec3::new(-half, 0.0, -half),
                Vec3::new(half, 0.0, half),
                Vec3::new(-half, 0.0, half),
            ],
        ];
        let mut params = crate::NavMeshBuildParams {
            cell_size: 0.5,
            ..Default::default()
        };
        params.agent.radius = 0.0;
        NavMeshBuilder::new(params).build(&tris).expect("builds")
    }

    #[test]
    fn merge_moved_keeps_the_rest_of_the_corridor() {
        // Walked forward from 1 into 2: the corridor now starts at 2.
        let mut corridor = vec![1, 2, 3, 4];
        merge_moved(&mut corridor, &[1, 2]);
        assert_eq!(corridor, vec![2, 3, 4]);
        // Stepped off the corridor into 9: it now leads back through 2.
        let mut corridor = vec![2, 3, 4];
        merge_moved(&mut corridor, &[2, 9]);
        assert_eq!(corridor, vec![9, 2, 3, 4]);
        // No common polygon: unchanged.
        let mut corridor = vec![2, 3];
        merge_moved(&mut corridor, &[7, 8]);
        assert_eq!(corridor, vec![2, 3]);
    }

    #[test]
    fn merge_shortcut_replaces_the_corridor_start() {
        // A straight walk 1 → 5 → 4 skips 2 and 3.
        let mut corridor = vec![1, 2, 3, 4, 6];
        assert!(merge_shortcut(&mut corridor, &[1, 5, 4]));
        assert_eq!(corridor, vec![1, 5, 4, 6]);
        // A walk that rejoins at the start changes nothing.
        let mut corridor = vec![1, 2, 3];
        assert!(!merge_shortcut(&mut corridor, &[1, 7]));
        assert_eq!(corridor, vec![1, 2, 3]);
    }

    #[test]
    fn a_surface_walk_reaches_open_points_and_clamps_at_walls() {
        let mesh = floor_mesh(4.0);
        let (start_poly, start) = find_nearest_on(&mesh, Vec3::ZERO, Vec3::splat(1.0)).expect("on the floor");
        let inside = walk_segment(&mesh, start_poly, start, Vec3::new(2.0, 0.0, 1.0), 0);
        assert!(inside.reached);
        assert_eq!(inside.visited.first().copied(), Some(start_poly));
        assert_eq!(inside.position, Vec3::new(2.0, 0.0, 1.0));
        let out = move_along_surface(&mesh, start_poly, start, Vec3::new(9.0, 0.0, 1.0), 0);
        assert!(!out.reached, "the floor ends at x = 4");
        assert!(
            out.position.x <= 4.0 + 1e-4 && out.position.x > 3.5,
            "{:?}",
            out.position
        );
        assert!(
            (out.position.z - 1.0).abs() < 1e-3,
            "slides along the wall: {:?}",
            out.position
        );
    }

    #[test]
    fn sweep_and_ray_helpers() {
        // Two unit circles 4 m apart, closing at 1 m/s, touch after 2 s and separate after 6 s.
        let hit = sweep_circles(Vec3::ZERO, 1.0, Vec3::X, Vec3::new(4.0, 0.0, 0.0), 1.0).expect("hits");
        assert!(
            (hit.0 - 2.0).abs() < 1e-5 && (hit.1 - 6.0).abs() < 1e-5,
            "{hit:?}"
        );
        assert!(sweep_circles(Vec3::ZERO, 1.0, Vec3::Z, Vec3::new(4.0, 0.0, 0.0), 1.0).is_none());
        assert!(sweep_circles(Vec3::ZERO, 1.0, Vec3::ZERO, Vec3::new(4.0, 0.0, 0.0), 1.0).is_none());
        // A ray of 2 m/s toward a wall 1 m away crosses it at t = 0.5 s.
        let t = ray_segment(
            Vec3::ZERO,
            Vec3::new(2.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, -1.0),
            Vec3::new(1.0, 0.0, 1.0),
        );
        assert_eq!(t, Some(0.5));
        assert_eq!(
            ray_segment(
                Vec3::ZERO,
                Vec3::new(0.5, 0.0, 0.0),
                Vec3::new(1.0, 0.0, -1.0),
                Vec3::new(1.0, 0.0, 1.0)
            ),
            None,
            "out of reach within 1 s"
        );
    }

    #[test]
    fn walls_are_the_polygon_edges_without_portals() {
        let mesh = floor_mesh(4.0);
        let near_edge = Vec3::new(3.5, 0.0, 0.0);
        let walls = collect_walls(&mesh, near_edge, 1.0, 2.0, 0);
        assert!(!walls.is_empty(), "the floor's +X edge is a wall");
        for w in &walls {
            let d = dist_point_segment_sq(near_edge, w.a, w.b).sqrt();
            assert!(d <= 1.0 + 1e-4);
            // The normal points off the walkable side.
            assert!((near_edge - w.a).dot(w.normal) <= 1e-4, "{w:?}");
        }
        assert!(
            walls
                .iter()
                .all(|w| (w.a.x - 4.0).abs() < 1e-4 || (w.a.z.abs() - 4.0).abs() < 1e-4)
        );
        // In the middle of the floor no wall is within reach.
        assert!(collect_walls(&mesh, Vec3::ZERO, 1.0, 2.0, 0).is_empty());
    }

    #[test]
    fn the_sampler_prefers_the_desired_velocity_when_nothing_is_near() {
        let mesh = floor_mesh(4.0);
        let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
        let id = crowd
            .add_agent(&mesh, Vec3::new(-2.0, 0.0, 0.0), CrowdAgentParams::default())
            .expect("adds");
        crowd
            .set_target(&mesh, id, Vec3::new(3.0, 0.0, 0.0))
            .expect("target");
        crowd.step(&mesh);
        let a = crowd.agent(id).expect("agent");
        assert!(
            a.velocity().x > 0.0 && a.velocity().z.abs() < 1e-3,
            "{:?}",
            a.velocity()
        );
        let step_dv = a.params().max_acceleration * crowd.config().step_seconds;
        assert!(a.velocity().length() <= step_dv + 1e-5, "acceleration-limited");
    }

    #[test]
    fn sync_mesh_revision_replans_only_on_change() {
        let mesh = floor_mesh(4.0);
        let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
        let id = crowd
            .add_agent(&mesh, Vec3::new(-2.0, 0.0, 0.0), CrowdAgentParams::default())
            .expect("adds");
        crowd
            .set_target(&mesh, id, Vec3::new(3.0, 0.0, 0.0))
            .expect("target");
        let planned = crowd.stats().replans;
        crowd.sync_mesh_revision(0);
        crowd.sync_mesh_revision(0);
        crowd.step(&mesh);
        assert_eq!(
            crowd.stats().replans,
            planned,
            "first sight of a revision does not replan"
        );
        crowd.sync_mesh_revision(1);
        crowd.step(&mesh);
        assert_eq!(crowd.stats().replans, planned + 1, "a new revision replans once");
    }

    #[test]
    fn proximity_grid_survives_an_enormous_extent() {
        let positions = [Vec3::ZERO, Vec3::new(1e30, 0.0, 1e30)];
        let grid = ProximityGrid::build(&positions, 0.1);
        assert!(grid.width.saturating_mul(grid.height) <= ProximityGrid::MAX_BUCKETS);
        assert_eq!(grid.storage.offsets.last().copied().unwrap_or(0), 2);
    }

    #[test]
    fn arrival_enters_on_the_quarter_radius_floor_and_holds_to_one_radius() {
        let reach = |radius: f32| (radius * 0.25).max(0.01);
        // Radius 0.40: entry at 0.10, hold until just past 0.40. Y is not in the distance.
        assert_eq!(
            arrival_state(CrowdAgentState::Walking, 0.10 * 0.10, 0.40, false),
            CrowdAgentState::Arrived
        );
        assert_eq!(
            arrival_state(CrowdAgentState::Arrived, 0.20 * 0.20, 0.40, false),
            CrowdAgentState::Arrived,
            "0.20 m is inside the hold band"
        );
        assert_eq!(
            arrival_state(CrowdAgentState::Arrived, 0.41 * 0.41, 0.40, false),
            CrowdAgentState::Walking
        );
        // Same XZ, a goal 0.40 m higher: the caller passes horizontal distance 0.
        assert_eq!(
            arrival_state(CrowdAgentState::Walking, 0.0, 0.40, false),
            CrowdAgentState::Arrived
        );
        // Radius 0.02: the 0.01 m floor, not radius/4 (0.005).
        assert!(reach(0.02) >= 0.01);
        assert_eq!(
            arrival_state(CrowdAgentState::Walking, 0.008 * 0.008, 0.02, false),
            CrowdAgentState::Arrived
        );
        assert_eq!(
            arrival_state(CrowdAgentState::Walking, 0.011 * 0.011, 0.02, false),
            CrowdAgentState::Walking
        );
    }

    /// A radius below the 0.01 m entry floor is valid (`radius > 0`). Entry is then wider than
    /// the hold (`radius`), so one distance arrives on this tick and leaves on the next.
    #[test]
    fn a_radius_below_the_arrival_floor_does_not_flicker() {
        let radius = 0.004;
        let dist = 0.008_f32;
        let d2 = dist * dist;
        let once = arrival_state(CrowdAgentState::Walking, d2, radius, false);
        let twice = arrival_state(once, d2, radius, false);
        assert_eq!(
            once,
            CrowdAgentState::Arrived,
            "0.008 m is inside the 0.01 m floor"
        );
        assert_eq!(
            twice, once,
            "the same distance must not walk again on the next tick, got {twice:?}"
        );
        let waiting = arrival_state(CrowdAgentState::Walking, d2, radius, true);
        assert_eq!(waiting, CrowdAgentState::AwaitingTile);
        assert_eq!(
            arrival_state(waiting, d2, radius, true),
            CrowdAgentState::AwaitingTile,
            "a tile wait must not drop back to walking at the same distance"
        );
    }

    #[test]
    fn a_point_inside_the_default_snap_box_is_on_the_mesh() {
        let mut build = crate::NavMeshBuildParams {
            cell_size: 0.25,
            border_size: 0.0,
            ..Default::default()
        };
        build.agent.radius = 0.0;
        let tris = [
            [
                Vec3::new(-0.5, 0.0, -0.5),
                Vec3::new(0.5, 0.0, -0.5),
                Vec3::new(0.5, 0.0, 0.5),
            ],
            [
                Vec3::new(-0.5, 0.0, -0.5),
                Vec3::new(0.5, 0.0, 0.5),
                Vec3::new(-0.5, 0.0, 0.5),
            ],
        ];
        let mesh = NavMeshBuilder::new(build).build(&tris).expect("floor");
        let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
        // Distance from the origin is about 2.69 m, and each axis is under the default 2 m box.
        let id = crowd
            .add_agent(&mesh, Vec3::new(1.9, 0.0, 1.9), CrowdAgentParams::default())
            .expect("inside the snap box");
        assert!(crowd.agent(id).is_some());
        let err = crowd
            .add_agent(&mesh, Vec3::new(2.6, 0.0, 0.0), CrowdAgentParams::default())
            .expect_err("outside the box on X");
        assert_eq!(err, NavCrowdError::OffMesh);
        assert_eq!(err.code(), "nav_off_mesh");
    }

    /// Item 2 of lane w10-nav-followups: pairing copies of one crossing (same endpoints,
    /// direction, user id and kind) when the link list changes.
    #[test]
    fn pair_links_keeps_copies_in_use_and_prefers_unchanged_copies() {
        let c = OffMeshLink::new(Vec3::new(-1.5, 0.0, 0.0), Vec3::new(1.5, 0.0, 0.0)).with_user_id(9);
        let mut off = c;
        off.enabled = false;
        let other = OffMeshLink::new(Vec3::new(5.0, 0.0, 0.0), Vec3::new(6.0, 0.0, 0.0)).with_user_id(9);
        let used = |v: &[usize]| {
            v.iter()
                .map(|&i| (i, 0u8))
                .collect::<std::collections::BTreeMap<usize, u8>>()
        };
        // Two copies in use, one leaves: the first in use keeps it, the other is gone.
        assert_eq!(
            pair_links(&[c, c], &[c], &used(&[0, 1])),
            vec![(0, Some(0)), (1, None)]
        );
        // One of two identical copies leaves: the one in use keeps the copy left.
        assert_eq!(pair_links(&[c, c], &[c], &used(&[1])), vec![(1, Some(0))]);
        assert_eq!(
            pair_links(&[other, c, c], &[c, other], &used(&[0, 2])),
            vec![(0, Some(1)), (2, Some(0))]
        );
        // Reordered beside a switched-off copy: each pairs with its unchanged self.
        assert_eq!(
            pair_links(&[off, c], &[c, off], &used(&[0, 1])),
            vec![(0, Some(1)), (1, Some(0))]
        );
        // Switched off in place: still the same link.
        assert_eq!(pair_links(&[c], &[off], &used(&[0])), vec![(0, Some(0))]);
        // A link switched on while its switched-on copy leaves pairs with what is left.
        assert_eq!(pair_links(&[off, c], &[c], &used(&[0])), vec![(0, Some(0))]);
        // Appending a copy keeps every index (no shift without a reason).
        assert_eq!(
            pair_links(&[c, c], &[c, c, c], &used(&[0, 1])),
            vec![(0, Some(0)), (1, Some(1))]
        );
        // On a shortage a crossing (rank 0) beats a planned landing (rank 2) on an earlier copy.
        let ranked: std::collections::BTreeMap<usize, u8> = [(0, 2u8), (1, 0u8)].into_iter().collect();
        assert_eq!(pair_links(&[c, c], &[c], &ranked), vec![(0, None), (1, Some(0))]);
        // Gone and out of range.
        assert_eq!(
            pair_links(&[c, other], &[c], &used(&[1, 7])),
            vec![(1, None), (7, None)]
        );
    }
}
