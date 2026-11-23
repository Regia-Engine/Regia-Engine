// SPDX-License-Identifier: MPL-2.0
//! Crowd lane graphs (ADR-1934): ambient crowds walking authored lanes, with no navmesh query per
//! agent.
//!
//! A [`NavLaneGraph`] is a set of one-way lanes: a polyline with a width, a speed limit and filter
//! bits, plus the lanes it leads to. Junctions are nothing more than lanes that share an end, so a
//! crossing, a merge and a split are all one idea. A two-way pavement is two lanes.
//!
//! A [`NavLaneCrowd`] moves agents along the graph with a fixed step. An agent is a lane, a
//! distance along it, a sideways offset and a speed: stepping it is a walk down a sorted list, so a
//! crowd costs `O(n log n)` and no pathfinding. An agent slows behind the agent ahead in its band,
//! and sidesteps to get past one when the lane is wide enough. At the end of a lane it takes the
//! next lane of its route, or a next lane chosen by a hash of its seed and the lane, so a run
//! replays exactly.
//!
//! A route is an A* search over lanes ([`NavLaneGraph::find_route`]), cost by length over speed
//! limit, so a game can send one agent somewhere and leave the others wandering. A
//! [`crate::NavCrowd`] agent can walk a route too: [`NavLaneGraph::route_points`] gives the
//! waypoints.
//!
//! Lane preference and opposing flow (ADR-2133, `.navlanes.ron` v2): a lane's `weight` makes a
//! wanderer pick it more often at a junction (in proportion to the weights of the open choices)
//! and makes routes through it cheaper; an agent's `prefer_flags` makes it take a lane sharing
//! those bits whenever one is open. Two lanes over the same strip in opposite directions are paired
//! with `opposite`: an agent then sees the agents of the other lane coming toward it and keeps to
//! its right to let them by. With every weight 1, no preference and no pairing, a crowd steps
//! exactly as with v1 data.
//!
//! Lanes are meant to lie on the navmesh; [`NavLaneGraph::check_on_mesh`] finds the points that do
//! not. In a streamed world, [`NavLaneCrowd::sync_streaming`] freezes the agents on tiles that left
//! ([`NavLaneAgentState::Dormant`]), as [`crate::NavCrowd::sync_streaming`] does.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::streaming::OriginShifted;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::agent::NavAgentState;
use crate::crowd::NavCrowdStep;
use crate::mesh::NavMesh;
use crate::query::find_nearest_on;
use crate::region_queue::NavRegionStep;
use crate::tile_cache::NavTileCache;

/// The current `.navlanes.ron` version (2 adds `weight` and `opposite`, ADR-2133).
pub const NAV_LANES_FORMAT_VERSION: u32 = 2;

/// The most lanes one graph holds.
pub const MAX_NAV_LANES: usize = 65_536;

/// The most points one lane holds.
pub const MAX_LANE_POINTS: usize = 4096;

/// The most agents one lane crowd holds.
pub const MAX_LANE_AGENTS: usize = 1 << 20;

/// The default filter bit of a lane.
pub const NAV_LANE_WALK: u16 = 1;

/// Why a lane operation failed. [`Self::code`] is the stable agent-protocol reason.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum NavLaneError {
    /// A lane definition is invalid.
    #[error("invalid lane {index}: {reason}")]
    InvalidLane {
        /// The lane's index in its list.
        index: usize,
        /// What is wrong.
        reason: &'static str,
    },
    /// More than `MAX_NAV_LANES` lanes.
    #[error("too many lanes: {0} (the maximum is 65536)")]
    TooManyLanes(usize),
    /// A `.navlanes.ron` text that is not valid RON for the format.
    #[error("cannot parse the lane file: {0}")]
    Parse(String),
    /// A file `format_version` newer than this build reads.
    #[error("lane file format_version {found} is newer than the supported {supported}")]
    Version {
        /// The file's version.
        found: u32,
        /// The newest version this build reads.
        supported: u32,
    },
    /// No lane has this index.
    #[error("no lane {0}")]
    UnknownLane(u32),
    /// No agent has this id.
    #[error("no lane agent with id {0}")]
    UnknownAgent(NavLaneAgentId),
    /// An agent's parameters or position is invalid.
    #[error("invalid lane agent: {0}")]
    InvalidAgent(&'static str),
    /// A lane crowd setting is invalid.
    #[error("invalid lane crowd config: {0}")]
    InvalidConfig(&'static str),
    /// The crowd already holds `max_agents` agents.
    #[error("the lane crowd is full")]
    TooManyAgents,
    /// No route between the lanes for this agent's flags.
    #[error("no lane route from {from} to {to}")]
    NoRoute {
        /// The start lane.
        from: u32,
        /// The goal lane.
        to: u32,
    },
    /// A bad elapsed time.
    #[error("elapsed time must be finite and non-negative")]
    InvalidTime,
    /// A lane file could not be read (ADR-2420): missing, unreadable, larger than
    /// `MAX_NAV_FILE_BYTES`, or not UTF-8.
    #[error("cannot read the lane file: {0}")]
    Io(String),
}

impl NavLaneError {
    /// The stable lowercase reason code (agent protocol `data.reason`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidLane { .. } | Self::TooManyLanes(_) => "nav_invalid_lane",
            Self::Parse(_) => "nav_lane_parse",
            Self::Version { .. } => "nav_lane_version",
            Self::UnknownLane(_) => "nav_unknown_lane",
            Self::UnknownAgent(_) => "nav_unknown_lane_agent",
            Self::InvalidAgent(_) => "nav_invalid_lane_agent",
            Self::InvalidConfig(_) => "nav_invalid_lane_config",
            Self::TooManyAgents => "nav_too_many_lane_agents",
            Self::NoRoute { .. } => "nav_no_lane_route",
            Self::InvalidTime => "nav_invalid_input",
            Self::Io(_) => "nav_lane_io",
        }
    }
}

/// One lane as authored: walked from the first point to the last.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NavLaneDef {
    /// The game's id (default 0 = unnamed).
    #[serde(default)]
    pub user_id: u64,
    /// The centre line `(x, y, z)`, at least two points.
    pub points: Vec<(f32, f32, f32)>,
    /// Full width in metres (default 2).
    #[serde(default = "default_width")]
    pub width: f32,
    /// Speed limit in metres per second (default 1.4); an agent walks at the lower of this and
    /// its own speed.
    #[serde(default = "default_speed")]
    pub speed: f32,
    /// Filter bits (default 1).
    #[serde(default = "default_flags")]
    pub flags: u16,
    /// Indices of the lanes this one leads to. Empty, a lane is a dead end.
    #[serde(default)]
    pub next: Vec<u32>,
    /// Preference (default 1, finite and positive; v2): a wanderer at a junction picks among the
    /// open lanes in proportion to their weights, and a route's cost through the lane is its time
    /// divided by the weight.
    #[serde(default = "default_weight")]
    pub weight: f32,
    /// The lane running the other way over the same strip (v2): its agents count as oncoming, and
    /// agents keep right to pass them. Pairing is mutual: naming one side is enough.
    #[serde(default)]
    pub opposite: Option<u32>,
}

fn default_width() -> f32 {
    2.0
}

fn default_weight() -> f32 {
    1.0
}

fn default_speed() -> f32 {
    1.4
}

fn default_flags() -> u16 {
    NAV_LANE_WALK
}

impl NavLaneDef {
    /// A lane along `points` with the default width, speed and flags and nothing after it.
    pub fn new(points: &[Vec3]) -> Self {
        Self {
            user_id: 0,
            points: points.iter().map(|p| (p.x, p.y, p.z)).collect(),
            width: default_width(),
            speed: default_speed(),
            flags: default_flags(),
            next: Vec::new(),
            weight: default_weight(),
            opposite: None,
        }
    }
}

/// The `.navlanes.ron` file root (`docs/formats/navlanes-ron.md`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NavLaneSet {
    /// The format version (1 or 2; [`NAV_LANES_FORMAT_VERSION`] is written).
    pub format_version: u32,
    /// Lanes closer than this at an end are joined by [`NavLaneGraph::build`] (default 0: only
    /// the listed `next` entries count).
    #[serde(default)]
    pub join_distance: f32,
    /// The lanes, in order.
    pub lanes: Vec<NavLaneDef>,
}

impl NavLaneSet {
    /// Parses and checks a `.navlanes.ron` text. Errors: malformed RON (`nav_lane_parse`), a newer
    /// `format_version` (`nav_lane_version`), invalid lanes (`nav_invalid_lane`).
    pub fn parse(text: &str) -> Result<Self, NavLaneError> {
        let set: Self = ron::from_str(text).map_err(|e| NavLaneError::Parse(e.to_string()))?;
        if set.format_version > NAV_LANES_FORMAT_VERSION {
            return Err(NavLaneError::Version {
                found: set.format_version,
                supported: NAV_LANES_FORMAT_VERSION,
            });
        }
        if set.format_version == 0 {
            return Err(NavLaneError::Parse(
                "format_version must be at least 1".to_string(),
            ));
        }
        // The join cannot fail: checking the lanes is enough here, and `graph` joins them.
        NavLaneGraph::checked_lanes(&set.lanes, set.join_distance)?;
        Ok(set)
    }

    /// The set as RON text.
    pub fn to_ron(&self) -> Result<String, NavLaneError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| NavLaneError::Parse(e.to_string()))
    }

    /// The graph of this set.
    pub fn graph(&self) -> Result<NavLaneGraph, NavLaneError> {
        NavLaneGraph::build(&self.lanes, self.join_distance)
    }
}

