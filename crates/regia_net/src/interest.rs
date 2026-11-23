// SPDX-License-Identifier: MPL-2.0
//! The interest core shared by the ECS replication path (`crate::relevance`, `crate::replication`)
//! and the ECS-free [`crate::external_replication`] (spec 120 § 8.4, § 8.6): the incremental
//! uniform grid with one band per relevance radius, the relevance shape with its hysteresis rule,
//! and the distance send-rate tiers. Core: builds without the `ecs` feature.
//!
//! Moved here from `relevance.rs` and `profile.rs` (ADR-8186) so both paths run the same code; the
//! old paths (`relevance::InterestGrid`, `profile::{RelevanceShape, RateTiers, RateTier}`) are
//! re-exports.

use std::collections::BTreeMap;

use glam::Vec3;
use serde::{Deserialize, Serialize};

use crate::fast_map::FastMap;

/// Where an entity is relevant (spec 72 § 5.1 stage 5).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum RelevanceShape {
    /// Relevant within `radius` metres of a viewpoint; once relevant it stays relevant until it is
    /// farther than `radius × (1 + hysteresis)`.
    Spatial {
        /// Radius in metres (> 0).
        radius: f32,
        /// Extra fraction of the radius before the entity stops being relevant (0..=1).
        hysteresis: f32,
    },
    /// Passes the spatial stage everywhere.
    World,
}

/// Distance send-rate tiers of a profile (spec 120 § 8.3): how often an entity's updates go to a
/// connection by its distance to the nearest viewpoint. Near entities go on every send tick, mid
/// ones every `mid_every`-th send, far ones at `far_hz`; beyond the relevance radius they are not
/// relevant at all (hidden). Spawns, owned entities and forced records are never held back.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RateTiers {
    /// Below this distance (metres) every send carries the entity.
    #[serde(default = "d_near_m")]
    pub near_m: f32,
    /// Below this distance (metres, >= `near_m`) every `mid_every`-th send carries it.
    #[serde(default = "d_mid_m")]
    pub mid_m: f32,
    /// The mid tier's send divisor (>= 1).
    #[serde(default = "d_mid_every")]
    pub mid_every: u32,
    /// Updates per second beyond `mid_m` (> 0).
    #[serde(default = "d_far_hz")]
    pub far_hz: f32,
}

fn d_near_m() -> f32 {
    150.0
}
fn d_mid_m() -> f32 {
    600.0
}
fn d_mid_every() -> u32 {
    3
}
fn d_far_hz() -> f32 {
    2.0
}

impl Default for RateTiers {
    fn default() -> Self {
        Self {
            near_m: d_near_m(),
            mid_m: d_mid_m(),
            mid_every: d_mid_every(),
            far_hz: d_far_hz(),
        }
    }
}

/// One distance tier of [`RateTiers`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateTier {
    /// Every send.
    Near,
    /// Every `mid_every`-th send.
    Mid,
    /// At `far_hz`.
    Far,
}

impl RateTier {
    /// The name used in agent replies.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Near => "near",
            Self::Mid => "mid",
            Self::Far => "far",
        }
    }
}

impl RateTiers {
    /// The tier of an entity `distance` metres from the nearest viewpoint.
    pub fn tier(&self, distance: f32) -> RateTier {
        if distance < self.near_m {
            RateTier::Near
        } else if distance < self.mid_m {
            RateTier::Mid
        } else {
            RateTier::Far
        }
    }

    /// The fewest ticks between two updates of an entity in `tier`, for a server that sends every
    /// `send_every_ticks` ticks at `tick_hz`. Near is one send interval; far rounds up to whole
    /// send intervals so the rate never exceeds `far_hz`.
    pub fn interval_ticks(&self, tier: RateTier, send_every_ticks: u32, tick_hz: f64) -> u32 {
        let every = send_every_ticks.max(1);
        match tier {
            RateTier::Near => every,
            RateTier::Mid => every.saturating_mul(self.mid_every.max(1)),
            RateTier::Far => {
                let hz = f64::from(self.far_hz);
                if !(hz > 0.0 && hz.is_finite()) {
                    return every;
                }
                let ticks = (tick_hz / hz).ceil().max(1.0) as u32;
                ticks.div_ceil(every).max(1).saturating_mul(every)
            }
        }
    }

