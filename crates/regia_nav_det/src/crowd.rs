// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pathing/crowd.rs, commit c9513623f, tag
// regia-absorb-4), ADR-8012. Plan mark as-is; the row is adapt for named issues (ADR-8448):
// `super::grid` is `crate::grid` and `crate::math` is `regia_detsim::math`; `math::total_cmp`
// is `f32::total_cmp` (`regia_detsim::math` wraps only transcendentals); `f32::min` / `f32::max` /
// `.clamp` are `det_min` / `det_max` / `det_clamp` (det-scan, ADR-8085); the caps below are
// `CrowdSettings` whose defaults are the source values; tunnel mouths and junctions are not scanned
// (W-04/W-08: `NavGrid` has no tunnel layer, `register_choke` is the seam). New code, not from the
// source (KI-WD-030, ADR-10070): `Crowd::register_tunnel_chokes` registers them from a `TunnelLayer`.
// Tests from ir-sim/src/pathing/tests.rs lines 1059-1194, asserts as-is. Named issue: no game state,
// so the cliff grid and the march use `DetAgents` (ADR-8448).

//! Formation slots, choke lanes and slots that bend back to reachable ground (plan row S-05,
//! ADR-8448).
//!
//! An order that names several members and a point is one flow: a shared anchor and a heading
//! from the group's centroid to that anchor. Each member steers to its own slot in that frame
//! through [`crate::agent`], so routing and local avoidance apply underneath.
//!
//! - Slots sit in the group frame (forward = heading, right = heading + 90 degrees), so a column
//!   marches facing its travel. When the leg from a member to its slot has no line of travel, the
//!   slot slides back along the leg ([`Crowd::bend_slot`]).
//! - Near a registered choke the slots narrow to [`LANE_COUNT`] files trailing the anchor.
//!   Tunnel mouths and junctions come from the map's tunnel layer through
//!   [`Crowd::register_tunnel_chokes`] (KI-WD-030, ADR-10070); any other narrow through
//!   [`Crowd::register_choke`].
//! - A flow refreshes from living members and evaporates when idle.
//!
//! Members are sorted by id. Headings use [`regia_detsim::math`]. Choke checks scan in registration
//! order. No time source is read; callers pass `tick`.

use std::collections::BTreeMap;

use regia_detsim::float::{det_clamp, det_max, det_min};
use regia_detsim::math;
use serde::Serialize;

use crate::grid::{COST_STEEP, NavGrid, Profile};
use crate::layer::TunnelLayer;

/// Most flows tracked at once. Past this, new groups keep their caller's own slots; the refusal
/// is counted, never silent.
pub const CROWD_MAX_GROUPS: usize = 64;
/// Most members in one flow.
pub const CROWD_MAX_MEMBERS: usize = 16;
/// Files through a choke point.
pub const LANE_COUNT: usize = 3;
/// Lateral gap between two choke files, metres.
pub const LANE_WIDTH_M: f32 = 8.0;
/// Gap between two slots in file, metres.
pub const SLOT_GAP_M: f32 = 11.0;
/// A choke this close to the group leg switches the slots to lanes, metres.
pub const CHOKE_RADIUS_M: f32 = 60.0;
/// A flow nobody refreshed for this long is dropped (3 s at 60 Hz).
pub const FLOW_IDLE_TICKS: u64 = 180;
/// Fractions back toward a member at which a blocked slot is retried.
const BEND_BACKOFF: [f32; 3] = [0.75, 0.5, 0.25];

/// Caps for [`Crowd`]. Every peer of a lockstep match must use the same values. The defaults are
/// the source constants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrowdSettings {
    /// Most flows tracked at once ([`CROWD_MAX_GROUPS`]).
    pub max_groups: usize,
    /// Most members in one flow ([`CROWD_MAX_MEMBERS`]).
    pub max_members: usize,
    /// Files through a choke ([`LANE_COUNT`]). Must be at least 1.
    pub lane_count: usize,
    /// Lateral gap between two choke files, metres ([`LANE_WIDTH_M`]).
    pub lane_width_m: f32,
    /// Gap between two slots, metres ([`SLOT_GAP_M`]). Also the distance inside which the heading
    /// stops re-aiming: one trailing member pulls the centroid by lag/n, and a dead zone smaller
    /// than a slot keeps re-aiming the frame through that laggard.
    pub slot_gap_m: f32,
    /// A choke this close to the group leg switches the slots to lanes, metres
    /// ([`CHOKE_RADIUS_M`]).
    pub choke_radius_m: f32,
    /// Ticks without a refresh before a flow is dropped ([`FLOW_IDLE_TICKS`]).
    pub flow_idle_ticks: u64,
}

impl Default for CrowdSettings {
    fn default() -> Self {
        Self {
            max_groups: CROWD_MAX_GROUPS,
            max_members: CROWD_MAX_MEMBERS,
            lane_count: LANE_COUNT,
            lane_width_m: LANE_WIDTH_M,
            slot_gap_m: SLOT_GAP_M,
            choke_radius_m: CHOKE_RADIUS_M,
            flow_idle_ticks: FLOW_IDLE_TICKS,
        }
    }
}

