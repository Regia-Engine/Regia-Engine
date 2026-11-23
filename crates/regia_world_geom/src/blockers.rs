// SPDX-License-Identifier: MPL-2.0
//! Section blocker plugs at run time (spec 121 § 5.3 rule 6 and § 5.7; plan row W-13, T7 collapse).
//!
//! The cook (W-08) writes one plug per collapsible section: the passage cross-section over
//! `plug_length_m`, cooked as canonical `Blocker` triangles after every passage triangle of the
//! layer, and marks the section and every portal on its boundary with the plug's id. At run time a
//! plug is one bit of [`GeomState`]:
//!
//! - [`GeomState::set_blocker`] flips the bit, XORs the blocker's one hash term (§ 5.2 rule 2) and
//!   bumps the generation once. Nothing else is written: every reader reads the same bit, so the
//!   query candidates, the nav cells and the sound occlusion change together, at that generation.
//! - [`GeomState::apply_blocker_flips`] applies one tick's flips in a fixed order (ascending
//!   section, then id, then `false` before `true`), whatever order the caller listed them in, so
//!   two peers fed the same flips reach the same state and the same `state_hash`.
//! - **Queries** (§ 5.3 rule 6): while a plug is active its triangles are candidates: its up-facing
//!   top is a floor of `support`, `floors_at` and `surface_at`, every face is a `segment` and
//!   `rock_between` solid, and its end faces are walls of `push_disc`. While it is clear they are
//!   skipped by one bit test, and with no plug active at all the queries skip the test entirely,
//!   so the answers of a state with every plug clear are the answers of a map with no plugs.
//! - **Nav and portals**: [`WorldGeom::section_open`], [`WorldGeom::portal_open`],
//!   [`WorldGeom::closed_sections`] (DO27's closed set: the whole list, ascending) and
//!   [`WorldGeom::cell_route`], a route over the section graph a layered nav reads (W-20 builds
//!   its polys on it).
//!
//! Health, explosive-only damage and "stays until cleared" are sim rules and stay in the host's
//! sim. There is no digging: in D2 the plug becomes a voxel carve behind the same call.

use crate::WorldGeom;
use crate::edit::{EditStats, Generation, GeomEditError, GeomState};
use crate::query::SectionId;

/// A cooked blocker plug's id: dense, equal to its index in [`WorldGeom::blockers`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub struct BlockerId(pub u16);

/// A cooked portal's id: dense, equal to its index in [`WorldGeom::portals`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub struct PortalId(pub u16);

impl GeomState {
    /// Activates or clears blocker plug `id` (spec 121 § 5.7). The query candidates, the nav
    /// cells and the sound occlusion all read this one bit, so they change together; the
    /// generation is bumped once. Setting a plug to the state it is already in changes nothing
    /// and returns the current generation. Returns the generation after the call.
    ///
    /// # Errors
    ///
    /// [`GeomEditError::UnknownBlocker`] (`blocker_unknown`) when `geom` has no plug `id`; the
    /// state is unchanged.
    pub fn set_blocker(
        &mut self,
        geom: &WorldGeom,
        id: BlockerId,
        active: bool,
    ) -> Result<Generation, GeomEditError> {
        self.set_blocker_with_stats(geom, id, active, &mut EditStats::default())
    }

    /// [`GeomState::set_blocker`] with a work counter: a flip adds exactly one hash term (E4).
    ///
    /// # Errors
    ///
    /// As [`GeomState::set_blocker`].
    pub fn set_blocker_with_stats(
        &mut self,
        geom: &WorldGeom,
        id: BlockerId,
        active: bool,
        stats: &mut EditStats,
    ) -> Result<Generation, GeomEditError> {
        if usize::from(id.0) >= geom.blockers().len() {
            return Err(GeomEditError::UnknownBlocker(id));
        }
        if self.blocker_bit(id.0) == active {
            return Ok(self.generation());
        }
        Ok(self.toggle_blocker(id.0, stats))
    }