    /// Checks the tiers; on failure the offending field (`rate_tiers.<name>`) and why.
    pub(crate) fn check(&self) -> Result<(), (&'static str, String)> {
        let bad = |field: &'static str, message: String| Err((field, message));
        if !(self.near_m >= 0.0 && self.near_m.is_finite()) {
            return bad("rate_tiers.near_m", format!("must be >= 0, is {}", self.near_m));
        }
        if !(self.mid_m >= self.near_m && self.mid_m.is_finite()) {
            return bad(
                "rate_tiers.mid_m",
                format!("must be >= near_m ({}), is {}", self.near_m, self.mid_m),
            );
        }
        if self.mid_every < 1 {
            return bad("rate_tiers.mid_every", "must be >= 1".into());
        }
        if !(self.far_hz > 0.0 && self.far_hz.is_finite()) {
            return bad("rate_tiers.far_hz", format!("must be > 0, is {}", self.far_hz));
        }
        Ok(())
    }
}

/// The spatial relevance limit of a [`RelevanceShape::Spatial`] entity (spec 72 § 5.1): `radius`
/// to become relevant, `radius × (1 + hysteresis)` to stay relevant once it is (`was_in`).
#[inline]
pub fn spatial_limit(radius: f32, hysteresis: f32, was_in: bool) -> f32 {
    if was_in {
        radius * (1.0 + hysteresis)
    } else {
        radius
    }
}

/// The shared uniform interest grid over the XZ plane (spec 72 § 5.3), updated incrementally.
///
/// Entities sit in one *band* per relevance radius (spec 120 § 8.4): profiles with the same
/// spatial radius share a band, and a viewpoint queries each band with that band's own radius.
/// One long-range profile therefore costs a query over its own few entities, not a query of every
/// entity out to its range.
#[derive(Debug, Clone, Default)]
pub struct InterestGrid {
    /// Cell size in metres.
    pub cell_m: f32,
    bands: Vec<GridBand>,
    /// Index of a slot in its cell list. `None` means the slot is not indexed.
    slot_at: Vec<Option<u32>>,
    /// Equality checks `remove` performed. A hinted removal checks the hinted slot once.
    remove_eqs: u64,
}

/// The entities of one relevance radius.
#[derive(Debug, Clone, Default)]
struct GridBand {
    /// The query radius of this band, in metres (the profiles' `radius`, without hysteresis: a
    /// held entity is evaluated through the held list, so the query only finds newcomers).
    radius: f32,
    cells: FastMap<(i32, i32), Vec<u32>>,
}

/// Where a slot sits in the grid: `(band, cell)`.
pub(crate) type GridAt = (u32, (i32, i32));

impl InterestGrid {
    pub(crate) fn cell_of(&self, p: Vec3) -> (i32, i32) {
        let c = self.cell_m.max(1e-3);
        ((p.x / c).floor() as i32, (p.z / c).floor() as i32)
    }

    /// The band of a query radius, created on first use. Bands are never removed: an unused band
    /// has no cells and costs a skipped loop iteration.
    pub(crate) fn band_of(&mut self, radius: f32) -> u32 {
        let bits = radius.to_bits();
        if let Some(at) = self.bands.iter().position(|b| b.radius.to_bits() == bits) {
            return at as u32;
        }
        self.bands.push(GridBand {
            radius,
            cells: FastMap::default(),
        });
        (self.bands.len() - 1) as u32
    }

    /// Drops every indexed slot and takes a new cell size; the bands keep their radii.
    pub(crate) fn reset(&mut self, cell_m: f32) {
        self.cell_m = cell_m;
        for b in &mut self.bands {
            b.cells.clear();
        }
        self.slot_at.clear();
    }

    fn set_slot_index(&mut self, slot: u32, at: u32) {
        let i = slot as usize;
        if self.slot_at.len() <= i {
            self.slot_at.resize(i + 1, None);
        }
        self.slot_at[i] = Some(at);
    }

    /// Band 0's `insert` (the unit tests' single-band grid).
    #[cfg(test)]
    fn insert(&mut self, cell: (i32, i32), slot: u32) {
        if self.bands.is_empty() {
            self.band_of(0.0);
        }
        self.insert_at((0, cell), slot);
    }

    /// Band 0's `remove`.
    #[cfg(test)]
    fn remove(&mut self, cell: (i32, i32), slot: u32) {
        self.remove_at((0, cell), slot);
    }

    pub(crate) fn insert_at(&mut self, (band, cell): GridAt, slot: u32) {
        let Some(b) = self.bands.get_mut(band as usize) else {
            return;
        };
        let at = {
            let list = b.cells.entry(cell).or_default();
            let at = list.len() as u32;
            list.push(slot);
            at
        };
        self.set_slot_index(slot, at);
    }

