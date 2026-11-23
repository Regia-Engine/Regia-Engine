// SPDX-License-Identifier: MPL-2.0
//! The agent-protocol view of a host's deterministic agents (plan row S-03, ADR-8405).
//!
//! [`crate::det::agent::DetAgents`] lives in the host's lockstep sim, not in an engine resource,
//! so the engine cannot read it. A host that wants agents to see it publishes a report into
//! [`DetAgentsView`] once per tick (after its movers); the `nav.det_agents` protocol method reads
//! the last one. The view is read-only and never feeds the sim.
//!
//! [`DetFlowView`] does the same for flow fields (ADR-8839): the host publishes its grid, and the
//! `sim.grid_flow` method builds a field for the goal and profile an agent names and returns a
//! window of it.

use std::sync::{Arc, Mutex, PoisonError};

use regia_app::prelude::*;
use serde::Serialize;

use crate::det::agent::{DetAgents, DetAgentsReport};
use crate::det::flow::{CellRect, ClusterFlow, ClusterStats, FlowField, FlowSettings, FlowStats, FlowView};
use crate::det::grid::{NavGrid, Profile};

/// The last published [`DetAgentsReport`] and the tick it was taken on.
#[derive(Resource, Debug, Clone, Default)]
pub struct DetAgentsView {
    /// The sim tick of the report.
    pub tick: u64,
    /// The report, `None` until the host publishes one.
    pub report: Option<DetAgentsReport>,
}

impl DetAgentsView {
    /// Take a report of `agents` for `tick`.
    pub fn publish(&mut self, tick: u64, agents: &DetAgents) {
        self.tick = tick;
        self.report = Some(agents.report());
    }
}

// ------------------------------------------------------------------------------------- flow fields

/// Grids with more cells than this answer [`DetFlowView::query`] with a clustered flow by default
/// (1024 x 1024 cells; a full field's build time grows with the cell count, 17 ms at 256 x 256 on the
/// min-spec laptop).
pub const FLOW_FULL_MAX_CELLS: usize = 1 << 20;

/// Most cells one [`DetFlowView::query`] window returns (64 x 64).
pub const FLOW_WINDOW_MAX_CELLS: usize = 4096;

/// The grid a host publishes for the `sim.grid_flow` protocol method (ADR-8839).
///
/// [`crate::det::grid::NavGrid`] lives in the host's lockstep sim, so the engine cannot read it. A
/// host that wants agents to inspect flow fields publishes its grid here after each tick's
/// `sync_stamps` ([`Self::publish`] copies it only when its generation or shape moved, or
/// [`Self::publish_shared`] hands over an `Arc`). `sim.grid_flow` then builds the field for the
/// goal and profile the agent names ([`Self::query`]) and keeps the last one, so window after window
/// on the same goal costs no search. The view is read-only and never feeds the sim: the fields it
/// builds are its own, not the host's.
#[derive(Resource, Debug, Default)]
pub struct DetFlowView {
    /// The sim tick of the last publish.
    pub tick: u64,
    /// The published grid, `None` until the host publishes one.
    pub grid: Option<Arc<NavGrid>>,
    /// How fields are built and sampled (match the host's own [`FlowSettings`]).
    pub settings: FlowSettings,
    /// Grids with more cells than this answer with a [`ClusterFlow`] (default
    /// [`FLOW_FULL_MAX_CELLS`]); 0 means the same.
    pub full_max_cells: usize,
    last: Mutex<Option<LastFlow>>,
}

impl Clone for DetFlowView {
    /// The published grid and settings; the clone starts with no cached field.
    fn clone(&self) -> Self {
        DetFlowView {
            tick: self.tick,
            grid: self.grid.clone(),
            settings: self.settings,
            full_max_cells: self.full_max_cells,
            last: Mutex::new(None),
        }
    }
}

/// The field behind a [`FlowQuery`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowQueryKind {
    /// A [`FlowField`] over the whole grid.
    Full,
    /// A [`ClusterFlow`]: values chain through the grid's cluster regions (can sit above the full
    /// field's, see the book page).
    Clustered,
}

/// One answer of [`DetFlowView::query`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FlowQuery {
    /// The sim tick of the published grid.
    pub tick: u64,
    /// The grid's [`NavGrid::generation`] the field was built at.
    pub generation: u64,
    /// Full field or clustered flow.
    pub kind: FlowQueryKind,
    /// The goal cells after dropping blocked ones (sorted, deduplicated).
    pub goals: Vec<u32>,
    /// The capped window.
    pub view: FlowView,
    /// True when the field came from the view's last query (no search ran).
    pub cached: bool,
    /// The full field's build counters (`None` for a clustered flow).
    pub stats: Option<FlowStats>,
    /// The full field's [`FlowField::bits_hash`] (`None` for a clustered flow).
    pub bits_hash: Option<u64>,
    /// The clustered flow's counters (`None` for a full field).
    pub cluster_stats: Option<ClusterStats>,
    /// Bytes the field holds ([`FlowField::memory_bytes`] / [`ClusterFlow::memory_bytes`]).
    pub memory_bytes: usize,
}