/// One live flow as [`Crowd::report`] lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CrowdFlowRow {
    /// The order group's id.
    pub group: u32,
    /// Living members in the flow.
    pub members: usize,
    /// Slots are in choke files.
    pub lane_mode: bool,
}

/// Flows and chokes as the agent protocol reports them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CrowdReport {
    /// Live flows.
    pub groups: usize,
    /// Registered choke points.
    pub chokes: usize,
    /// Flows whose slots are in choke files.
    pub lane_groups: usize,
    /// One row per live flow, ascending group id.
    pub flows: Vec<CrowdFlowRow>,
}

/// One group move: who, where to, and which way the slots face.
#[derive(Debug, Clone, Default)]
pub struct CrowdGroup {
    /// Members in ascending id.
    pub members: Vec<u32>,
    /// The order point, world XZ.
    pub anchor: [f32; 2],
    /// Group forward, radians in the `atan2(dx, dz)` convention.
    pub heading: f32,
    /// Slots are in choke files while this holds.
    pub lane_mode: bool,
    /// Last tick a member refreshed the flow.
    pub last_tick: u64,
    /// Slot order (rank = index), fixed when the flow starts or its membership changes.
    /// Empty means plain id order.
    pub order: Vec<u32>,
    /// Slots per row the order was laid out for (the column count).
    pub order_cols: usize,
    /// Per rank: the member holds short of its slot. Empty until the order exists.
    pub hold: Vec<bool>,
}

/// The crowd layer. A host keeps one on [`crate::agent::DetAgents`].
#[derive(Debug, Clone, Default)]
pub struct Crowd {
    groups: BTreeMap<u32, CrowdGroup>,
    /// Registered narrows, world XZ, sorted.
    chokes: Vec<[f32; 2]>,
    settings: CrowdSettings,
}

impl Crowd {
    /// An empty layer with these settings.
    pub fn new(settings: CrowdSettings) -> Self {
        Self {
            groups: BTreeMap::new(),
            chokes: Vec::new(),
            settings,
        }
    }

    /// The settings.
    pub fn settings(&self) -> &CrowdSettings {
        &self.settings
    }

    /// How many flows are tracked.
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// How many choke points are registered.
    pub fn choke_count(&self) -> usize {
        self.chokes.len()
    }

    /// True when `group` has a live flow.
    pub fn contains(&self, group: u32) -> bool {
        self.groups.contains_key(&group)
    }

    /// True when the group's slots are in choke files.
    pub fn lane_mode(&self, group: u32) -> bool {
        self.groups.get(&group).is_some_and(|g| g.lane_mode)
    }

    /// Flows, chokes and lane mode for the agent protocol.
    pub fn report(&self) -> CrowdReport {
        let flows: Vec<CrowdFlowRow> = self
            .groups
            .iter()
            .map(|(id, g)| CrowdFlowRow {
                group: *id,
                members: g.members.len(),
                lane_mode: g.lane_mode,
            })
            .collect();
        let lane_groups = flows.iter().filter(|row| row.lane_mode).count();
        CrowdReport {
            groups: self.groups.len(),
            chokes: self.chokes.len(),
            lane_groups,
            flows,
        }
    }

    /// Register a narrow (or any door line) as a choke point. Points within a metre of a known
    /// one are the same choke. Tunnel mouths and junctions use this seam (W-04/W-08).
    pub fn register_choke(&mut self, at: [f32; 2]) {
        for c in &self.chokes {
            let dx = c[0] - at[0];
            let dz = c[1] - at[1];
            if dx * dx + dz * dz < 1.0 {
                return;
            }
        }
        self.chokes.push(at);
        self.chokes
            .sort_by(|a, b| total_cmp(a[0], b[0]).then_with(|| total_cmp(a[1], b[1])));
    }

    /// Register every tunnel mouth and junction of `layer` as a choke point (KI-WD-030,
    /// ADR-10070): the top node of each portal (where a ramp meets the surface), in ascending
    /// portal order, then each junction node (where passages join), in ascending node order.
    /// Points within a metre of a known choke are the same choke, so a second call adds nothing.
    /// Returns how many chokes were added. A host calls it once per map after building the layer
    /// ([`TunnelLayer::build`]); [`Self::clear_chokes`] forgets them on a map change.
    pub fn register_tunnel_chokes(&mut self, layer: &TunnelLayer) -> usize {
        let before = self.chokes.len();
        for at in tunnel_choke_points(layer) {
            self.register_choke(at);
        }
        self.chokes.len() - before
    }

    /// Forget every registered choke (map change).
    pub fn clear_chokes(&mut self) {
        self.chokes.clear();
    }