/// The `from` versions of `.navlanes.ron`'s migration steps: v1 → v2 ([`migrate_nav_lanes`]).
pub fn nav_lanes_migration_sources() -> Vec<u32> {
    vec![1]
}

/// Upgrades a `.navlanes.ron` text to [`NAV_LANES_FORMAT_VERSION`]. v1 → v2 only adds optional
/// fields (`weight`, `opposite`), so a v1 file that loads keeps its text and gets the new version
/// number; comments and order survive. Errors: what [`NavLaneSet::parse`] refuses.
pub fn migrate_nav_lanes(text: &str) -> Result<String, NavLaneError> {
    NavLaneSet::parse(text)?;
    regia_core::version::rewrite_ron_version(text, "format_version", NAV_LANES_FORMAT_VERSION)
        .ok_or_else(|| NavLaneError::Parse("no root `format_version` number".to_string()))
}

/// The serde shape fingerprint of `.navlanes.ron` (hashed by `cargo xtask check-formats`).
pub fn nav_lanes_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<NavLaneSet>(),
        serde_field_names::<NavLaneDef>(),
    ])
}

/// A lane of a built graph.
#[derive(Debug, Clone, PartialEq)]
pub struct NavLane {
    /// The game's id.
    pub user_id: u64,
    /// The centre line.
    pub points: Vec<Vec3>,
    /// Distance along the lane at each point (the first is 0, the last the length).
    pub stations: Vec<f32>,
    /// Full width in metres.
    pub width: f32,
    /// Speed limit in metres per second.
    pub speed: f32,
    /// Filter bits.
    pub flags: u16,
    /// The lanes this one leads to, sorted.
    pub next: Vec<u32>,
    /// Switched off: nothing plans onto it, and an agent reaching it treats it as a dead end.
    pub enabled: bool,
    /// Preference weight (see [`NavLaneDef::weight`]).
    pub weight: f32,
    /// The paired lane running the other way (mutual; see [`NavLaneDef::opposite`]).
    pub opposite: Option<u32>,
}

impl NavLane {
    /// The lane's length in metres.
    pub fn length(&self) -> f32 {
        self.stations.last().copied().unwrap_or(0.0)
    }

    /// The first point.
    fn start(&self) -> Vec3 {
        self.points.first().copied().unwrap_or(Vec3::ZERO)
    }

    /// The last point.
    fn end(&self) -> Vec3 {
        self.points.last().copied().unwrap_or(Vec3::ZERO)
    }

    /// The point `distance` metres along the lane (clamped to it) and the unit direction there.
    pub fn sample(&self, distance: f32) -> (Vec3, Vec3) {
        let len = self.length();
        // A hand-built lane may be malformed (the public fields allow it); answer without a panic.
        if self.points.len() < 2
            || self.stations.len() != self.points.len()
            || !(len >= 0.0 && len.is_finite())
        {
            return (self.start(), Vec3::ZERO);
        }
        let d = if distance.is_nan() {
            0.0
        } else {
            distance.clamp(0.0, len)
        };
        let i = self
            .stations
            .partition_point(|&s| s <= d)
            .clamp(1, self.points.len().saturating_sub(1).max(1));
        let (a, b) = (self.points[i - 1], self.points[i]);
        let (s0, s1) = (self.stations[i - 1], self.stations[i]);
        let t = if s1 > s0 { (d - s0) / (s1 - s0) } else { 0.0 };
        (a + (b - a) * t, (b - a).normalize_or_zero())
    }
}

/// A lane graph (see the module docs). Insert it as a resource for [`NavLanePlugin`].
#[derive(Resource, Debug, Clone, PartialEq, Default)]
pub struct NavLaneGraph {
    lanes: Vec<NavLane>,
}

/// A route over lanes.
#[derive(Debug, Clone, PartialEq)]
pub struct NavLaneRoute {
    /// The lanes, first to last.
    pub lanes: Vec<u32>,
    /// The sum of the lanes' lengths in metres.
    pub length: f32,
}

/// A point of a lane that has no navmesh under it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavLaneGap {
    /// The lane.
    pub lane: u32,
    /// The point's index.
    pub point: usize,
    /// The point.
    pub position: Vec3,
}

/// An open A* entry over lanes.
struct OpenLane {
    f: f32,
    lane: u32,
}

impl PartialEq for OpenLane {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for OpenLane {}
impl PartialOrd for OpenLane {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OpenLane {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .f
            .total_cmp(&self.f)
            .then_with(|| other.lane.cmp(&self.lane))
    }
}

impl NavLaneGraph {
    /// Builds a graph from `defs`. With a positive `join_distance`, a lane also leads to every
    /// other lane starting within that many metres of its end; at 0 only `next` counts. Errors: too many lanes, or
    /// a lane with fewer than two points, a non-finite point, two equal consecutive points, a
    /// width or speed that is not finite and positive, or a `next` outside the list.
    pub fn build(defs: &[NavLaneDef], join_distance: f32) -> Result<Self, NavLaneError> {
        let _span = tracing::debug_span!("nav_lane_graph_build", lanes = defs.len()).entered();
        let mut lanes = Self::checked_lanes(defs, join_distance)?;
        join_lane_ends(&mut lanes, join_distance);
        Ok(Self { lanes })
    }

    /// The lanes of `defs`, checked and with their opposites paired, before any join: everything
    /// [`Self::build`] can refuse, so [`NavLaneSet::parse`] checks a file without paying for the
    /// join.
    fn checked_lanes(defs: &[NavLaneDef], join_distance: f32) -> Result<Vec<NavLane>, NavLaneError> {
        if defs.len() > MAX_NAV_LANES {
            return Err(NavLaneError::TooManyLanes(defs.len()));
        }
        if !(join_distance.is_finite() && join_distance >= 0.0) {
            return Err(NavLaneError::InvalidLane {
                index: 0,
                reason: "join_distance must be finite and not negative",
            });
        }
        let mut lanes = Vec::with_capacity(defs.len());
        for (index, def) in defs.iter().enumerate() {
            let invalid = |reason| NavLaneError::InvalidLane { index, reason };
            if !(2..=MAX_LANE_POINTS).contains(&def.points.len()) {
                return Err(invalid("a lane needs 2 to 4096 points"));
            }
            if !(def.width.is_finite() && def.width > 0.0) {
                return Err(invalid("width must be finite and positive"));
            }
            if !(def.speed.is_finite() && def.speed > 0.0) {
                return Err(invalid("speed must be finite and positive"));
            }
            if def.next.iter().any(|&n| n as usize >= defs.len()) {
                return Err(invalid("next names a lane outside the list"));
            }
            if !(def.weight.is_finite() && def.weight > 0.0) {
                return Err(invalid("weight must be finite and positive"));
            }
            if def
                .opposite
                .is_some_and(|o| o as usize >= defs.len() || o as usize == index)
            {
                return Err(invalid("opposite names itself or a lane outside the list"));
            }
            let points: Vec<Vec3> = def.points.iter().map(|p| Vec3::new(p.0, p.1, p.2)).collect();
            if !points.iter().all(|p| p.is_finite()) {
                return Err(invalid("points must be finite"));
            }
            let mut stations = Vec::with_capacity(points.len());
            let mut along = 0.0f32;
            stations.push(0.0);
            for pair in points.windows(2) {
                let step = (pair[1] - pair[0]).length();
                if step <= 0.0 {
                    return Err(invalid("two consecutive points are equal"));
                }
                along += step;
                stations.push(along);
            }
            let mut next = def.next.clone();
            next.sort_unstable();
            next.dedup();
            lanes.push(NavLane {
                user_id: def.user_id,
                points,
                stations,
                width: def.width,
                speed: def.speed,
                flags: def.flags,
                next,
                enabled: true,
                weight: def.weight,
                opposite: def.opposite,
            });
        }
        pair_opposites(&mut lanes)?;
        Ok(lanes)
    }

    /// Every lane, by index.
    pub fn lanes(&self) -> &[NavLane] {
        &self.lanes
    }

    /// Lane `index`.
    pub fn lane(&self, index: u32) -> Option<&NavLane> {
        self.lanes.get(index as usize)
    }

    /// How many lanes the graph has.
    pub fn len(&self) -> usize {
        self.lanes.len()
    }

    /// Whether the graph has no lanes.
    pub fn is_empty(&self) -> bool {
        self.lanes.is_empty()
    }

    /// The total length of the lanes in metres.
    pub fn total_length(&self) -> f32 {
        self.lanes.iter().map(NavLane::length).sum()
    }

    /// Switches every lane with `user_id` on or off (a closed street); returns how many changed.
    /// Agents on a closed lane walk off the end of it; nothing enters one. An agent stopped because
    /// its next lane closed walks on when it reopens.
    pub fn set_lane_enabled(&mut self, user_id: u64, enabled: bool) -> usize {
        let mut changed = 0;
        for lane in &mut self.lanes {
            if lane.user_id == user_id && lane.enabled != enabled {
                lane.enabled = enabled;
                changed += 1;
            }
        }
        changed
    }