/// Why [`DetFlowView::query`] gave no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowQueryError {
    /// No grid is published.
    NoGrid,
    /// The profile index is past the grid's profiles.
    BadProfile,
    /// No goal cell is given, or one is off the grid.
    BadGoal,
    /// Every goal cell is blocked for the profile.
    GoalBlocked,
    /// The window holds no cell after it is cut to the grid.
    EmptyWindow,
}

#[derive(Debug)]
struct LastFlow {
    grid: Arc<NavGrid>,
    generation: u64,
    profile: Profile,
    goals: Vec<u32>,
    settings: FlowSettings,
    clustered: bool,
    /// The full field's `bits_hash`, taken once at build (hashing every cell per window would cost
    /// as much as the window on a large grid).
    bits_hash: Option<u64>,
    field: LastField,
}

#[derive(Debug)]
enum LastField {
    Full(FlowField),
    Clustered(ClusterFlow),
}

impl DetFlowView {
    /// Publish `grid` for `tick`: copied when no grid is published or its generation or size
    /// moved, else only the tick is set. A host that swaps in another grid of the same size and
    /// generation calls [`Self::publish_shared`] (or [`Self::clear`] first).
    pub fn publish(&mut self, tick: u64, grid: &NavGrid) {
        self.tick = tick;
        let same = self.grid.as_ref().is_some_and(|g| {
            g.generation() == grid.generation() && g.width() == grid.width() && g.height() == grid.height()
        });
        if !same {
            self.grid = Some(Arc::new(grid.clone()));
        }
    }

    /// Publish a shared grid for `tick` (no copy).
    pub fn publish_shared(&mut self, tick: u64, grid: Arc<NavGrid>) {
        self.tick = tick;
        self.grid = Some(grid);
    }