    /// Refresh (or start) the flow for an order group. `members` are ids, `anchor` the order
    /// point, `centroid` the members' mean position. Returns the lane mode. A group under two
    /// living members, or past the group cap, holds no flow and returns false.
    pub fn refresh(
        &mut self,
        group: u32,
        members: &[u32],
        anchor: [f32; 2],
        centroid: [f32; 2],
        choke_near: bool,
        tick: u64,
    ) -> bool {
        let max_members = self.settings.max_members;
        let max_groups = self.settings.max_groups;
        let dead = self.settings.slot_gap_m;
        let mut sorted: Vec<u32> = members.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        sorted.truncate(max_members);
        if sorted.len() < 2 {
            self.groups.remove(&group);
            return false;
        }
        if self.groups.len() >= max_groups && !self.groups.contains_key(&group) {
            return false;
        }
        let dx = anchor[0] - centroid[0];
        let dz = anchor[1] - centroid[1];
        let far = dx * dx + dz * dz > dead * dead;
        match self.groups.get_mut(&group) {
            Some(g) => {
                if g.members != sorted {
                    g.order.clear();
                    g.hold.clear();
                }
                g.members = sorted;
                g.anchor = anchor;
                if far {
                    g.heading = math::atan2(dx, dz);
                }
                g.lane_mode = choke_near;
                g.last_tick = tick;
            }
            None => {
                let heading = if far { math::atan2(dx, dz) } else { 0.0 };
                self.groups.insert(
                    group,
                    CrowdGroup {
                        members: sorted,
                        anchor,
                        heading,
                        lane_mode: choke_near,
                        last_tick: tick,
                        order: Vec::new(),
                        order_cols: 0,
                        hold: Vec::new(),
                    },
                );
            }
        }
        choke_near
    }

    /// Drop one member from its flow; the flow ends under two members.
    pub fn leave(&mut self, group: u32, member: u32) {
        let drop = match self.groups.get_mut(&group) {
            Some(g) => {
                g.members.retain(|id| *id != member);
                g.members.len() < 2
            }
            None => false,
        };
        if drop {
            self.groups.remove(&group);
        }
    }

    /// End a flow outright (order done).
    pub fn disband(&mut self, group: u32) {
        self.groups.remove(&group);
    }

    /// Drop flows nobody refreshed lately. Called from agent upkeep.
    pub fn service(&mut self, tick: u64) {
        let idle = self.settings.flow_idle_ticks;
        self.groups.retain(|_, g| g.last_tick + idle >= tick);
    }

    /// A member's slot as a world XZ point, or `None` without a flow. Rank is the member's index
    /// in the slot order, so slots are stable while the membership holds.
    pub fn slot_offset(&self, group: u32, member: u32) -> Option<[f32; 2]> {
        let g = self.groups.get(&group)?;
        let rank = g.rank_of(member)?;
        let (along, lateral) = layout_offset(rank, g.members.len(), g.lane_mode, &self.settings);
        let (s, c) = (math::sin(g.heading), math::cos(g.heading));
        let along = if g.hold.get(rank).copied().unwrap_or(false) {
            along - HOLD_STANDOFF_M
        } else {
            along
        };
        Some([
            g.anchor[0] + s * along + c * lateral,
            g.anchor[1] + c * along - s * lateral,
        ])
    }

    /// Fix the slot order of a flow that has none yet, from where the members stand now. Ids
    /// alone sent members across each other's paths to the far side of the block. Here the
    /// block's rows go to the members furthest forward and its columns to the members on that
    /// side, so nobody has to cross: along the travel when the group is strung out that way,
    /// across it when the group is a front. Ties fall to the id.
    pub fn assign_by_position(&mut self, group: u32, positions: &[(u32, [f32; 2])]) {
        let settings = self.settings;
        let Some(g) = self.groups.get_mut(&group) else {
            return;
        };
        let n = g.members.len();
        let per_row = if g.lane_mode {
            settings.lane_count
        } else {
            grid_cols(n)
        };
        if !g.order.is_empty() && g.order_cols == per_row {
            g.resort_columns(positions, per_row, &settings);
            return;
        }
        g.order.clear();
        let (s, c) = (math::sin(g.heading), math::cos(g.heading));
        // (id, forward, lateral) relative to the anchor, in the slot frame.
        let mut rows: Vec<(u32, f32, f32)> = Vec::with_capacity(n);
        for id in &g.members {
            let Some((_, p)) = positions.iter().find(|(m, _)| m == id) else {
                return;
            };
            let (dx, dz) = (p[0] - g.anchor[0], p[1] - g.anchor[1]);
            rows.push((*id, s * dx + c * dz, c * dx - s * dz));
        }
        let spread = |f: &dyn Fn(&(u32, f32, f32)) -> f32| {
            let lo = rows.iter().map(f).fold(f32::MAX, det_min);
            let hi = rows.iter().map(f).fold(f32::MIN, det_max);
            hi - lo
        };
        let strung_out = spread(&|r| r.1) >= spread(&|r| r.2);
        let by_fwd = |a: &(u32, f32, f32), b: &(u32, f32, f32)| total_cmp(b.1, a.1).then(a.0.cmp(&b.0));
        let by_lat = |a: &(u32, f32, f32), b: &(u32, f32, f32)| total_cmp(a.2, b.2).then(a.0.cmp(&b.0));
        let mut ranked: Vec<Option<u32>> = vec![None; n];
        if strung_out {
            // Rows by forward distance, each row left to right.
            rows.sort_by(by_fwd);
            for (row, chunk) in rows.chunks_mut(per_row).enumerate() {
                chunk.sort_by(by_lat);
                for (col, r) in chunk.iter().enumerate() {
                    ranked[row * per_row + col] = Some(r.0);
                }
            }
        } else {
            // Columns by lateral position, each column front to back. Column k holds the ranks
            // k, k + per_row, ...
            rows.sort_by(by_lat);
            let mut at = 0usize;
            for col in 0..per_row.min(n) {
                let count = (n - col).div_ceil(per_row);
                let chunk = &mut rows[at..at + count];
                chunk.sort_by(by_fwd);
                for (row, r) in chunk.iter().enumerate() {
                    ranked[row * per_row + col] = Some(r.0);
                }
                at += count;
            }
        }
        if ranked.iter().all(Option::is_some) {
            g.order = ranked.into_iter().flatten().collect();
            g.order_cols = per_row;
        }
    }