    /// The lane nearest to `point` within `reach` metres, for an agent with `flags` (`u16::MAX` for
    /// any), as `(lane, distance along it, distance away)`. Ties go to the lower lane index.
    pub fn nearest(&self, point: Vec3, reach: f32, flags: u16) -> Option<(u32, f32, f32)> {
        if !(point.is_finite() && reach.is_finite()) {
            return None;
        }
        let mut best: Option<(u32, f32, f32)> = None;
        for (i, lane) in self.lanes.iter().enumerate() {
            if lane.flags & flags == 0 || !lane.enabled {
                continue;
            }
            if lane.stations.len() != lane.points.len() {
                continue;
            }
            for k in 1..lane.points.len() {
                let (a, b) = (lane.points[k - 1], lane.points[k]);
                let ab = b - a;
                let t = ((point - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0);
                let d = (point - (a + ab * t)).length();
                if d <= reach && best.is_none_or(|(.., bd)| d < bd) {
                    let along = lane.stations[k - 1] + (lane.stations[k] - lane.stations[k - 1]) * t;
                    best = Some((i as u32, along, d));
                }
            }
        }
        best
    }

    /// The quickest lane route from `from` to `to` for `flags`: Dijkstra's search with lane length
    /// over speed limit as the cost (`next` may name any lane, so a straight-line heuristic could
    /// overestimate a jump and return a slower route). `from == to` is that lane alone.
    /// Closed lanes and lanes `flags` refuse are not entered; `from` itself may be either. Errors:
    /// an unknown lane, or no route.
    pub fn find_route(&self, from: u32, to: u32, flags: u16) -> Result<NavLaneRoute, NavLaneError> {
        let goal = self.lane(to).ok_or(NavLaneError::UnknownLane(to))?;
        self.lane(from).ok_or(NavLaneError::UnknownLane(from))?;
        let no_route = NavLaneError::NoRoute { from, to };
        let usable = |lane: &NavLane| lane.enabled && lane.flags & flags != 0;
        // The start only has to exist: an agent on a closed lane walks off its end.
        if from != to && !usable(goal) {
            return Err(no_route);
        }
        let mut best = vec![f32::INFINITY; self.lanes.len()];
        let mut came: Vec<Option<u32>> = vec![None; self.lanes.len()];
        let mut open = BinaryHeap::new();
        best[from as usize] = 0.0;
        open.push(OpenLane { f: 0.0, lane: from });
        while let Some(OpenLane { lane: at, .. }) = open.pop() {
            if at == to {
                let mut lanes = vec![to];
                let mut cur = to;
                while let Some(prev) = came[cur as usize] {
                    lanes.push(prev);
                    cur = prev;
                }
                lanes.reverse();
                let length = lanes
                    .iter()
                    .filter_map(|&l| self.lane(l))
                    .map(NavLane::length)
                    .sum();
                return Ok(NavLaneRoute { lanes, length });
            }
            let here = &self.lanes[at as usize];
            let leave = best[at as usize] + here.length() / here.speed / here.weight;
            for &n in &here.next {
                let lane = &self.lanes[n as usize];
                if usable(lane) && leave < best[n as usize] {
                    best[n as usize] = leave;
                    came[n as usize] = Some(at);
                    open.push(OpenLane { f: leave, lane: n });
                }
            }
        }
        Err(no_route)
    }

    /// The route's centre line as waypoints, `offset` metres right of it, for a [`crate::NavCrowd`]
    /// agent. Lanes that do not exist are skipped.
    pub fn route_points(&self, route: &NavLaneRoute, offset: f32) -> Vec<Vec3> {
        let mut out: Vec<Vec3> = Vec::new();
        for lane in route.lanes.iter().filter_map(|&l| self.lane(l)) {
            for (&point, &station) in lane.points.iter().zip(&lane.stations) {
                let (_, dir) = lane.sample(station);
                let p = point + right_of(dir) * offset;
                if out.last().is_none_or(|last| (*last - p).length() > 1e-3) {
                    out.push(p);
                }
            }
        }
        out
    }

    /// The lane points with no navmesh within `reach` metres.
    pub fn check_on_mesh(&self, mesh: &NavMesh, reach: f32) -> Vec<NavLaneGap> {
        let extents = Vec3::splat(reach.max(0.0));
        let mut out = Vec::new();
        for (i, lane) in self.lanes.iter().enumerate() {
            for (k, &p) in lane.points.iter().enumerate() {
                if find_nearest_on(mesh, p, extents).is_none() {
                    out.push(NavLaneGap {
                        lane: i as u32,
                        point: k,
                        position: p,
                    });
                }
            }
        }
        out
    }

    /// Moves every lane by `delta` (the floating-origin step of spec 78 § 12.2).
    pub fn shift_origin(&mut self, delta: Vec3) {
        if !delta.is_finite() {
            return;
        }
        for lane in &mut self.lanes {
            for p in &mut lane.points {
                *p += delta;
            }
        }
    }
}

/// Adds to every lane's `next` each other lane whose start lies within `join_distance` (3D) of its
/// end, then sorts and dedups `next`. Starts are bucketed in a grid of `2 * join_distance` cells,
/// so an end measures only the starts of its own and the 26 neighbouring cells: near-linear in the
/// lane count instead of every pair (finding 8 of sprint-deep-zai-nav-ai). Two points whose f32
/// distance is within `join_distance` are always in neighbouring cells, even after rounding, so
/// the result equals the all-pairs one. Returns how many start-end pairs were measured.
fn join_lane_ends(lanes: &mut [NavLane], join_distance: f32) -> usize {
    // `join_distance` is checked finite and non-negative before this.
    if join_distance <= 0.0 || lanes.is_empty() {
        return 0;
    }
    let cell = (join_distance * 2.0).min(f32::MAX);
    let key = |p: Vec3| -> [i64; 3] { [p.x, p.y, p.z].map(|v| (v / cell).floor() as i64) };
    let mut starts: Vec<([i64; 3], u32)> = lanes
        .iter()
        .enumerate()
        .map(|(j, lane)| (key(lane.start()), j as u32))
        .collect();
    starts.sort_unstable();
    let start_points: Vec<Vec3> = lanes.iter().map(NavLane::start).collect();
    let mut measured = 0;
    let mut cells: Vec<[i64; 3]> = Vec::with_capacity(27);
    for (i, lane) in lanes.iter_mut().enumerate() {
        let end = lane.end();
        let [kx, ky, kz] = key(end);
        cells.clear();
        for dx in -1..=1i64 {
            for dy in -1..=1i64 {
                for dz in -1..=1i64 {
                    cells.push([
                        kx.saturating_add(dx),
                        ky.saturating_add(dy),
                        kz.saturating_add(dz),
                    ]);
                }
            }
        }
        // A saturated key repeats a cell: visit each once.
        cells.sort_unstable();
        cells.dedup();
        for c in &cells {
            let from = starts.partition_point(|(k, _)| k < c);
            for &(k, j) in &starts[from..] {
                if k != *c {
                    break;
                }
                measured += 1;
                if j as usize != i && (start_points[j as usize] - end).length() <= join_distance {
                    lane.next.push(j);
                }
            }
        }
        lane.next.sort_unstable();
        lane.next.dedup();
    }
    measured
}

/// The horizontal direction to the right of `dir`.
/// Makes `opposite` mutual and checks each pair runs the other way over the same strip: each end
/// within half the wider lane's width of the other's start, lengths within 10 %.
fn pair_opposites(lanes: &mut [NavLane]) -> Result<(), NavLaneError> {
    for index in 0..lanes.len() {
        let Some(o) = lanes[index].opposite else {
            continue;
        };
        let o = o as usize;
        let invalid = |reason| NavLaneError::InvalidLane { index, reason };
        match lanes[o].opposite {
            None => lanes[o].opposite = Some(index as u32),
            Some(back) if back as usize == index => {}
            Some(_) => return Err(invalid("opposite pairs with a lane that names another")),
        }
        let (a, b) = (&lanes[index], &lanes[o]);
        let reach = a.width.max(b.width) * 0.5;
        let (la, lb) = (a.length(), b.length());
        if (a.start() - b.end()).length() > reach
            || (a.end() - b.start()).length() > reach
            || (la - lb).abs() > 0.1 * la.max(lb)
        {
            return Err(invalid("opposite must run the other way along the same strip"));
        }
    }
    Ok(())
}

fn right_of(dir: Vec3) -> Vec3 {
    Vec3::new(-dir.z, 0.0, dir.x).normalize_or_zero()
}

/// The movement parameters of a lane agent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavLaneAgentParams {
    /// Body radius in metres (`> 0`).
    pub radius: f32,
    /// Own walking speed in metres per second (`>= 0`).
    pub speed: f32,
    /// Filter bits: the agent uses lanes sharing a bit with these.
    pub flags: u16,
    /// Sideways shift in metres per second while passing (`>= 0`).
    pub side_speed: f32,
    /// Preferred lane bits (default 0, none): a wanderer at a junction takes an open lane sharing
    /// a bit with these whenever there is one (ADR-2133).
    pub prefer_flags: u16,
}

impl Default for NavLaneAgentParams {
    fn default() -> Self {
        Self {
            radius: 0.3,
            speed: 1.4,
            flags: NAV_LANE_WALK,
            side_speed: 0.6,
            prefer_flags: 0,
        }
    }
}

impl NavLaneAgentParams {
    fn validate(&self) -> Result<(), NavLaneError> {
        if !(self.radius.is_finite() && self.radius > 0.0) {
            return Err(NavLaneError::InvalidAgent("radius must be finite and positive"));
        }
        if !(self.speed.is_finite() && self.speed >= 0.0) {
            return Err(NavLaneError::InvalidAgent(
                "speed must be finite and not negative",
            ));
        }
        if !(self.side_speed.is_finite() && self.side_speed >= 0.0) {
            return Err(NavLaneError::InvalidAgent(
                "side_speed must be finite and not negative",
            ));
        }
        Ok(())
    }
}

/// The id of a lane agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NavLaneAgentId(pub u64);

impl std::fmt::Display for NavLaneAgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What a lane agent is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavLaneAgentState {
    /// Walking its lane.
    Walking,
    /// At the end of its route, or of a dead end with nowhere to go.
    Stopped,
    /// On a tile that left the world: frozen until it returns.
    Dormant,
}

impl NavLaneAgentState {
    /// The lowercase name used by the agent protocol.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Walking => "walking",
            Self::Stopped => "stopped",
            Self::Dormant => "dormant",
        }
    }
}