    /// Applies one tick's blocker flips in the fixed order: ascending section of the plug, then
    /// ascending id, then `false` before `true` (so a plug listed twice ends active). The order
    /// the caller lists them in does not matter. Each flip that changes a bit bumps the generation
    /// once. Sorts `flips` in place; allocates nothing.
    ///
    /// # Errors
    ///
    /// [`GeomEditError::UnknownBlocker`] for the first unknown id (in the sorted order); no flip
    /// of the call is applied.
    pub fn apply_blocker_flips(
        &mut self,
        geom: &WorldGeom,
        flips: &mut [(BlockerId, bool)],
    ) -> Result<Generation, GeomEditError> {
        let plugs = geom.blockers();
        let key = |f: &(BlockerId, bool)| (plugs.get(usize::from(f.0.0)).map(|p| p.section), f.0, f.1);
        // Insertion sort: one tick's list is short, and it needs no buffer. Keys are total and
        // equal keys are equal entries, so the result is the same for every input order.
        for i in 1..flips.len() {
            let mut j = i;
            while j > 0 && key(&flips[j - 1]) > key(&flips[j]) {
                flips.swap(j - 1, j);
                j -= 1;
            }
        }
        if let Some(bad) = flips.iter().find(|f| usize::from(f.0.0) >= plugs.len()) {
            return Err(GeomEditError::UnknownBlocker(bad.0));
        }
        for &(id, active) in flips.iter() {
            self.set_blocker(geom, id, active)?;
        }
        Ok(self.generation())
    }

    /// Whether blocker `id` is active (an unknown id reads clear).
    #[must_use]
    pub fn blocker_active(&self, id: BlockerId) -> bool {
        self.blocker_bit(id.0)
    }
}

/// One blocker plug as the agent method `terrain.blockers` reports it (`{ id, section, active }`; spec 121 § 12, W-28).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BlockerStatus {
    /// The plug.
    pub id: BlockerId,
    /// The section it closes.
    pub section: SectionId,
    /// Whether it is active in this state.
    pub active: bool,
}

impl GeomState {
    /// Every cooked blocker plug of `geom` with its state, ascending id: the read-only view the
    /// agent method `terrain.blockers` returns (W-28). Allocates nothing.
    pub fn blockers<'a>(&'a self, geom: &'a WorldGeom) -> impl ExactSizeIterator<Item = BlockerStatus> + 'a {
        geom.blockers().iter().map(|p| BlockerStatus {
            id: BlockerId(p.id),
            section: SectionId(p.section),
            active: self.blocker_bit(p.id),
        })
    }
}

/// One step of a [`WorldGeom::cell_route`]: the cell entered and the portal crossed into it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellStep {
    /// The cell: a section, or `None` for outdoors (cell 0).
    pub cell: Option<SectionId>,
    /// The portal crossed to enter it (`None` for the first step).
    pub portal: Option<PortalId>,
}

impl WorldGeom {
    /// Whether the plug (if any) on the triangle's section is active. `section` is the cooked
    /// triangle's section id.
    #[inline]
    pub(crate) fn plug_on(&self, state: &GeomState, section: Option<u16>) -> bool {
        section
            .and_then(|s| self.sections().get(usize::from(s)))
            .and_then(|s| s.blocker)
            .is_some_and(|b| state.blocker_bit(b))
    }

    /// Whether section `section` is open: it has no plug, or its plug is clear. An unknown section
    /// reads closed.
    #[must_use]
    pub fn section_open(&self, state: &GeomState, section: SectionId) -> bool {
        self.sections()
            .get(usize::from(section.0))
            .is_some_and(|s| s.blocker.is_none_or(|b| !state.blocker_bit(b)))
    }

    /// Whether portal `portal` is open: its own blocker (if any) is clear and both cells it joins
    /// are open (outdoors is always open). VisCell traversal (W-21), portal sound (W-25) and the
    /// layered nav (W-20) read this. An unknown portal reads closed.
    #[must_use]
    pub fn portal_open(&self, state: &GeomState, portal: PortalId) -> bool {
        self.portals().get(usize::from(portal.0)).is_some_and(|p| {
            p.blocker.is_none_or(|b| !state.blocker_bit(b))
                && p.sides
                    .iter()
                    .flatten()
                    .all(|&s| self.section_open(state, SectionId(s)))
        })
    }

    /// The closed sections (those whose plug is active), ascending: the whole list, replaced on
    /// every call (DO27's `closed()` for the collapse rubble). `out` is cleared first.
    pub fn closed_sections(&self, state: &GeomState, out: &mut Vec<SectionId>) {
        out.clear();
        if !state.any_blocker_active() {
            return;
        }
        for plug in self.blockers() {
            if state.blocker_bit(plug.id) {
                out.push(SectionId(plug.section));
            }
        }
        out.sort_unstable();
        out.dedup();
    }