    pub(crate) fn remove_at(&mut self, (band, cell): GridAt, slot: u32) {
        let hinted = self.slot_at.get(slot as usize).copied().flatten();
        let mut eqs = 0u64;
        let Some(b) = self.bands.get_mut(band as usize) else {
            return;
        };
        let outcome = {
            let Some(list) = b.cells.get_mut(&cell) else {
                return;
            };
            let at = if let Some(hint) = hinted
                && list.get(hint as usize) == Some(&slot)
            {
                eqs += 1;
                Some(hint)
            } else {
                let mut found = None;
                for (i, stored) in list.iter().enumerate() {
                    eqs += 1;
                    if *stored == slot {
                        found = Some(i as u32);
                        break;
                    }
                }
                found
            };
            if let Some(at) = at {
                list.swap_remove(at as usize);
                let swapped = list.get(at as usize).copied();
                let empty = list.is_empty();
                Some((at, swapped, empty))
            } else {
                None
            }
        };
        if outcome.is_some_and(|o| o.2) {
            b.cells.remove(&cell);
        }
        self.remove_eqs += eqs;
        let Some(outcome) = outcome else {
            return;
        };
        if let Some(swapped) = outcome.1 {
            self.set_slot_index(swapped, outcome.0);
        }
        if (slot as usize) < self.slot_at.len() {
            self.slot_at[slot as usize] = None;
        }
    }

    /// Marks the slots in the cells overlapping a circle, over every band (the unit tests).
    #[cfg(test)]
    fn query(&self, center: Vec3, radius: f32, mut mark: impl FnMut(u32)) {
        for b in &self.bands {
            query_cells(&b.cells, self.cell_m, center, radius, &mut mark);
        }
    }

    /// Marks, in each band, the slots of the cells overlapping that band's circle around `center`.
    /// Returns how many slots it marked (repeats included).
    pub(crate) fn query_bands(&self, center: Vec3, mut mark: impl FnMut(u32)) -> u64 {
        let mut marked = 0;
        for b in &self.bands {
            if !b.cells.is_empty() {
                marked += query_cells(&b.cells, self.cell_m, center, b.radius, &mut mark);
            }
        }
        marked
    }

    /// The bands' query radii in metres, in band order (agent, tests).
    pub fn band_radii(&self) -> impl Iterator<Item = f32> + '_ {
        self.bands.iter().map(|b| b.radius)
    }

    /// Occupied cells: `(cell, entity count)` over every band (debug draw, agent).
    pub fn occupied(&self) -> impl Iterator<Item = ((i32, i32), usize)> + '_ {
        let single = self.bands.iter().filter(|b| !b.cells.is_empty()).count() <= 1;
        let merged: Vec<((i32, i32), usize)> = if single {
            Vec::new()
        } else {
            let mut m: BTreeMap<(i32, i32), usize> = BTreeMap::new();
            for b in &self.bands {
                for (k, v) in &b.cells {
                    *m.entry(*k).or_insert(0) += v.len();
                }
            }
            m.into_iter().collect()
        };
        let direct = self
            .bands
            .iter()
            .filter(move |_| single)
            .flat_map(|b| b.cells.iter().map(|(k, v)| (*k, v.len())));
        direct.chain(merged)
    }
}