/// One lane agent.
#[derive(Debug, Clone, PartialEq)]
pub struct NavLaneAgent {
    id: NavLaneAgentId,
    params: NavLaneAgentParams,
    lane: u32,
    distance: f32,
    offset: f32,
    speed: f32,
    state: NavLaneAgentState,
    route: Vec<u32>,
    destination: bool,
    seed: u64,
}

impl NavLaneAgent {
    /// The agent's id.
    pub fn id(&self) -> NavLaneAgentId {
        self.id
    }

    /// The movement parameters.
    pub fn params(&self) -> &NavLaneAgentParams {
        &self.params
    }

    /// The lane it is on.
    pub fn lane(&self) -> u32 {
        self.lane
    }

    /// Metres along the lane.
    pub fn distance(&self) -> f32 {
        self.distance
    }

    /// Metres right of the centre line.
    pub fn offset(&self) -> f32 {
        self.offset
    }

    /// Its speed after the last step.
    pub fn speed(&self) -> f32 {
        self.speed
    }

    /// What it is doing.
    pub fn state(&self) -> NavLaneAgentState {
        self.state
    }

    /// The lanes still ahead on its route.
    pub fn route(&self) -> &[u32] {
        &self.route
    }

    /// Whether it heads for a destination (it stops at the end of its route) or wanders.
    pub fn has_destination(&self) -> bool {
        self.destination
    }
}

/// The settings of a [`NavLaneCrowd`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavLaneCrowdConfig {
    /// The fixed step (seconds, `0 < step <= 1`).
    pub step_seconds: f32,
    /// The most agents (`1..=MAX_LANE_AGENTS`).
    pub max_agents: usize,
    /// The most fixed steps one [`NavLaneCrowd::advance`] runs (`0..=MAX_STEPS_PER_ADVANCE`);
    /// leftover time is dropped.
    pub max_steps_per_advance: u32,
    /// How far ahead an agent looks for the agent it follows (metres, `> 0`).
    pub look_ahead: f32,
    /// The gap kept between two bodies (metres, `>= 0`).
    pub min_gap: f32,
    /// How many agents ahead on the lane one agent checks (`>= 1`): the bound on the step's cost
    /// on a packed lane.
    pub max_neighbours: u32,
}

impl Default for NavLaneCrowdConfig {
    fn default() -> Self {
        Self {
            step_seconds: 1.0 / 30.0,
            max_agents: 16 * 1024,
            max_steps_per_advance: 4,
            look_ahead: 4.0,
            min_gap: 0.3,
            max_neighbours: 8,
        }
    }
}

impl NavLaneCrowdConfig {
    fn validate(&self) -> Result<(), NavLaneError> {
        if !(self.step_seconds.is_finite() && self.step_seconds > 0.0 && self.step_seconds <= 1.0) {
            return Err(NavLaneError::InvalidConfig("step_seconds must be within (0, 1]"));
        }
        if self.max_agents == 0 || self.max_agents > MAX_LANE_AGENTS {
            return Err(NavLaneError::InvalidConfig(
                "max_agents must be within 1..=1048576",
            ));
        }
        if !(self.look_ahead.is_finite() && self.look_ahead > 0.0) {
            return Err(NavLaneError::InvalidConfig(
                "look_ahead must be finite and positive",
            ));
        }
        if !(self.min_gap.is_finite() && self.min_gap >= 0.0) {
            return Err(NavLaneError::InvalidConfig(
                "min_gap must be finite and not negative",
            ));
        }
        if self.max_neighbours == 0 {
            return Err(NavLaneError::InvalidConfig("max_neighbours must be at least 1"));
        }
        if self.max_steps_per_advance > crate::crowd::MAX_STEPS_PER_ADVANCE {
            return Err(NavLaneError::InvalidConfig(
                "max_steps_per_advance must be within 0..=3600",
            ));
        }
        Ok(())
    }
}

/// Counters of a lane crowd.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NavLaneCrowdStats {
    /// Fixed steps run.
    pub steps: u64,
    /// Lane changes at junctions.
    pub transitions: u64,
    /// Steps in which an agent was held back by the agent ahead.
    pub slowed: u64,
    /// Agents that stopped for good (route done, dead end).
    pub stopped: u64,
    /// Times an agent went dormant.
    pub dormant_entries: u64,
    /// Steps in which an agent kept right for an oncoming agent of the opposite lane.
    pub oncoming_yields: u64,
}

/// An agent walking onto a new lane, from [`NavLaneCrowd::transitions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavLaneTransition {
    /// The agent.
    pub agent: NavLaneAgentId,
    /// The lane it left.
    pub from: u32,
    /// The lane it entered.
    pub to: u32,
}

/// Agents moving along a [`NavLaneGraph`] (see the module docs). Insert it as a resource with
/// [`NavLanePlugin`] to step it in `FixedUpdate`, or call [`Self::step`] yourself.
#[derive(Resource, Debug, Clone, Default)]
pub struct NavLaneCrowd {
    config: NavLaneCrowdConfig,
    agents: Vec<NavLaneAgent>,
    next_id: u64,
    accumulator: f32,
    stats: NavLaneCrowdStats,
    transitions: Vec<NavLaneTransition>,
    /// Per lane: the agent indices sorted by distance, rebuilt each step.
    by_lane: Vec<Vec<u32>>,
    /// The next-step speed and offset per agent (Jacobi: every agent reads the same positions).
    plan: Vec<(f32, f32)>,
}

/// The agent ahead that holds another back.
#[derive(Debug, Clone, Copy)]
struct Blocker {
    /// The room left between the two, past the gap kept.
    clear: f32,
    /// Its offset.
    offset: f32,
    /// How far apart sideways two bodies must be for one to pass.
    band: f32,
}

/// The offset the nearest way past `blocker` asks for, inside `room` either side of the centre, or
/// `None` when the lane is too narrow on both sides: then the agent queues. Equally near, it goes
/// right.
fn pass_offset(offset: f32, blocker: &Blocker, room: f32) -> Option<f32> {
    let right = blocker.offset + blocker.band;
    let left = blocker.offset - blocker.band;
    match (left >= -room, right <= room) {
        (true, true) if offset - left < right - offset => Some(left),
        (_, true) => Some(right),
        (true, false) => Some(left),
        (false, false) => None,
    }
}

/// Seconds an agent gives itself to close the gap to the one ahead.
const FOLLOW_HEADWAY: f32 = 0.5;

/// The most lanes one agent enters in a single step (bounds a loop of tiny lanes).
const MAX_CROSSINGS_PER_STEP: usize = 64;