    /// The cheapest route from cell `from` to cell `to` through open portals (`None` is outdoors),
    /// or `None` when no route exists or either end is a closed section. A route is a chain of
    /// portals; a leg between two portals of one cell costs the integer millimetre distance between
    /// their nav nodes (W-08's `nav_mm`), and a leg across outdoors that distance times
    /// `outdoor_cost_pct / 100` (the surface penalty a planner gives the open map). Dijkstra over
    /// (portal, exit side), ties by the lower node, so the route is the same on every host. The
    /// first step is `from` with no portal. Allocates: a nav bake or re-plan helper, not a
    /// per-tick query.
    #[must_use]
    pub fn cell_route(
        &self,
        state: &GeomState,
        from: Option<SectionId>,
        to: Option<SectionId>,
        outdoor_cost_pct: u32,
    ) -> Option<Vec<CellStep>> {
        use std::cmp::Reverse;
        let _span = tracing::trace_span!("world_geom_cell_route").entered();
        let sections = self.sections();
        let portals = self.portals();
        let usable = |c: Option<SectionId>| match c {
            None => true,
            Some(s) => self.section_open(state, s),
        };
        if !usable(from) || !usable(to) {
            return None;
        }
        let first = CellStep {
            cell: from,
            portal: None,
        };
        if from == to {
            return Some(vec![first]);
        }
        // Outdoors touches every portal with an open side; a section touches its own portals.
        let outdoor: Vec<u16> = portals
            .iter()
            .filter(|p| p.sides.contains(&None))
            .map(|p| p.id)
            .collect();
        let ports_of = |c: Option<SectionId>| -> &[u16] {
            match c {
                None => &outdoor,
                Some(s) => &sections[usize::from(s.0)].portals,
            }
        };
        let cell_of = |side: Option<u16>| side.map(SectionId);
        // Node `2 * portal + k`: portal crossed into its side `k`.
        let leg = |p: u16, q: u16, through: Option<SectionId>| -> i64 {
            let (a, b) = (portals[usize::from(p)].nav_mm, portals[usize::from(q)].nav_mm);
            let d2: i64 = (0..3).map(|k| (i64::from(a[k]) - i64::from(b[k])).pow(2)).sum();
            let d = d2.isqrt();
            if through.is_none() {
                d * i64::from(outdoor_cost_pct) / 100
            } else {
                d
            }
        };
        let mut dist = vec![i64::MAX; portals.len() * 2];
        let mut prev: Vec<Option<usize>> = vec![None; portals.len() * 2];
        let mut heap = std::collections::BinaryHeap::new();
        let here = from.map(|s| s.0);
        for &p in ports_of(from) {
            if !self.portal_open(state, PortalId(p)) {
                continue;
            }
            let sides = portals[usize::from(p)].sides;
            let k = usize::from(sides[0] == here);
            let node = usize::from(p) * 2 + k;
            dist[node] = 0;
            heap.push(Reverse((0i64, node)));
        }
        let mut goal = None;
        while let Some(Reverse((d, node))) = heap.pop() {
            if d > dist[node] {
                continue;
            }
            let p = (node / 2) as u16;
            let cell = cell_of(portals[usize::from(p)].sides[node % 2]);
            if cell == to {
                goal = Some(node);
                break;
            }
            for &q in ports_of(cell) {
                if q == p || !self.portal_open(state, PortalId(q)) {
                    continue;
                }
                let sides = portals[usize::from(q)].sides;
                let k = usize::from(cell_of(sides[0]) == cell);
                let next = usize::from(q) * 2 + k;
                let nd = d + leg(p, q, cell);
                if nd < dist[next] {
                    dist[next] = nd;
                    prev[next] = Some(node);
                    heap.push(Reverse((nd, next)));
                }
            }
        }
        let mut at = goal?;
        let mut steps = Vec::new();
        loop {
            let p = at / 2;
            steps.push(CellStep {
                cell: cell_of(portals[p].sides[at % 2]),
                portal: Some(PortalId(p as u16)),
            });
            match prev[at] {
                Some(back) => at = back,
                None => break,
            }
        }
        steps.push(first);
        steps.reverse();
        Some(steps)
    }
}