    /// True when a choke sits near the leg from the group to its anchor. Registered chokes only:
    /// tunnel mouths and junctions are not on [`NavGrid`] (W-04/W-08). `grid` keeps the call
    /// shape; a host registers those mouths and junctions from its tunnel layer with
    /// [`Self::register_tunnel_chokes`].
    pub fn choke_near(&self, grid: Option<&NavGrid>, centroid: [f32; 2], anchor: [f32; 2]) -> bool {
        let radius = self.settings.choke_radius_m;
        for c in &self.chokes {
            if seg_dist(*c, centroid, anchor) <= radius {
                return true;
            }
        }
        tunnel_mouths_near(grid)
    }

    /// Slide a slot back along the member-to-slot leg until the leg has line of travel, so the
    /// slot bends round the obstacle instead of sitting inside it. Returns the slot and whether
    /// it moved. Off-grid pairs and open legs return the slot unchanged.
    pub fn bend_slot(grid: &NavGrid, profile: Profile, member: [f32; 2], slot: [f32; 2]) -> ([f32; 2], bool) {
        if !grid.in_bounds_world(member[0], member[1]) || !grid.in_bounds_world(slot[0], slot[1]) {
            return (slot, false);
        }
        let from = grid.cell_at(member[0], member[1]);
        let to = grid.cell_at(slot[0], slot[1]);
        if grid.los(profile, from, to, COST_STEEP, 1) {
            return (slot, false);
        }
        for t in BEND_BACKOFF {
            let p = [
                member[0] + (slot[0] - member[0]) * t,
                member[1] + (slot[1] - member[1]) * t,
            ];
            if !grid.in_bounds_world(p[0], p[1]) {
                continue;
            }
            if grid.los(profile, from, grid.cell_at(p[0], p[1]), COST_STEEP, 1) {
                return (p, true);
            }
        }
        (member, true)
    }
}

/// A member this close to its slot has arrived: its rank is no longer swapped.
const SLOT_LOCK_M: f32 = 2.6;
/// A column mate must be this far past a rear slot before the member for that slot drives in
/// (two body widths, metres).
const HOLD_CLEAR_M: f32 = 9.0;
/// How far short of its slot a holding member waits, metres.
const HOLD_STANDOFF_M: f32 = 10.0;

impl CrowdGroup {
    /// World XZ of the slot at `rank` (the formation slot, not a hold standoff).
    fn slot_at(&self, rank: usize, settings: &CrowdSettings) -> [f32; 2] {
        let (along, lateral) = layout_offset(rank, self.members.len(), self.lane_mode, settings);
        let (s, c) = (math::sin(self.heading), math::cos(self.heading));
        [
            self.anchor[0] + s * along + c * lateral,
            self.anchor[1] + c * along - s * lateral,
        ]
    }

    /// Within each column of the block, the member furthest forward takes the front slot. Two
    /// members bound for one column reach it one behind the other, and the one that parks first
    /// must be the one that parks deepest or it seals the way for its mate. Members already on
    /// their slot keep it. A member whose column mate has a deeper slot and has not yet passed
    /// this one holds short of its slot until that mate is through.
    fn resort_columns(&mut self, positions: &[(u32, [f32; 2])], per_row: usize, settings: &CrowdSettings) {
        let n = self.order.len();
        let (s, c) = (math::sin(self.heading), math::cos(self.heading));
        let pos_of = |id: u32| positions.iter().find(|(m, _)| *m == id).map(|(_, p)| *p);
        let fwd_of = |p: [f32; 2], anchor: [f32; 2]| s * (p[0] - anchor[0]) + c * (p[1] - anchor[1]);
        let mut hold = vec![false; n];
        for col in 0..per_row.min(n) {
            let ranks: Vec<usize> = (col..n).step_by(per_row).collect();
            if ranks.len() < 2 {
                continue;
            }
            let mut free_ranks: Vec<usize> = Vec::new();
            let mut free: Vec<(i32, usize, u32)> = Vec::new();
            let mut complete = true;
            for r in &ranks {
                let id = self.order[*r];
                let Some(p) = pos_of(id) else {
                    complete = false;
                    break;
                };
                let slot = self.slot_at(*r, settings);
                let (dx, dz) = (p[0] - slot[0], p[1] - slot[1]);
                if dx * dx + dz * dz <= SLOT_LOCK_M * SLOT_LOCK_M {
                    continue;
                }
                free_ranks.push(*r);
                free.push((-((fwd_of(p, self.anchor)).round() as i32), *r, id));
            }
            if !complete {
                continue;
            }
            if free.len() >= 2 {
                free.sort();
                for (rank, (_, _, id)) in free_ranks.iter().zip(free) {
                    self.order[*rank] = id;
                }
            }
            // Rear slots wait for the mates bound deeper in the column.
            for (i, r) in ranks.iter().enumerate().skip(1) {
                let slot_fwd = fwd_of(self.slot_at(*r, settings), self.anchor);
                hold[*r] = ranks[..i].iter().any(|front| {
                    let id = self.order[*front];
                    let Some(p) = pos_of(id) else {
                        return false;
                    };
                    let slot = self.slot_at(*front, settings);
                    let (dx, dz) = (p[0] - slot[0], p[1] - slot[1]);
                    let arrived = dx * dx + dz * dz <= SLOT_LOCK_M * SLOT_LOCK_M;
                    !arrived && fwd_of(p, self.anchor) < slot_fwd + HOLD_CLEAR_M
                });
            }
        }
        self.hold = hold;
    }