/// A 64-bit mix (the finalizer of MurmurHash3, public domain): the deterministic junction choice.
fn mix(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

impl NavLaneCrowd {
    /// An empty crowd. Errors: an invalid config.
    pub fn new(config: NavLaneCrowdConfig) -> Result<Self, NavLaneError> {
        config.validate()?;
        Ok(Self {
            config,
            ..Self::default()
        })
    }

    /// The settings.
    pub fn config(&self) -> &NavLaneCrowdConfig {
        &self.config
    }

    /// The counters.
    pub fn stats(&self) -> NavLaneCrowdStats {
        self.stats
    }

    /// Every agent, in id order.
    pub fn agents(&self) -> &[NavLaneAgent] {
        &self.agents
    }

    /// The agent with `id`.
    pub fn agent(&self, id: NavLaneAgentId) -> Option<&NavLaneAgent> {
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

    /// The lane changes of the last [`Self::step`] or [`Self::advance`], in order.
    pub fn transitions(&self) -> &[NavLaneTransition] {
        &self.transitions
    }

    /// How many agents are dormant.
    pub fn dormant_count(&self) -> usize {
        self.agents
            .iter()
            .filter(|a| a.state == NavLaneAgentState::Dormant)
            .count()
    }

    fn index_of(&self, id: NavLaneAgentId) -> Option<usize> {
        self.agents.binary_search_by_key(&id, |a| a.id).ok()
    }

    /// Adds an agent `distance` metres along `lane`, `offset` metres right of its centre (both
    /// clamped to the lane). `seed` picks its turns at junctions. Errors: an unknown lane, invalid
    /// parameters, a position that is not finite, a full crowd, or a lane the agent's flags refuse.
    pub fn add_agent(
        &mut self,
        graph: &NavLaneGraph,
        lane: u32,
        distance: f32,
        offset: f32,
        params: NavLaneAgentParams,
        seed: u64,
    ) -> Result<NavLaneAgentId, NavLaneError> {
        params.validate()?;
        let on = graph.lane(lane).ok_or(NavLaneError::UnknownLane(lane))?;
        if on.flags & params.flags == 0 {
            return Err(NavLaneError::InvalidAgent("the lane's flags refuse this agent"));
        }
        if !(distance.is_finite() && offset.is_finite()) {
            return Err(NavLaneError::InvalidAgent("distance and offset must be finite"));
        }
        if self.agents.len() >= self.config.max_agents {
            return Err(NavLaneError::TooManyAgents);
        }
        let id = NavLaneAgentId(self.next_id);
        self.next_id += 1;
        self.agents.push(NavLaneAgent {
            id,
            params,
            lane,
            distance: distance.clamp(0.0, on.length()),
            offset: clamp_offset(offset, on.width, params.radius),
            speed: 0.0,
            state: NavLaneAgentState::Walking,
            route: Vec::new(),
            destination: false,
            seed,
        });
        Ok(id)
    }

    /// Removes an agent. Errors: an unknown id.
    pub fn remove_agent(&mut self, id: NavLaneAgentId) -> Result<(), NavLaneError> {
        let i = self.index_of(id).ok_or(NavLaneError::UnknownAgent(id))?;
        self.agents.remove(i);
        Ok(())
    }

    /// Sends an agent to `lane`: it stops at that lane's end. Errors: an unknown agent or lane, or
    /// no route from its lane for its flags.
    pub fn set_destination(
        &mut self,
        graph: &NavLaneGraph,
        id: NavLaneAgentId,
        lane: u32,
    ) -> Result<(), NavLaneError> {
        let i = self.index_of(id).ok_or(NavLaneError::UnknownAgent(id))?;
        let agent = &self.agents[i];
        let route = graph.find_route(agent.lane, lane, agent.params.flags)?;
        let agent = &mut self.agents[i];
        agent.route = route.lanes.into_iter().skip(1).collect();
        agent.destination = true;
        if agent.state == NavLaneAgentState::Stopped {
            agent.state = NavLaneAgentState::Walking;
        }
        Ok(())
    }

    /// Clears an agent's route: it wanders again. Errors: an unknown id.
    pub fn clear_destination(&mut self, id: NavLaneAgentId) -> Result<(), NavLaneError> {
        let i = self.index_of(id).ok_or(NavLaneError::UnknownAgent(id))?;
        let agent = &mut self.agents[i];
        agent.route.clear();
        agent.destination = false;
        if agent.state == NavLaneAgentState::Stopped {
            agent.state = NavLaneAgentState::Walking;
        }
        Ok(())
    }

    /// The agent's world position, or `None` for an unknown id.
    pub fn position(&self, graph: &NavLaneGraph, id: NavLaneAgentId) -> Option<Vec3> {
        self.agent(id).and_then(|a| agent_position(graph, a))
    }

    /// Runs as many fixed steps as fit in `elapsed` seconds plus the carried remainder (at most
    /// `max_steps_per_advance`; time beyond that is dropped). Returns the steps run. Errors: a
    /// negative or non-finite `elapsed`.
    pub fn advance(&mut self, graph: &NavLaneGraph, elapsed: f32) -> Result<u32, NavLaneError> {
        if !(elapsed.is_finite() && elapsed >= 0.0) {
            return Err(NavLaneError::InvalidTime);
        }
        self.transitions.clear();
        self.accumulator += elapsed;
        let step = self.config.step_seconds;
        let mut steps = 0;
        while self.accumulator >= step && steps < self.config.max_steps_per_advance {
            self.run_step(graph);
            self.accumulator -= step;
            steps += 1;
        }
        if self.accumulator >= step {
            self.accumulator %= step;
        }
        Ok(steps)
    }

    /// Advances the crowd by one fixed step.
    pub fn step(&mut self, graph: &NavLaneGraph) {
        self.transitions.clear();
        self.run_step(graph);
    }

    /// One fixed step: every agent plans from the same positions (agent order cannot matter), then
    /// all move, then the ones past a lane's end cross the junction, in id order.
    fn run_step(&mut self, graph: &NavLaneGraph) {
        let _span = tracing::info_span!("nav_lane_crowd_step", agents = self.agents.len()).entered();
        self.stats.steps += 1;
        let dt = self.config.step_seconds;
        self.sort_lanes(graph);
        self.plan.clear();
        self.plan.resize(self.agents.len(), (0.0, 0.0));
        for i in 0..self.agents.len() {
            self.plan[i] = self.plan_agent(graph, i, dt);
        }
        for (i, agent) in self.agents.iter_mut().enumerate() {
            let (speed, offset) = self.plan[i];
            if agent.state != NavLaneAgentState::Walking {
                agent.speed = 0.0;
                continue;
            }
            agent.speed = speed;
            agent.offset = offset;
            agent.distance += speed * dt;
        }
        for i in 0..self.agents.len() {
            self.cross_junction(graph, i);
        }
    }

    /// Fills `by_lane`, each list sorted by distance, ties by id.
    fn sort_lanes(&mut self, graph: &NavLaneGraph) {
        self.by_lane.resize_with(graph.len(), Vec::new);
        for list in &mut self.by_lane {
            list.clear();
        }
        for (i, agent) in self.agents.iter().enumerate() {
            if let Some(list) = self.by_lane.get_mut(agent.lane as usize) {
                list.push(i as u32);
            }
        }
        let agents = &self.agents;
        for list in &mut self.by_lane {
            list.sort_by(|&a, &b| {
                let (a, b) = (&agents[a as usize], &agents[b as usize]);
                a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id))
            });
        }
    }

    /// Agent `i`'s speed and offset for this step: full speed, or the speed that keeps the gap to
    /// the nearest agent ahead whose band overlaps; when one holds it back it sidesteps toward the
    /// freer side of the lane.
    fn plan_agent(&mut self, graph: &NavLaneGraph, i: usize, dt: f32) -> (f32, f32) {
        let a = &self.agents[i];
        let Some(lane) = graph.lane(a.lane) else {
            return (0.0, a.offset);
        };
        if a.state != NavLaneAgentState::Walking {
            return (0.0, a.offset);
        }
        let top = a.params.speed.min(lane.speed);
        let list = self.by_lane.get(a.lane as usize).map_or(&[][..], Vec::as_slice);
        // Everything ahead of this agent in the sorted list: find it by search, walk a bounded span.
        let at = list.partition_point(|&j| {
            let o = &self.agents[j as usize];
            o.distance.total_cmp(&a.distance).then(o.id.cmp(&a.id)) != Ordering::Greater
        });
        let mut blocked: Option<Blocker> = None;
        let band = |o: &NavLaneAgent| a.params.radius + o.params.radius + self.config.min_gap * 0.5;
        for &j in list[at..].iter().take(self.config.max_neighbours as usize) {
            let o = &self.agents[j as usize];
            let gap = o.distance - a.distance;
            if gap > self.config.look_ahead {
                break;
            }
            if (o.offset - a.offset).abs() >= band(o) {
                continue;
            }
            let clear = gap - a.params.radius - o.params.radius - self.config.min_gap;
            if blocked.is_none_or(|b| clear < b.clear) {
                blocked = Some(Blocker {
                    clear,
                    offset: o.offset,
                    band: band(o),
                });
            }
        }
        // The first agents of the next lane count as ahead too, so a junction does not bunch up.
        if blocked.is_none() {
            let left = lane.length() - a.distance;
            if left < self.config.look_ahead
                && let Some(next) = choose_next(graph, a)
            {
                let list = self.by_lane.get(next as usize).map_or(&[][..], Vec::as_slice);
                // The first agent of the next lane whose band overlaps this one's counts as ahead.
                for &j in list.iter().take(self.config.max_neighbours as usize) {
                    let o = &self.agents[j as usize];
                    let gap = left + o.distance;
                    if gap > self.config.look_ahead {
                        break;
                    }
                    if (o.offset - a.offset).abs() >= band(o) {
                        continue;
                    }
                    blocked = Some(Blocker {
                        clear: gap - a.params.radius - o.params.radius - self.config.min_gap,
                        offset: o.offset,
                        band: band(o),
                    });
                    break;
                }
            }
        }
        let room = (lane.width * 0.5 - a.params.radius).max(0.0);
        let step = a.params.side_speed * dt;
        // Oncoming agents of the paired lane: keep right of the nearest one in the way.
        let keep_right = lane
            .opposite
            .and_then(|o| self.oncoming_offset(graph, lane, o, i))
            .map(|goal| goal.clamp(-room, room));
        self.stats.oncoming_yields += u64::from(keep_right.is_some());
        let Some(blocker) = blocked else {
            let offset = keep_right.map_or(a.offset, |goal| a.offset + (goal - a.offset).clamp(-step, step));
            return (top, offset);
        };
        let mut offset = a.offset;
        if let Some(goal) = keep_right {
            offset = a.offset + (goal - a.offset).clamp(-step, step);
        } else if blocker.clear < a.params.speed * 2.0
            && let Some(goal) = pass_offset(a.offset, &blocker, room)
        {
            offset = a.offset + (goal - a.offset).clamp(-step, step);
        }
        // One headway of time to close the gap: the agent eases in and stops short.
        let can = (blocker.clear / FOLLOW_HEADWAY.max(dt)).max(0.0);
        self.stats.slowed += u64::from(can < top);
        (can.min(top), offset)
    }

    /// For agent `i` on `lane`, paired with lane `opposite`: the offset right of the nearest
    /// oncoming agent whose band overlaps it, within twice the look ahead (the two close at both
    /// speeds), or `None` when none is in the way. Distances map across the pair by length.
    fn oncoming_offset(&self, graph: &NavLaneGraph, lane: &NavLane, opposite: u32, i: usize) -> Option<f32> {
        let a = &self.agents[i];
        let other = graph.lane(opposite)?;
        let list = self.by_lane.get(opposite as usize)?;
        let (la, lb) = (lane.length(), other.length());
        if la <= 0.0 || lb <= 0.0 {
            return None;
        }
        let scale = lb / la;
        let reach = self.config.look_ahead * 2.0;
        // On the other lane, the window ahead of `a` runs from `lb - (d + reach)` up to `lb - d`.
        let (low, high) = ((la - a.distance - reach) * scale, (la - a.distance) * scale);
        let from = list.partition_point(|&j| self.agents[j as usize].distance < low);
        let mut best: Option<(f32, f32)> = None;
        for &j in list[from..].iter().take(self.config.max_neighbours as usize) {
            let o = &self.agents[j as usize];
            if o.distance > high {
                break;
            }
            if o.state != NavLaneAgentState::Walking {
                continue;
            }
            // In `a`'s frame the other walks toward it, mirrored: its right is `a`'s left.
            let along = la - o.distance / scale;
            let side = -o.offset;
            let band = a.params.radius + o.params.radius + self.config.min_gap;
            if (side - a.offset).abs() >= band {
                continue;
            }
            let gap = along - a.distance;
            if best.is_none_or(|(g, _)| gap < g) {
                best = Some((gap, side + band));
            }
        }
        best.map(|(_, goal)| goal)
    }

    /// Agent `i` walked past its lane's end: it takes the next lane, or stops. The distance left
    /// over carries into the next lane, and on through further lanes shorter than it, within the
    /// same step (bounded by `MAX_CROSSINGS_PER_STEP`).
    fn cross_junction(&mut self, graph: &NavLaneGraph, i: usize) {
        let agent = &mut self.agents[i];
        for _ in 0..MAX_CROSSINGS_PER_STEP {
            let Some(lane) = graph.lane(agent.lane) else {
                return;
            };
            // A lane that reopened frees an agent stopped at its junction.
            if agent.state == NavLaneAgentState::Stopped && choose_next(graph, agent).is_some() {
                agent.state = NavLaneAgentState::Walking;
            }
            if agent.state != NavLaneAgentState::Walking || agent.distance < lane.length() {
                return;
            }
            match choose_next(graph, agent) {
                Some(next) => {
                    let from = agent.lane;
                    let over = agent.distance - lane.length();
                    let on = &graph.lanes[next as usize];
                    agent.lane = next;
                    // Kept whole so a long step carries on below; clamped when the loop gives up.
                    agent.distance = over;
                    agent.offset = clamp_offset(agent.offset, on.width, agent.params.radius);
                    if agent.route.first() == Some(&next) {
                        agent.route.remove(0);
                    }
                    self.stats.transitions += 1;
                    self.transitions.push(NavLaneTransition {
                        agent: agent.id,
                        from,
                        to: next,
                    });
                    // Only a remainder that fills the lane just entered crosses again.
                    if over < on.length() {
                        return;
                    }
                }
                None => {
                    agent.distance = lane.length();
                    agent.speed = 0.0;
                    agent.state = NavLaneAgentState::Stopped;
                    self.stats.stopped += 1;
                    return;
                }
            }
        }
        // Out of crossings: the rest is dropped.
        if let Some(lane) = graph.lane(agent.lane) {
            agent.distance = agent.distance.min(lane.length());
        }
    }

    /// Freezes the agents standing on a tile that is not ready and wakes the others (the hook of
    /// spec 78 § 13.2): see [`crate::NavCrowd::sync_streaming`]. An agent walking toward a tile
    /// that left stops when it reaches the tile. [`NavLanePlugin`] calls this.
    pub fn sync_streaming(&mut self, graph: &NavLaneGraph, cache: &NavTileCache) {
        if !cache.is_streamed() {
            return;
        }
        for agent in &mut self.agents {
            if agent.state == NavLaneAgentState::Stopped {
                continue;
            }
            let Some(position) = agent_position(graph, agent) else {
                continue;
            };
            let ready = cache
                .tile_at(position)
                .is_none_or(|(tx, tz)| cache.tile_ready(tx, tz));
            match (ready, agent.state) {
                (false, NavLaneAgentState::Walking) => {
                    agent.state = NavLaneAgentState::Dormant;
                    agent.speed = 0.0;
                    self.stats.dormant_entries += 1;
                }
                (true, NavLaneAgentState::Dormant) => agent.state = NavLaneAgentState::Walking,
                _ => {}
            }
        }
    }

    /// A 64-bit FNV-1a digest of every agent's id, state, lane, distance, offset and speed bits:
    /// two runs with the same inputs give the same digest.
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
            eat(&a.lane.to_le_bytes());
            for v in [a.distance, a.offset, a.speed] {
                eat(&v.to_bits().to_le_bytes());
            }
        }
        h
    }
}