    /// Forget the published grid and the cached field.
    pub fn clear(&mut self) {
        self.grid = None;
        *self.last.get_mut().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// The field of `goals` (cell indices) for `profile` on the published grid, and its window
    /// `rect` (cut to the grid, then to `cap` cells, at most [`FLOW_WINDOW_MAX_CELLS`]). A full
    /// [`FlowField`] up to `full_max_cells` grid cells, a [`ClusterFlow`] above. The last
    /// field is kept: the same grid, generation, profile, goals and settings answer from it.
    pub fn query(
        &self,
        profile: Profile,
        goals: &[u32],
        rect: CellRect,
        cap: usize,
    ) -> Result<FlowQuery, FlowQueryError> {
        let _span = tracing::trace_span!("det_flow_view_query").entered();
        let grid = self.grid.as_ref().ok_or(FlowQueryError::NoGrid)?;
        if usize::from(profile.0) >= grid.settings().profiles.len() {
            return Err(FlowQueryError::BadProfile);
        }
        let cells = grid.width() * grid.height();
        if goals.is_empty() || goals.iter().any(|g| *g as usize >= cells) {
            return Err(FlowQueryError::BadGoal);
        }
        let mut open: Vec<u32> = goals
            .iter()
            .copied()
            .filter(|g| grid.passable(profile, *g))
            .collect();
        if open.is_empty() {
            return Err(FlowQueryError::GoalBlocked);
        }
        open.sort_unstable();
        open.dedup();
        let rect = rect.clipped(grid);
        if rect.area() == 0 {
            return Err(FlowQueryError::EmptyWindow);
        }
        let cap = cap.clamp(1, FLOW_WINDOW_MAX_CELLS);
        let limit = if self.full_max_cells == 0 {
            FLOW_FULL_MAX_CELLS
        } else {
            self.full_max_cells
        };
        let clustered = cells > limit;
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let hit = last.as_ref().is_some_and(|l| {
            Arc::ptr_eq(&l.grid, grid)
                && l.generation == grid.generation()
                && l.profile == profile
                && l.goals == open
                && l.settings == self.settings
                && l.clustered == clustered
        });
        if !hit {
            let (field, bits_hash) = if clustered {
                let flow = ClusterFlow::build(grid, profile, &open, 1 << 16);
                (LastField::Clustered(flow), None)
            } else {
                let field = FlowField::build(grid, profile, &open, self.settings);
                let hash = field.bits_hash();
                (LastField::Full(field), Some(hash))
            };
            *last = Some(LastFlow {
                grid: Arc::clone(grid),
                generation: grid.generation(),
                profile,
                goals: open.clone(),
                settings: self.settings,
                clustered,
                bits_hash,
                field,
            });
        }
        let Some(entry) = last.as_mut() else {
            return Err(FlowQueryError::NoGrid);
        };
        let common = |kind, view, memory_bytes| FlowQuery {
            tick: self.tick,
            generation: grid.generation(),
            kind,
            goals: open.clone(),
            view,
            cached: hit,
            stats: None,
            bits_hash: None,
            cluster_stats: None,
            memory_bytes,
        };
        Ok(match &mut entry.field {
            LastField::Full(field) => FlowQuery {
                stats: Some(field.stats()),
                bits_hash: entry.bits_hash,
                ..common(FlowQueryKind::Full, field.view(rect, cap), field.memory_bytes())
            },
            LastField::Clustered(flow) => {
                let view = flow.view(grid, rect, cap);
                FlowQuery {
                    cluster_stats: Some(flow.stats()),
                    ..common(FlowQueryKind::Clustered, view, flow.memory_bytes())
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::det::grid::{GridSettings, NavStatic, Stamp};

    fn grid(w: usize, h: usize) -> NavGrid {
        let settings = Arc::new(GridSettings {
            canvas_cells: [w, h],
            ..GridSettings::hover_ground_jet()
        });
        let mut g = NavGrid::new(Arc::new(NavStatic::build(None, settings).expect("canvas"))).expect("grid");
        g.sync_stamps(vec![(1, Stamp::from_world(0.0, 0.0, 30.0))]);
        g
    }

    /// ADR-8839: the published grid answers a window of the full field, bit for bit what
    /// `FlowField::view` gives; the second query on the same goal answers from the kept field; a
    /// grid above the limit answers with the clustered flow's window; the refusals name their cause;
    /// `publish` copies only when the generation moved.
    #[test]
    fn flow_view_answers_windows_of_the_published_grid() {
        let mut view = DetFlowView::default();
        let rect = CellRect {
            x0: 100,
            z0: 100,
            x1: 140,
            z1: 140,
        };
        assert_eq!(
            view.query(Profile::HOVER, &[0], rect, 64).err(),
            Some(FlowQueryError::NoGrid)
        );
        let mut g = grid(256, 256);
        view.publish(7, &g);
        let first = view.grid.clone().expect("published");
        view.publish(8, &g);
        assert!(
            Arc::ptr_eq(&first, view.grid.as_ref().expect("kept")),
            "same generation: no copy"
        );
        let goal = g.cell_at(200.0, 150.0);
        let q = view
            .query(Profile::HOVER, &[goal, goal], rect, 4096)
            .expect("answer");
        let reference = FlowField::build(&g, Profile::HOVER, &[goal], FlowSettings::default());
        assert_eq!(q.view, reference.view(rect, 4096));
        assert_eq!(q.kind, FlowQueryKind::Full);
        assert_eq!(q.goals, vec![goal]);
        assert_eq!(q.tick, 8);
        assert!(!q.cached);
        assert_eq!(q.bits_hash, Some(reference.bits_hash()));
        let again = view.query(Profile::HOVER, &[goal], rect, 100).expect("answer");
        assert!(again.cached, "same goal: no search");
        assert_eq!(again.bits_hash, q.bits_hash, "the hash taken at build");
        assert!(again.view.truncated && again.view.rect.area() <= 100);
        // A stamp moves the generation: publish copies and the next query searches again.
        // (`sync_stamps` takes the whole set: keep the disc at the origin.)
        g.sync_stamps(vec![
            (1, Stamp::from_world(0.0, 0.0, 30.0)),
            (2, Stamp::from_world(150.0, 150.0, 20.0)),
        ]);
        view.publish(9, &g);
        assert!(!Arc::ptr_eq(&first, view.grid.as_ref().expect("copied")));
        let moved = view.query(Profile::HOVER, &[goal], rect, 4096).expect("answer");
        assert!(!moved.cached);
        assert_eq!(moved.generation, g.generation());
        assert_eq!(
            moved.view,
            FlowField::build(&g, Profile::HOVER, &[goal], FlowSettings::default()).view(rect, 4096)
        );
        // Refusals.
        let blocked = g.cell_at(0.0, 0.0);
        assert_eq!(
            view.query(Profile::HOVER, &[blocked], rect, 64).err(),
            Some(FlowQueryError::GoalBlocked)
        );
        assert_eq!(
            view.query(Profile::HOVER, &[], rect, 64).err(),
            Some(FlowQueryError::BadGoal)
        );
        assert_eq!(
            view.query(Profile::HOVER, &[u32::MAX], rect, 64).err(),
            Some(FlowQueryError::BadGoal)
        );
        assert_eq!(
            view.query(Profile(200), &[goal], rect, 64).err(),
            Some(FlowQueryError::BadProfile)
        );
        let off = CellRect {
            x0: 300,
            z0: 0,
            x1: 400,
            z1: 10,
        };
        assert_eq!(
            view.query(Profile::HOVER, &[goal], off, 64).err(),
            Some(FlowQueryError::EmptyWindow)
        );
        // Above the limit: the clustered flow's window.
        view.full_max_cells = 1000;
        let c = view.query(Profile::HOVER, &[goal], rect, 4096).expect("answer");
        assert_eq!(c.kind, FlowQueryKind::Clustered);
        let mut flow = ClusterFlow::build(&g, Profile::HOVER, &[goal], 1 << 16);
        assert_eq!(c.view, flow.view(&g, rect, 4096));
        assert!(c.bits_hash.is_none() && c.cluster_stats.is_some());
        view.clear();
        assert_eq!(
            view.query(Profile::HOVER, &[goal], rect, 64).err(),
            Some(FlowQueryError::NoGrid)
        );
    }
}