    /// A member's rank in the slot block.
    fn rank_of(&self, member: u32) -> Option<usize> {
        if self.order.len() == self.members.len() {
            self.order.iter().position(|id| *id == member)
        } else {
            self.members.iter().position(|id| *id == member)
        }
    }
}

/// Columns of the open-ground block for `n` members.
fn grid_cols(n: usize) -> usize {
    let mut cols = 1usize;
    while cols * cols < n {
        cols += 1;
    }
    cols
}

/// Slot frame position: `(along, lateral)` metres from the anchor, forward positive. Open ground
/// centres the block on the anchor; choke files trail it so the group files through.
fn layout_offset(rank: usize, n: usize, lane_mode: bool, settings: &CrowdSettings) -> (f32, f32) {
    if lane_mode {
        let lanes = settings.lane_count as f32;
        let file = (rank % settings.lane_count) as f32 - (lanes - 1.0) * 0.5;
        let file_rank = (rank / settings.lane_count) as f32;
        (-file_rank * settings.slot_gap_m, file * settings.lane_width_m)
    } else {
        let cols = grid_cols(n);
        let rows = n.div_ceil(cols);
        let col = (rank % cols) as f32 - (cols - 1) as f32 * 0.5;
        let row = (rank / cols) as f32 - (rows - 1) as f32 * 0.5;
        (-row * settings.slot_gap_m, col * settings.slot_gap_m)
    }
}

/// Distance from `p` to the segment `a`–`b`.
fn seg_dist(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let abx = b[0] - a[0];
    let abz = b[1] - a[1];
    let apx = p[0] - a[0];
    let apz = p[1] - a[1];
    let ab2 = abx * abx + abz * abz;
    let t = if ab2 < 1e-6 {
        0.0
    } else {
        det_clamp((apx * abx + apz * abz) / ab2, 0.0, 1.0)
    };
    let cx = a[0] + abx * t - p[0];
    let cz = a[1] + abz * t - p[1];
    math::sqrt(cx * cx + cz * cz)
}

/// The source's `math::total_cmp`: `f32`'s total order. Not a transcendental, so it is not in
/// [`regia_detsim::math`].
#[inline]
fn total_cmp(a: f32, b: f32) -> std::cmp::Ordering {
    a.total_cmp(&b)
}

/// The XZ points [`Crowd::register_tunnel_chokes`] registers: each portal's tunnel node (the ramp
/// top at the mouth), ascending, then each junction node, ascending. Ids past the node list are
/// skipped.
fn tunnel_choke_points(layer: &TunnelLayer) -> Vec<[f32; 2]> {
    let mouths = layer.portals.iter().map(|(node, _)| *node);
    mouths
        .chain(layer.junctions.iter().copied())
        .filter_map(|node| layer.nodes.get(node as usize))
        .map(|n| [n.x, n.z])
        .collect()
}