/// The world position of `agent`.
fn agent_position(graph: &NavLaneGraph, agent: &NavLaneAgent) -> Option<Vec3> {
    let lane = graph.lane(agent.lane)?;
    let (p, dir) = lane.sample(agent.distance);
    Some(p + right_of(dir) * agent.offset)
}

/// How far an agent of `radius` may stand from the centre of a lane `width` wide.
fn clamp_offset(offset: f32, width: f32, radius: f32) -> f32 {
    let room = (width * 0.5 - radius).max(0.0);
    offset.clamp(-room, room)
}

/// The lanes `agent` may take after its current one.
fn open_next<'a>(graph: &'a NavLaneGraph, agent: &'a NavLaneAgent) -> impl Iterator<Item = u32> + 'a {
    graph.lane(agent.lane).into_iter().flat_map(move |lane| {
        lane.next.iter().copied().filter(move |&n| {
            graph
                .lane(n)
                .is_some_and(|l| l.enabled && l.flags & agent.params.flags != 0)
        })
    })
}

/// The next lane of `agent`'s route when it is open (none at the end of the route, or when the
/// next lane closed), or for a wanderer a seeded pick among the open ones.
fn choose_next(graph: &NavLaneGraph, agent: &NavLaneAgent) -> Option<u32> {
    if agent.destination {
        let planned = *agent.route.first()?;
        return open_next(graph, agent).find(|&n| n == planned);
    }
    let prefer = agent.params.prefer_flags;
    let preferred = |n: &u32| graph.lane(*n).is_some_and(|l| l.flags & prefer != 0);
    let narrowed = prefer != 0 && open_next(graph, agent).any(|n| preferred(&n));
    let choices = || open_next(graph, agent).filter(move |n| !narrowed || preferred(n));
    let count = choices().count();
    if count == 0 {
        return None;
    }
    let h = mix(agent.seed ^ u64::from(agent.lane).wrapping_mul(0x9e37_79b9_7f4a_7c15));
    let weight = |n: u32| graph.lane(n).map_or(1.0, |l| l.weight);
    let first = choices().next().map_or(1.0, weight);
    if choices().all(|n| weight(n) == first) {
        // Equal weights: the v1 pick, unchanged.
        return choices().nth((h % count as u64) as usize);
    }
    // In proportion to the weights: 53 bits of the hash as a fraction of their sum.
    let total: f64 = choices().map(|n| f64::from(weight(n))).sum();
    let mut left = (h >> 11) as f64 * (1.0 / (1u64 << 53) as f64) * total;
    let mut last = None;
    for n in choices() {
        left -= f64::from(weight(n));
        if left < 0.0 {
            return Some(n);
        }
        last = Some(n);
    }
    last
}

/// Runs a [`NavLaneCrowd`] resource in `FixedUpdate` (in [`NavLaneStep`], after the region and
/// crowd steps), over the [`NavLaneGraph`] resource. Each tick adds one fixed step of time, so the
/// crowd keeps real time at any `step_seconds`. With a [`NavTileCache`] resource it follows
/// streaming too, and rebuilds queued tiles itself unless the app has a [`NavRegionQueue`](crate::NavRegionQueue).
#[derive(Default)]
pub struct NavLanePlugin;

impl Plugin for NavLanePlugin {
    fn build(&self, app: &mut App) {
        crate::origin::ensure(app);
        app.add_systems(
            FixedUpdate,
            step_nav_lanes
                .in_set(NavLaneStep)
                .after(NavRegionStep)
                .after(NavCrowdStep),
        );
    }
}

/// The step of [`NavLanePlugin`].
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NavLaneStep;