/// Marks the slots in the cells of one band overlapping a circle; returns how many it marked.
fn query_cells(
    cells: &FastMap<(i32, i32), Vec<u32>>,
    cell_m: f32,
    center: Vec3,
    radius: f32,
    mark: &mut impl FnMut(u32),
) -> u64 {
    let c = cell_m.max(1e-3);
    let mut marked = 0u64;
    let (x0, z0) = (
        ((center.x - radius) / c).floor() as i32,
        ((center.z - radius) / c).floor() as i32,
    );
    let (x1, z1) = (
        ((center.x + radius) / c).floor() as i32,
        ((center.z + radius) / c).floor() as i32,
    );
    // A cell's nearest point is at most this far from the circle's center when the circle
    // reaches it. The margin keeps a boundary cell whatever the rounding at these coordinates.
    let scale = radius.max(center.x.abs()).max(center.z.abs()).max(c);
    let reach = (radius + scale * 1e-5).powi(2);
    let overlaps = |x: i32, z: i32| {
        let (left, near) = (x as f32 * c, z as f32 * c);
        let dx = (left - center.x).max(center.x - (left + c)).max(0.0);
        let dz = (near - center.z).max(center.z - (near + c)).max(0.0);
        dx * dx + dz * dz <= reach
    };
    // Few viewpoints visit few cells; when the square of cells exceeds the occupied cell
    // count, walk the occupied cells instead.
    // `i128`: a radius near `f32::MAX` spans 2^32 cells per axis, whose product wraps `i64`.
    let span = (i128::from(x1) - i128::from(x0) + 1) * (i128::from(z1) - i128::from(z0) + 1);
    if span > cells.len() as i128 {
        for ((x, z), list) in cells {
            if (x0..=x1).contains(x) && (z0..=z1).contains(z) && overlaps(*x, *z) {
                for s in list {
                    mark(*s);
                }
                marked += list.len() as u64;
            }
        }
        return marked;
    }
    for x in x0..=x1 {
        for z in z0..=z1 {
            if overlaps(x, z)
                && let Some(list) = cells.get(&(x, z))
            {
                for s in list {
                    mark(*s);
                }
                marked += list.len() as u64;
            }
        }
    }
    marked
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A set of marked slots (the grid tests' stand-in for the send loop's bit set).
    #[derive(Default)]
    struct Marks(Vec<bool>);

    impl Marks {
        fn reset(&mut self, n: usize) {
            self.0.clear();
            self.0.resize(n, false);
        }
        fn set(&mut self, slot: u32) {
            if let Some(b) = self.0.get_mut(slot as usize) {
                *b = true;
            }
        }
        fn get(&self, slot: u32) -> bool {
            self.0.get(slot as usize).copied().unwrap_or(false)
        }
    }

    /// The hysteresis rule: enter at the radius, leave beyond the widened radius.
    #[test]
    fn the_spatial_limit_widens_only_for_a_held_entity() {
        assert_eq!(spatial_limit(100.0, 0.25, false), 100.0);
        assert_eq!(spatial_limit(100.0, 0.25, true), 125.0);
    }

    /// Every entity within the radius is marked, on both sides of the origin and on cell edges.
    #[test]
    fn a_grid_query_marks_everything_in_the_circle() {
        let mut grid = InterestGrid {
            cell_m: 16.0,
            ..InterestGrid::default()
        };
        let mut next = 7u64;
        let mut coord = move || {
            next = next.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            ((next >> 40) % 8192) as f32 / 8.0 - 512.0
        };
        let mut points = Vec::new();
        for slot in 0..3000u32 {
            let p = Vec3::new(coord(), 0.0, coord());
            grid.insert(grid.cell_of(p), slot);
            points.push(p);
        }
        for (center, radius) in [
            (Vec3::ZERO, 0.0),
            (Vec3::new(16.0, 0.0, -16.0), 16.0),
            (Vec3::new(-33.5, 0.0, 120.25), 47.0),
            (Vec3::new(400.0, 0.0, -400.0), 900.0),
            (Vec3::new(-512.0, 0.0, -512.0), 64.0),
            (Vec3::new(-400.25, 0.0, 16.0), 110.5),
        ] {
            let mut marked = Marks::default();
            marked.reset(points.len());
            grid.query(center, radius, |s| marked.set(s));
            for (slot, p) in points.iter().enumerate() {
                let inside = p.distance(center) <= radius;
                assert!(
                    !inside || marked.get(slot as u32),
                    "{p} from {center} within {radius}"
                );
            }
        }
    }

    /// A corner cell of the square around the circle holds nothing the circle reaches.
    #[test]
    fn a_grid_query_skips_the_corners_of_its_square() {
        let mut grid = InterestGrid {
            cell_m: 10.0,
            ..InterestGrid::default()
        };
        grid.insert((10, 10), 1);
        grid.insert((9, 0), 2);
        for x in -20..20 {
            grid.insert((x, 30), 3);
        }
        let mut marked = Marks::default();
        marked.reset(8);
        grid.query(Vec3::ZERO, 100.0, |s| marked.set(s));
        assert!(!marked.get(1), "the corner at (100, 100) is outside");
        assert!(marked.get(2));
    }

    /// A radius spanning the whole `i32` cell range walks the occupied cells instead of looping
    /// over 2^64 cell coordinates (the span product used to wrap `i64`).
    #[test]
    fn a_grid_query_with_an_enormous_radius_terminates() {
        let mut grid = InterestGrid {
            cell_m: 10.0,
            ..InterestGrid::default()
        };
        grid.insert((3, -4), 1);
        grid.insert((-900, 20), 2);
        let mut marked = Marks::default();
        marked.reset(4);
        grid.query(Vec3::ZERO, f32::MAX, |s| marked.set(s));
        assert!(marked.get(1) && marked.get(2));
    }

    /// Every slot of one cell leaves with one equality check, and none of them stay queryable.
    #[test]
    fn removing_a_cell_checks_each_slot_once() {
        let mut grid = InterestGrid {
            cell_m: 8.0,
            ..InterestGrid::default()
        };
        let n = 64u32;
        for slot in 0..n {
            grid.insert((1, 2), slot);
        }
        let before = grid.remove_eqs;
        for slot in 0..n {
            grid.remove((1, 2), slot);
        }
        let checks = grid.remove_eqs - before;
        assert_eq!(
            checks,
            u64::from(n),
            "equality checks to remove {n} slots: {checks}"
        );
        let mut marked = Marks::default();
        marked.reset(n as usize);
        grid.query(Vec3::new(12.0, 0.0, 20.0), 1.0, |s| marked.set(s));
        for slot in 0..n {
            assert!(!marked.get(slot), "slot {slot} stayed in the cell");
        }
    }
}