/// Tunnel mouths and junctions are not on the surface grid (W-04/W-08): they are registered from
/// the tunnel layer ([`Crowd::register_tunnel_chokes`]). `grid` is accepted so
/// [`Crowd::choke_near`] keeps its call shape; the answer is always false.
fn tunnel_mouths_near(grid: Option<&NavGrid>) -> bool {
    let _surface = grid;
    false
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use regia_detsim::float::det_clamp;
    use regia_detsim::math;

    use super::*;
    use crate::agent::{DetAgentWorld, DetAgentWorldMut, DetAgents, DetBody, DetMover, KinematicMover};
    use crate::grid::{GridGround, HeightCells, Stamp};

    const SIDE: usize = 256;
    const WIDTH: f32 = 1280.0;
    const BASE: u16 = 16384;
    const M_PER_COUNT: f32 = 1.0 / 32.0;
    /// The source's group slot park distance (m).
    const GROUP_SLOT_PARK_M: f32 = 2.5;
    /// The source's battle body radius (m).
    const MOVER_RADIUS_M: f32 = 3.9;

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
        cells.center_height = BASE as f32 * M_PER_COUNT;
        cells
    }

    /// A 60 m wall along x = 0 from z = -`half_len` to z = +`half_len`, with an optional gap of
    /// `gap` meters centered at z = `gap_z`.
    fn wall(half_len: f32, gap: Option<(f32, f32)>) -> HeightCells {
        terrain(move |x, z| {
            let in_wall = x.abs() <= 7.5 && z.abs() <= half_len;
            let in_gap = gap.is_some_and(|(gz, w)| (z - gz).abs() <= w * 0.5);
            if in_wall && !in_gap { 60.0 } else { 0.0 }
        })
    }

    #[test]
    fn settings_default_to_the_source_values() {
        let s = CrowdSettings::default();
        assert_eq!(s.max_groups, CROWD_MAX_GROUPS);
        assert_eq!(s.max_members, CROWD_MAX_MEMBERS);
        assert_eq!(s.lane_count, LANE_COUNT);
        assert_eq!(s.lane_width_m, LANE_WIDTH_M);
        assert_eq!(s.slot_gap_m, SLOT_GAP_M);
        assert_eq!(s.choke_radius_m, CHOKE_RADIUS_M);
        assert_eq!(s.flow_idle_ticks, FLOW_IDLE_TICKS);
    }

    #[test]
    fn crowd_slots_face_the_march_and_hold_spacing() {
        let mut crowd = Crowd::default();
        let members = [7u32, 8, 9, 10];
        let lane = crowd.refresh(11, &members, [0.0, 0.0], [-100.0, 0.0], false, 0);
        assert!(!lane, "open ground files nobody");
        assert!(crowd.contains(11));
        assert!(!crowd.lane_mode(11));
        let slots: Vec<[f32; 2]> = members
            .iter()
            .map(|id| crowd.slot_offset(11, *id).expect("a slot"))
            .collect();
        // The block centres on the anchor.
        let (mx, mz) = (
            slots.iter().map(|s| s[0]).sum::<f32>() / 4.0,
            slots.iter().map(|s| s[1]).sum::<f32>() / 4.0,
        );
        assert!(
            mx.abs() < 0.05 && mz.abs() < 0.05,
            "the block centres on the anchor: {slots:?}"
        );
        // Rank order trails the eastward march.
        assert!(slots[0][0] > slots[3][0], "ranks trail the march: {slots:?}");
        // Spacing holds in the rotated frame.
        for (i, a) in slots.iter().enumerate() {
            for b in &slots[i + 1..] {
                let dx = a[0] - b[0];
                let dz = a[1] - b[1];
                let d = math::sqrt(dx * dx + dz * dz);
                assert!(d >= SLOT_GAP_M - 0.1, "slots {a:?} and {b:?} overlap ({d})");
            }
        }
        // The same inputs replay exactly.
        let mut again = Crowd::default();
        again.refresh(11, &members, [0.0, 0.0], [-100.0, 0.0], false, 0);
        for (id, s) in members.iter().zip(&slots) {
            assert_eq!(
                again.slot_offset(11, *id).expect("a slot"),
                *s,
                "the flow replays exactly"
            );
        }
    }

    #[test]
    fn crowd_lanes_file_through_a_registered_choke() {
        let mut crowd = Crowd::default();
        crowd.register_choke([0.0, 40.0]);
        crowd.register_choke([0.0, 40.0]);
        assert_eq!(crowd.choke_count(), 1, "a second report is the same choke");
        assert!(
            !crowd.choke_near(None, [-100.0, -40.0], [100.0, -40.0]),
            "off the leg, no lanes"
        );
        assert!(
            crowd.choke_near(None, [-100.0, 40.0], [100.0, 40.0]),
            "the choke sits on the leg"
        );
        let members = [21u32, 22, 23, 24];
        let lane = crowd.refresh(12, &members, [100.0, 40.0], [-100.0, 40.0], true, 0);
        assert!(lane && crowd.lane_mode(12));
        // Files across the eastward march, ranks trailing behind the anchor.
        let slots: Vec<[f32; 2]> = members
            .iter()
            .map(|id| crowd.slot_offset(12, *id).expect("a slot"))
            .collect();
        for (rank, s) in slots.iter().enumerate() {
            let along = s[0] - 100.0;
            let lateral = -(s[1] - 40.0);
            let file = (rank % LANE_COUNT) as f32 - 1.0;
            assert!(
                (lateral - file * LANE_WIDTH_M).abs() < 0.1,
                "rank {rank} files: {s:?}"
            );
            assert!(along <= 0.05, "rank {rank} trails the anchor: {s:?}");
        }
        assert!(
            (slots[3][0] - (100.0 - SLOT_GAP_M)).abs() < 0.1,
            "the fourth member takes the second rank: {:?}",
            slots[3]
        );
        // Past the choke the files open back up.
        let lane = crowd.refresh(12, &members, [100.0, 40.0], [-100.0, 40.0], false, 1);
        assert!(!lane && !crowd.lane_mode(12));
    }

    #[test]
    fn crowd_flows_evaporate_when_idle() {
        let mut crowd = Crowd::default();
        crowd.refresh(13, &[31, 32], [10.0, 0.0], [-50.0, 0.0], false, 100);
        assert!(crowd.contains(13));
        crowd.service(100 + FLOW_IDLE_TICKS);
        assert!(crowd.contains(13), "still held at the idle edge");
        crowd.service(100 + FLOW_IDLE_TICKS + 1);
        assert!(!crowd.contains(13), "an idle flow evaporates");
        crowd.refresh(14, &[31, 32, 33], [10.0, 0.0], [-50.0, 0.0], false, 200);
        crowd.leave(14, 31);
        assert!(crowd.contains(14), "two members still hold a flow");
        crowd.leave(14, 32);
        assert!(!crowd.contains(14), "one member is no flow");
        // Singles and strangers hold no flow.
        assert!(!crowd.refresh(15, &[40], [0.0, 0.0], [0.0, 0.0], false, 200));
        assert!(crowd.slot_offset(15, 40).is_none());
        assert!(crowd.slot_offset(99, 40).is_none());
    }

    #[test]
    fn crowd_bent_slots_pull_back_from_a_cliff() {
        let mut agents = DetAgents::default();
        agents.set_ground(Some(Arc::new(wall(150.0, None))));
        agents.prepare(&Empty);
        let grid = agents.grid().expect("the wall grid");
        let member = [-100.0, 0.0];
        let (same, bent) = Crowd::bend_slot(grid, Profile::HOVER, member, [-50.0, 0.0]);
        assert!(!bent && same == [-50.0, 0.0], "an open leg keeps its slot");
        let (back, bent) = Crowd::bend_slot(grid, Profile::HOVER, member, [100.0, 0.0]);
        assert!(bent, "the far slot sits past the cliff");
        assert!(back[0] < -7.5, "the bent slot stays west of the wall: {back:?}");
        let from = grid.cell_at(member[0], member[1]);
        assert!(
            grid.los(
                Profile::HOVER,
                from,
                grid.cell_at(back[0], back[1]),
                COST_STEEP,
                1
            ),
            "the bent leg has line of travel"
        );
    }

    #[test]
    fn a_crowd_group_marches_to_its_point_without_wedging() {
        let mut state = march_world(Some(wall(300.0, Some((0.0, 40.0)))), [120.0, 0.0]);
        let ids: Vec<u32> = (0..6).map(|k| 700 + k).collect();
        for (k, id) in ids.iter().enumerate() {
            put(
                &mut state,
                *id,
                [-120.0 - (k / 3) as f32 * 12.0, 0.0, -24.0 + (k % 3) as f32 * 24.0],
            );
        }
        assert_eq!(state.order(&ids), 6);
        step(&mut state);
        let group = state.group;
        assert!(
            state.agents.crowd.contains(group),
            "the order marches as one flow"
        );
        assert_eq!(state.agents.stats.crowd_flows, 1, "one order, one flow");
        let mut arrived_tick = None;
        for t in 0..60 * 60 {
            step(&mut state);
            let parked = ids.iter().all(|id| {
                let Some(raw) = state.agents.crowd_slot(group, *id) else {
                    return false;
                };
                // The source's `group_slot` returns this bent slot, then parks on it.
                let at = pos(&state, *id);
                let (slot, _) = state.agents.crowd_bent_slot(Profile::HOVER, [at[0], at[2]], raw);
                d2(at, [slot[0], 0.0, slot[1]]) <= GROUP_SLOT_PARK_M + 1.0
            });
            if parked {
                arrived_tick = Some(t);
                break;
            }
        }
        assert!(
            arrived_tick.is_some(),
            "the crowd did not arrive: {:?} {:?}",
            state.agents.stats,
            ids.iter()
                .map(|id| {
                    let at = pos(&state, *id);
                    let raw = state.agents.crowd_slot(group, *id);
                    (*id, at, raw)
                })
                .collect::<Vec<_>>()
        );
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                let d = d2(pos(&state, *a), pos(&state, *b));
                assert!(d >= 2.0 * MOVER_RADIUS_M - 0.5, "{a} and {b} overlap ({d} m)");
            }
        }
        assert!(
            state.agents.stats.stuck_events <= 3,
            "stuck events {}",
            state.agents.stats.stuck_events
        );
    }

    /// 600 ticks, 16 members, one choke: two runs land on the same position hash.
    #[test]
    fn crowd_hash_is_stable_over_two_runs() {
        let once = hash_run();
        assert_eq!(hash_run(), once, "two runs diverged: {once:#x}");
        assert_eq!(once, PINNED_CROWD_HASH, "the crowd run moved: {once:#x}");
    }

    /// Position hash of the 600-tick, 16-member, one-choke run on the test machine
    /// (x86_64, test profile). Not a time budget. Re-pinned for ADR-8876 (`MATH_BITS_VERSION` 1, the
    /// canonical correctly rounded `regia_detsim::math::atan2`; was `0x897c_5e07_5bcf_f219`).
    const PINNED_CROWD_HASH: u64 = 0x1703_a172_e1ac_f294;

    fn hash_run() -> u64 {
        let mut state = march_world(None, [80.0, 0.0]);
        state.agents.register_choke([0.0, 0.0]);
        let ids: Vec<u32> = (0..16).map(|k| 800 + k).collect();
        for (k, id) in ids.iter().enumerate() {
            let row = (k / 4) as f32;
            let col = (k % 4) as f32;
            put(&mut state, *id, [-80.0 - row * 12.0, 0.0, -18.0 + col * 12.0]);
        }
        assert_eq!(state.order(&ids), 16);
        assert_eq!(state.agents.crowd.choke_count(), 1);
        for _ in 0..600 {
            step(&mut state);
        }
        assert!(
            state.agents.crowd.lane_mode(state.group),
            "the choke files the group"
        );
        position_hash(&state)
    }

    struct Empty;

    impl DetAgentWorld for Empty {
        fn ids(&self) -> Vec<u32> {
            Vec::new()
        }
        fn body(&self, _id: u32) -> Option<DetBody> {
            None
        }
        fn obstacle(&self, _id: u32) -> Option<Stamp> {
            None
        }
    }

    struct Mover {
        pos: [f32; 3],
        yaw: f32,
    }

    struct Bodies {
        map: BTreeMap<u32, Mover>,
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
                radius: MOVER_RADIUS_M,
                profile: Some(Profile::HOVER),
                team: 1,
                vel: [0.0, 0.0],
                yaw: b.yaw,
                cruise_m_s: 30.0,
                soft: false,
            })
        }
        fn obstacle(&self, _id: u32) -> Option<Stamp> {
            None
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

    struct March {
        bodies: Bodies,
        agents: DetAgents,
        ids: Vec<u32>,
        group: u32,
        anchor: [f32; 2],
        tick: u64,
        ordered: bool,
    }

    fn march_world(ground: Option<HeightCells>, anchor: [f32; 2]) -> March {
        let mut agents = DetAgents::default();
        let ground = ground.map(Arc::new);
        agents.set_ground(ground.clone().map(|g| g as Arc<dyn GridGround + Send + Sync>));
        March {
            bodies: Bodies {
                map: BTreeMap::new(),
                ground,
            },
            agents,
            ids: Vec::new(),
            group: 1,
            anchor,
            tick: 0,
            ordered: false,
        }
    }

    fn put(world: &mut March, id: u32, at: [f32; 3]) {
        world.bodies.map.insert(id, Mover { pos: at, yaw: 0.0 });
    }

    impl March {
        /// One group order. Returns how many living members took it.
        fn order(&mut self, ids: &[u32]) -> usize {
            self.ids = ids
                .iter()
                .copied()
                .filter(|id| self.bodies.map.contains_key(id))
                .collect();
            self.ordered = self.ids.len() >= 2;
            self.ids.len()
        }
    }

    fn step(world: &mut March) {
        world.tick += 1;
        let tick = world.tick;
        world.agents.service(tick, &world.bodies);
        if world.ordered {
            let members: Vec<(u32, [f32; 2])> = world
                .ids
                .iter()
                .filter_map(|id| {
                    let b = world.bodies.map.get(id)?;
                    Some((*id, [b.pos[0], b.pos[2]]))
                })
                .collect();
            world
                .agents
                .crowd_refresh(world.group, &members, world.anchor, tick);
        }
        let goals: Vec<(u32, [f32; 3])> = world
            .ids
            .iter()
            .filter_map(|id| {
                let slot = world.agents.crowd_slot(world.group, *id)?;
                let b = world.bodies.map.get(id)?;
                let (bent, _) = world
                    .agents
                    .crowd_bent_slot(Profile::HOVER, [b.pos[0], b.pos[2]], slot);
                Some((*id, [bent[0], b.pos[1], bent[1]]))
            })
            .collect();
        {
            let mut mover = KinematicMover::new(&mut world.agents, &mut world.bodies, tick);
            for (id, goal) in &goals {
                mover.move_kinematic(*id, *goal, GROUP_SLOT_PARK_M);
            }
        }
        separate(&mut world.bodies);
        world.agents.commit_escapes(tick, &mut world.bodies);
    }

    fn separate(bodies: &mut Bodies) {
        let ids: Vec<u32> = bodies.map.keys().copied().collect();
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                let (pa, pb) = (bodies.map[a].pos, bodies.map[b].pos);
                let dx = pb[0] - pa[0];
                let dz = pb[2] - pa[2];
                let d = math::sqrt(dx * dx + dz * dz);
                let need = MOVER_RADIUS_M * 2.0;
                if d >= need {
                    continue;
                }
                let (nx, nz) = if d > 1e-4 { (dx / d, dz / d) } else { (1.0, 0.0) };
                let over = need - d;
                if let Some(x) = bodies.map.get_mut(a) {
                    x.pos[0] -= nx * over * 0.5;
                    x.pos[2] -= nz * over * 0.5;
                }
                if let Some(x) = bodies.map.get_mut(b) {
                    x.pos[0] += nx * over * 0.5;
                    x.pos[2] += nz * over * 0.5;
                }
            }
        }
    }

    fn pos(world: &March, id: u32) -> [f32; 3] {
        world.bodies.map[&id].pos
    }

    fn d2(a: [f32; 3], b: [f32; 3]) -> f32 {
        let dx = a[0] - b[0];
        let dz = a[2] - b[2];
        math::sqrt(dx * dx + dz * dz)
    }

    fn position_hash(world: &March) -> u64 {
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
}