/// `FixedUpdate`: one lane-crowd step. Applies any pending origin shift first, through the shared
/// cursor, and moves every [`crate::NavAgentState`] by those deltas. This system may be the one
/// that consumes the cursor, including when no lane crowd is present. Lane agents themselves are
/// a distance along a lane, so the graph shift moves them. The lane crowd never steps between
/// the rebase and the shift.
pub(crate) fn step_nav_lanes(
    lane_crowd: Option<ResMut<NavLaneCrowd>>,
    mut stores: crate::origin::NavStores,
    mut origin_cursor: Option<ResMut<crate::origin::NavOriginShiftCursor>>,
    origin_messages: Option<Res<Messages<OriginShifted>>>,
    mut agents: Query<&mut NavAgentState>,
    fixed: Option<Res<FixedTime>>,
) {
    if let (Some(cursor), Some(messages)) = (origin_cursor.as_deref_mut(), origin_messages.as_deref()) {
        let deltas = crate::origin::apply_origin_shifts(&mut cursor.0, messages, &mut stores);
        crate::origin::shift_agent_states(&deltas, &mut agents);
    }
    let (Some(mut lane_crowd), Some(graph)) = (lane_crowd, stores.lanes) else {
        return;
    };
    if let Some(mut cache) = stores.cache {
        if stores.regions.is_none() {
            cache.update();
        }
        lane_crowd.sync_streaming(&graph, &cache);
    }
    let tick = fixed.map_or(1.0 / 60.0, |f| f.step().as_secs_f32());
    // A tick is finite and positive, so this cannot fail.
    if let Err(e) = lane_crowd.advance(&graph, tick) {
        tracing::warn!(target: "regia::nav", error = %e, "lane crowd step refused");
    }
}

/// The most lanes one road gives in each direction ([`NavRoadLaneParams::lanes_each_way`]).
pub const MAX_ROAD_LANES_EACH_WAY: u32 = 8;

/// How [`NavRoadNetwork::add_road`] turns a road spline into lanes (ADR-2420).
///
/// The carriageway is `width` metres across, centred on the spline (shifted by `center_offset`).
/// It is split into equal lanes: `lanes_each_way` walk with the spline on its right-hand side
/// and, when `two_way`, as many walk against it on the left-hand side. Each lane is its own strip,
/// so none is marked `opposite` (that pairs two directions over one strip). Right is
/// `tangent × +Y`, level whatever the spline's roll or bank.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavRoadLaneParams {
    /// Carriageway width in metres (a road profile's `width_m`, default 7).
    pub width: f32,
    /// Lanes in each direction, `1..=MAX_ROAD_LANES_EACH_WAY` (default 1).
    pub lanes_each_way: u32,
    /// Lanes both ways (default) or only along the spline.
    pub two_way: bool,
    /// Moves every lane sideways by this many metres, positive to the right (default 0): a
    /// pavement beside the carriageway, or a one-way road drawn along its edge.
    pub center_offset: f32,
    /// Speed limit of the lanes in metres per second (default 1.4).
    pub speed: f32,
    /// Filter bits of the lanes (default [`NAV_LANE_WALK`]).
    pub flags: u16,
    /// Preference weight of the lanes (default 1).
    pub weight: f32,
    /// How far a lane may stray from its exact offset curve, metres (default 0.05).
    pub max_error: f32,
    /// The user id of the road's first lane; the others count up from it (default 0: unnamed).
    pub first_user_id: u64,
}

impl Default for NavRoadLaneParams {
    fn default() -> Self {
        Self {
            width: 7.0,
            lanes_each_way: 1,
            two_way: true,
            center_offset: 0.0,
            speed: default_speed(),
            flags: default_flags(),
            weight: default_weight(),
            max_error: 0.05,
            first_user_id: 0,
        }
    }
}

impl NavRoadLaneParams {
    /// Checks the settings. Errors: [`NavLaneError::InvalidConfig`] (`nav_invalid_lane_config`).
    pub fn validate(&self) -> Result<(), NavLaneError> {
        let positive = |v: f32| v.is_finite() && v > 0.0;
        if !positive(self.width) {
            return Err(NavLaneError::InvalidConfig(
                "road width must be finite and positive",
            ));
        }
        if !(1..=MAX_ROAD_LANES_EACH_WAY).contains(&self.lanes_each_way) {
            return Err(NavLaneError::InvalidConfig("lanes_each_way must be 1 to 8"));
        }
        if !self.center_offset.is_finite() {
            return Err(NavLaneError::InvalidConfig("center_offset must be finite"));
        }
        if !positive(self.speed) {
            return Err(NavLaneError::InvalidConfig(
                "road speed must be finite and positive",
            ));
        }
        if !positive(self.weight) {
            return Err(NavLaneError::InvalidConfig(
                "road weight must be finite and positive",
            ));
        }
        if !(self.max_error.is_finite() && self.max_error >= 0.001) {
            return Err(NavLaneError::InvalidConfig("max_error must be at least 0.001"));
        }
        Ok(())
    }

    /// The width of one lane.
    pub fn lane_width(&self) -> f32 {
        let count = if self.two_way {
            2 * self.lanes_each_way
        } else {
            self.lanes_each_way
        };
        self.width / count.max(1) as f32
    }

    /// The sideways offsets of the lanes along the spline, then of the lanes against it, inner
    /// first (positive is right of the spline).
    pub fn offsets(&self) -> (Vec<f32>, Vec<f32>) {
        let w = self.lane_width();
        let n = self.lanes_each_way;
        if self.two_way {
            let along = (0..n)
                .map(|k| self.center_offset + (k as f32 + 0.5) * w)
                .collect();
            let against = (0..n)
                .map(|k| self.center_offset - (k as f32 + 0.5) * w)
                .collect();
            (along, against)
        } else {
            // One way: the lanes fill the carriageway from its left edge to its right edge.
            let left = self.center_offset - self.width * 0.5;
            ((0..n).map(|k| left + (k as f32 + 0.5) * w).collect(), Vec::new())
        }
    }
}

/// Lanes generated from road splines (ADR-2420): a read-only adapter from
/// [`regia_scene::spline::Spline`], the spline component roads are drawn with, to
/// [`NavLaneDef`]s. Nothing here changes the spline.
///
/// Each road adds its lanes at the end of the list ([`Self::add_road`] returns their indices);
/// lanes of one road do not lead into each other, except that a closed spline's lanes are loops.
/// Roads join where their lane ends meet: [`Self::into_set`]'s `join_distance` joins every lane
/// end to the lane starts within that distance, so roads drawn end to start (or meeting at a
/// junction) flow on. A lane follows its offset curve within `max_error`; on a bend tighter than
/// its offset the offset curve folds, and so does the lane: keep the road's curve radius above
/// half its width.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NavRoadNetwork {
    lanes: Vec<NavLaneDef>,
}

impl NavRoadNetwork {
    /// An empty network.
    pub fn new() -> Self {
        Self::default()
    }

    /// The lanes so far, in order.
    pub fn lanes(&self) -> &[NavLaneDef] {
        &self.lanes
    }

    /// Adds the lanes of the road `spline` placed at `pose` (its entity's world transform).
    /// Returns the indices of the new lanes: along the spline first, inner to outer, then
    /// against it. Errors (`nav_invalid_lane_config`): invalid params, an invalid spline or one
    /// with no length, a lane needing more than `MAX_LANE_POINTS` points (raise `max_error`), or
    /// a network past `MAX_NAV_LANES` lanes.
    pub fn add_road(
        &mut self,
        spline: &regia_scene::spline::Spline,
        pose: &regia_core::math::Affine3A,
        params: &NavRoadLaneParams,
    ) -> Result<std::ops::Range<u32>, NavLaneError> {
        let _span = tracing::debug_span!("nav_road_lanes", points = spline.points.len()).entered();
        params.validate()?;
        spline
            .validate()
            .map_err(|_| NavLaneError::InvalidConfig("the road spline is invalid"))?;
        if !pose.is_finite() {
            return Err(NavLaneError::InvalidConfig("the road pose must be finite"));
        }
        let curve = spline.world_curve(pose);
        let length = curve.length();
        if spline.points.len() < 2 || !(length.is_finite() && length > 0.0) {
            return Err(NavLaneError::InvalidConfig("the road spline has no length"));
        }
        let (along, against) = params.offsets();
        let count = along.len() + against.len();
        if self.lanes.len() + count > MAX_NAV_LANES {
            return Err(NavLaneError::TooManyLanes(self.lanes.len() + count));
        }
        let closed = curve.is_closed();
        let mut user_id = params.first_user_id;
        let mut add = |points: Vec<Vec3>, lanes: &mut Vec<NavLaneDef>| -> Result<(), NavLaneError> {
            if points.len() > MAX_LANE_POINTS {
                return Err(NavLaneError::InvalidConfig(
                    "a road lane needs more than 4096 points: raise max_error",
                ));
            }
            if points.len() < 2 {
                return Err(NavLaneError::InvalidConfig("the road spline has no length"));
            }
            let index = lanes.len() as u32;
            let mut def = NavLaneDef::new(&points);
            def.user_id = user_id;
            if params.first_user_id != 0 {
                user_id += 1;
            }
            def.width = params.lane_width();
            def.speed = params.speed;
            def.flags = params.flags;
            def.weight = params.weight;
            if closed {
                def.next = vec![index];
            }
            lanes.push(def);
            Ok(())
        };
        let mut lanes = Vec::with_capacity(count);
        // Built on a scratch list, so a failure leaves the network as it was.
        let base = self.lanes.len() as u32;
        debug_assert!(base as usize + count <= MAX_NAV_LANES);
        let mut scratch: Vec<NavLaneDef> = Vec::new();
        for &offset in &along {
            add(
                offset_polyline(&curve, offset, params.max_error, false),
                &mut scratch,
            )?;
        }
        for &offset in &against {
            add(
                offset_polyline(&curve, offset, params.max_error, true),
                &mut scratch,
            )?;
        }
        for mut def in scratch {
            def.next = def.next.iter().map(|n| n + base).collect();
            lanes.push(def);
        }
        self.lanes.extend(lanes);
        Ok(base..base + count as u32)
    }

    /// The network as a `.navlanes.ron` set, checked by building its graph. Errors: what
    /// [`NavLaneGraph::build`] refuses (a negative `join_distance`).
    pub fn into_set(self, join_distance: f32) -> Result<NavLaneSet, NavLaneError> {
        let set = NavLaneSet {
            format_version: NAV_LANES_FORMAT_VERSION,
            join_distance,
            lanes: self.lanes,
        };
        set.graph()?;
        Ok(set)
    }
}

/// The level sideways unit vector of a spline sample: `tangent × +Y`, or the frame's right
/// vector flattened for a vertical tangent.
fn road_right(sample: &regia_scene::spline::SplineSample) -> Vec3 {
    let r = sample.tangent.cross(Vec3::Y);
    if r.length_squared() > 1.0e-8 {
        r.normalize()
    } else {
        Vec3::new(sample.right.x, 0.0, sample.right.z).normalize_or_zero()
    }
}

/// The curve `offset` metres to the right of `curve` as a polyline within `tol` of it: every
/// segment start is a vertex, and a stretch is halved until its quarter points lie within `tol`
/// of its chord. `reverse` walks it from the end to the start.
fn offset_polyline(
    curve: &regia_scene::spline::SplineCurve,
    offset: f32,
    tol: f32,
    reverse: bool,
) -> Vec<Vec3> {
    let at = |d: f32| {
        let s = curve.at_distance(d);
        s.position + road_right(&s) * offset
    };
    let length = curve.length();
    let mut knots: Vec<f32> = curve.segment_starts();
    knots.push(length);
    knots.dedup();
    let mut sampler = OffsetSampler {
        at: &at,
        tol,
        out: vec![at(knots[0])],
    };
    for pair in knots.windows(2) {
        sampler.split((pair[0], at(pair[0])), (pair[1], at(pair[1])), 0);
    }
    let mut out = sampler.out;
    out.dedup_by(|b, a| (*b - *a).length_squared() <= 1.0e-12);
    if reverse {
        out.reverse();
    }
    out
}

/// The adaptive sampler of [`offset_polyline`].
struct OffsetSampler<'a, F: Fn(f32) -> Vec3> {
    at: &'a F,
    tol: f32,
    out: Vec<Vec3>,
}

impl<F: Fn(f32) -> Vec3> OffsetSampler<'_, F> {
    /// Appends the polyline from `(d0, p0)` to `(d1, p1)` (distance along, point), without its
    /// start.
    fn split(&mut self, (d0, p0): (f32, Vec3), (d1, p1): (f32, Vec3), depth: u32) {
        let fits = depth >= 16
            || [0.25f32, 0.5, 0.75].iter().all(|&f| {
                let q = (self.at)(d0 + (d1 - d0) * f);
                dist_point_segment(q, p0, p1) <= self.tol
            });
        if fits {
            self.out.push(p1);
            return;
        }
        let dm = 0.5 * (d0 + d1);
        let mid = (dm, (self.at)(dm));
        self.split((d0, p0), mid, depth + 1);
        self.split(mid, (d1, p1), depth + 1);
    }
}

/// The 3D distance from `p` to the segment `a → b`.
fn dist_point_segment(p: Vec3, a: Vec3, b: Vec3) -> f32 {
    let ab = b - a;
    let len2 = ab.length_squared();
    let t = if len2 > 0.0 {
        ((p - a).dot(ab) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (a + ab * t - p).length()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(points: Vec<Vec3>, stations: Vec<f32>) -> NavLane {
        NavLane {
            user_id: 0,
            points,
            stations,
            width: 1.0,
            speed: 1.0,
            flags: NAV_LANE_WALK,
            next: Vec::new(),
            enabled: true,
            weight: 1.0,
            opposite: None,
        }
    }

    #[test]
    fn sample_of_a_malformed_lane_does_not_panic() {
        assert_eq!(lane(Vec::new(), Vec::new()).sample(1.0), (Vec3::ZERO, Vec3::ZERO));
        let one = lane(vec![Vec3::X], vec![0.0]);
        assert_eq!(one.sample(1.0), (Vec3::X, Vec3::ZERO));
        let two = vec![Vec3::ZERO, Vec3::X];
        let _ = lane(two.clone(), vec![0.0, f32::NAN]).sample(0.5);
        let _ = lane(two.clone(), vec![0.0, -1.0]).sample(0.5);
        let _ = lane(two.clone(), vec![0.0]).sample(0.5);
        let ok = lane(two, vec![0.0, 1.0]);
        assert_eq!(ok.sample(0.5).0, Vec3::new(0.5, 0.0, 0.0));
        assert_eq!(ok.sample(f32::NAN).0, Vec3::ZERO);
    }

    /// A straight two-point lane from `a` to `b`.
    fn segment(a: Vec3, b: Vec3) -> NavLane {
        lane(vec![a, b], vec![0.0, (b - a).length()])
    }

    /// The joins every pair would make: the reference the grid must match.
    fn all_pairs(lanes: &[NavLane], join_distance: f32) -> Vec<Vec<u32>> {
        lanes
            .iter()
            .enumerate()
            .map(|(i, lane)| {
                let mut next: Vec<u32> = (0..lanes.len())
                    .filter(|&j| j != i && (lanes[j].start() - lane.end()).length() <= join_distance)
                    .map(|j| j as u32)
                    .collect();
                next.extend(&lane.next);
                next.sort_unstable();
                next.dedup();
                next
            })
            .collect()
    }

    /// Finding 8 (sprint-deep-zai-nav-ai): joining lane ends to lane starts measures a bounded
    /// number of pairs per lane (a grid of streets, 4096 lanes), not every pair, and joins exactly
    /// what the all-pairs pass joins.
    #[test]
    fn lane_join_scales_with_the_lane_count_not_its_square() {
        let side = 64usize;
        let mut lanes = Vec::new();
        for z in 0..side {
            for x in 0..side {
                // A street block 10 m long, ends 0.1 m short of the next block's start, with a
                // little height so the y bucket counts too.
                let a = Vec3::new(x as f32 * 10.0, (x % 3) as f32 * 0.05, z as f32 * 10.0);
                lanes.push(segment(a, a + Vec3::new(9.9, 0.0, 0.0)));
            }
        }
        let n = lanes.len();
        assert_eq!(n, 4096);
        let expected = all_pairs(&lanes, 0.25);
        let measured = join_lane_ends(&mut lanes, 0.25);
        assert!(
            measured <= 8 * n,
            "{measured} start-end pairs measured for {n} lanes (all pairs: {})",
            n * (n - 1)
        );
        let got: Vec<Vec<u32>> = lanes.iter().map(|l| l.next.clone()).collect();
        assert_eq!(got, expected);
        // Every block but the last of a row leads to the next one.
        assert_eq!(got.iter().filter(|next| next.len() == 1).count(), n - side);
    }

    /// The grid joins exactly what all pairs join at awkward spots: ends right at the distance,
    /// on cell borders, at negative coordinates, stacked duplicates and a lane joining itself.
    #[test]
    fn lane_join_matches_all_pairs_at_the_edges() {
        let d = 0.5f32;
        let mut points = Vec::new();
        // A deterministic scatter (a fixed linear congruential sequence) in a 6 m box around the
        // origin, so many ends sit within a few join distances of many starts.
        let mut state = 0x2545_f491u32;
        let mut next = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32 * 6.0 - 3.0
        };
        for _ in 0..600 {
            points.push(Vec3::new(next(), next() * 0.1, next()));
        }
        let mut lanes: Vec<NavLane> = points
            .as_chunks::<2>()
            .0
            .iter()
            .filter(|p| p[0] != p[1])
            .map(|p| segment(p[0], p[1]))
            .collect();
        // Exactly at the distance, across a cell border (cells are 2 * d = 1 m), on both signs.
        lanes.push(segment(Vec3::new(-5.0, 0.0, 0.0), Vec3::new(-0.25, 0.0, 0.0)));
        lanes.push(segment(Vec3::new(0.25, 0.0, 0.0), Vec3::new(5.0, 0.0, 0.0)));
        lanes.push(segment(Vec3::new(10.0, 0.0, 0.0), Vec3::new(11.0, 0.0, 0.0)));
        lanes.push(segment(Vec3::new(11.0, 0.0, 0.0), Vec3::new(12.0, 0.0, 0.0)));
        lanes.push(segment(Vec3::new(11.0, 0.0, 0.0), Vec3::new(12.0, 0.0, 0.0)));
        // A loop whose end is its own start: never its own next.
        lanes.push(segment(Vec3::new(20.0, 0.0, 0.0), Vec3::new(20.0, 0.0, 0.0001)));
        // A listed `next` survives the join.
        lanes[0].next = vec![3];
        let expected = all_pairs(&lanes, d);
        join_lane_ends(&mut lanes, d);
        let got: Vec<Vec<u32>> = lanes.iter().map(|l| l.next.clone()).collect();
        assert_eq!(got, expected);
        assert!(got.iter().any(|next| next.len() > 2), "the scatter joins densely");
    }

    #[test]
    fn nearest_skips_a_lane_whose_stations_are_short() {
        let bad = lane(vec![Vec3::ZERO, Vec3::X], vec![0.0]);
        let graph = NavLaneGraph { lanes: vec![bad] };
        assert!(
            graph
                .nearest(Vec3::new(0.5, 0.0, 0.0), 5.0, NAV_LANE_WALK)
                .is_none()
        );
    }
}
