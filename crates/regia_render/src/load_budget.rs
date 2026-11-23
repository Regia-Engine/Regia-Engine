// SPDX-License-Identifier: MPL-2.0
//! Per-frame load budget (ADR-7884): bounds how much of a frame's main-thread time can go to work
//! the scene warm did not cover, so a late spawn, a streaming LOD swap or a warm the watchdog gave
//! up on costs a few frames of pop-in instead of one 200-500 ms stall.
//!
//! [`FrameLoadBudget`] is opt-in. The default ([`LateDraw::Block`], no caps) is what every release
//! before it did: a frame uploads every mesh it references and builds every pipeline it draws with.
//! With [`LateDraw::Defer`]:
//!
//! - **Uploads.** At the start of the draw list the meshes the frame references but the mesh cache
//!   does not hold are ordered nearest first (squared distance from the eye to the instance's
//!   origin, then submission order for ties) and admitted while the bytes admitted so far are
//!   under [`FrameLoadBudget::upload_bytes_per_frame`]. The mesh that crosses the budget is still
//!   admitted, so a frame uploads at most the budget plus one mesh, and a single mesh larger than
//!   the budget still lands (alone) on its frame. The other meshes' draws are skipped this frame
//!   and planned again on the next one, so a burst is resident after about
//!   `ceil(total / budget)` frames. Ephemeral meshes (`mesh_key` 0) are always drawn and never
//!   charged: they are uploaded on every frame anyway.
//! - **Pipelines.** A material graph whose pipeline set is not built is built on the frame thread
//!   only while that keeps the frame's frame-thread pipeline creations within
//!   [`FrameLoadBudget::new_pipelines_per_frame`] (a set's size is the largest set seen so far,
//!   [`GRAPH_SET_PIPELINES_GUESS`] before the first). Otherwise the set goes to the material-graph
//!   worker threads and the draws that need it are skipped until it lands. The engine's deferred
//!   variants (blended, transmissive, mask-decal, thin-film) already skip their draw and build on
//!   the warm workers when a [`crate::PipelineWarmQueue`] runs; they count in
//!   [`LoadBudgetReport::async_pipelines_pending`].
//!
//! Warm jobs (a [`crate::PipelineWarmQueue`] worker) and [`crate::pipeline_warm::blocking_scope`]s
//! are never budgeted: a warm must cover the whole frame.
//!
//! The budget is per [`crate::FrameRenderer::render_into`] call (one view). The counters of the
//! last call are [`crate::FrameRenderer::load_budget_report`]; a `debug` log line marks the start
//! and the end of each deferral burst, never every frame.

use crate::key_hash::KeyMap;

/// What a draw does when what it needs is not resident (mesh) or not built (pipeline) within the
/// frame's [`FrameLoadBudget`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LateDraw {
    /// Upload and build everything the frame draws, now (every release before ADR-7884). The caps
    /// are ignored.
    #[default]
    Block,
    /// Work past the caps waits for later frames (uploads) or the worker threads (pipelines), and
    /// the draws that need it are skipped until it is done.
    Defer,
}

impl LateDraw {
    /// The protocol name (`block`, `defer`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Defer => "defer",
        }
    }

    /// The mode named `name` ([`Self::name`]).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "block" => Some(Self::Block),
            "defer" => Some(Self::Defer),
            _ => None,
        }
    }
}

/// The per-frame load budget of a [`crate::FrameRenderer`] ([`crate::FrameRenderer::set_load_budget`]).
/// The default changes nothing (see the module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FrameLoadBudget {
    /// Most mesh bytes (vertices plus indices) one frame uploads to the mesh cache, plus the one
    /// mesh that crosses it. `None`: no cap.
    pub upload_bytes_per_frame: Option<u64>,
    /// Most pipelines the frame thread creates in one frame for material-graph draws. `None`: no
    /// cap.
    pub new_pipelines_per_frame: Option<u32>,
    /// What happens past the caps. [`LateDraw::Block`] ignores them.
    pub late: LateDraw,
}

impl FrameLoadBudget {
    /// The values `book/user-guide/do27-host-checklist.md` recommends for a 60 fps target: 64 MiB
    /// and 8 pipelines per frame, deferred.
    pub const RECOMMENDED_60FPS: Self = Self {
        upload_bytes_per_frame: Some(64 << 20),
        new_pipelines_per_frame: Some(8),
        late: LateDraw::Defer,
    };

    /// The upload cap in force: `Some` only under [`LateDraw::Defer`].
    #[must_use]
    pub fn upload_cap(&self) -> Option<u64> {
        match self.late {
            LateDraw::Defer => self.upload_bytes_per_frame,
            LateDraw::Block => None,
        }
    }

    /// The frame-thread pipeline cap in force: `Some` only under [`LateDraw::Defer`].
    #[must_use]
    pub fn pipeline_cap(&self) -> Option<u32> {
        match self.late {
            LateDraw::Defer => self.new_pipelines_per_frame,
            LateDraw::Block => None,
        }
    }

    /// Whether this budget can change a frame (a cap under [`LateDraw::Defer`]).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.upload_cap().is_some() || self.pipeline_cap().is_some()
    }
}

/// What the load budget did on the last [`crate::FrameRenderer::render_into`]
/// ([`crate::FrameRenderer::load_budget_report`]). All zero under the default budget, except
/// [`Self::async_pipelines_pending`], which counts the engine's background builds either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadBudgetReport {
    /// Distinct meshes the frame referenced and left for a later frame (past the upload cap).
    pub deferred_meshes: u32,
    /// Their vertex and index bytes.
    pub deferred_bytes: u64,
    /// Draws skipped this frame: their mesh was deferred or their material graph's pipelines
    /// are still building on a worker.
    pub deferred_draws: u32,
    /// Pipeline builds the frame waits on that run off the frame thread: material-graph sets on
    /// their workers plus the engine's deferred variants (process-wide,
    /// [`crate::pipeline_warm::deferred_pending`]). Read when the report is taken.
    pub async_pipelines_pending: u32,
    /// Distinct meshes the plan admitted for upload this frame (0 without an upload cap).
    pub admitted_meshes: u32,
    /// Their vertex and index bytes.
    pub admitted_bytes: u64,
    /// Material-graph sets this frame sent to the workers instead of building them here.
    pub async_graph_sets: u32,
    /// Deferral bursts since the renderer was made: runs of frames with something deferred.
    pub bursts: u64,
}

/// Pipelines a material-graph set is assumed to cost before one was built and measured. Larger
/// than any set the engine builds (forward, blend modes, transmission, depth, instanced and the
/// vertex-stage passes), so a cap below it sends every set to the workers until one is measured.
pub const GRAPH_SET_PIPELINES_GUESS: u32 = 32;

/// One mesh the frame references that the mesh cache does not hold ([`admit`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PendingUpload {
    /// The cache key the draw list uses.
    pub(crate) key: u64,
    /// Vertex plus index bytes.
    pub(crate) bytes: u64,
    /// Squared distance from the eye to the instance's origin.
    pub(crate) distance_sq: f32,
    /// First submission index that references it.
    pub(crate) order: u32,
}

/// Admits `pending` (distinct keys) nearest first while the admitted bytes are under `cap`; the
/// mesh that crosses `cap` is admitted too. Fills `admitted` with the admitted keys and returns
/// `(admitted meshes, admitted bytes, deferred meshes, deferred bytes)`. Sorts `pending` in place.
pub(crate) fn admit(
    pending: &mut [PendingUpload],
    cap: u64,
    admitted: &mut KeyMap<u64, ()>,
) -> (u32, u64, u32, u64) {
    pending.sort_by(|a, b| {
        a.distance_sq
            .total_cmp(&b.distance_sq)
            .then(a.order.cmp(&b.order))
    });
    let (mut used, mut count) = (0u64, 0u32);
    let (mut deferred, mut deferred_bytes) = (0u32, 0u64);
    for p in pending.iter() {
        if used < cap {
            used = used.saturating_add(p.bytes);
            count += 1;
            admitted.insert(p.key, ());
        } else {
            deferred += 1;
            deferred_bytes = deferred_bytes.saturating_add(p.bytes);
        }
    }
    (count, used, deferred, deferred_bytes)
}

/// The renderer's load-budget state: the budget, this frame's plan and counters, and the burst
/// log state.
#[derive(Default)]
pub(crate) struct LoadBudgetState {
    pub(crate) budget: FrameLoadBudget,
    /// The host set the budget (a warm's stand-in hands it over, [`Self::absorb`]).
    pub(crate) explicit: bool,
    /// This frame's counters ([`LoadBudgetReport::async_pipelines_pending`] is filled on read).
    pub(crate) report: LoadBudgetReport,
    /// The upload cap applies to this frame (set by the plan).
    pub(crate) gate_uploads: bool,
    /// The pipeline cap applies to this frame.
    pub(crate) gate_pipelines: bool,
    /// Keys the plan admitted this frame.
    pub(crate) admitted: KeyMap<u64, ()>,
    /// Scratch for the plan.
    pub(crate) pending: Vec<PendingUpload>,
    /// Dedup scratch for the plan.
    pub(crate) seen: KeyMap<u64, ()>,
    /// Frame-thread pipeline count when the draw list started.
    pub(crate) pipelines_at_start: u64,
    /// Largest material-graph set measured on this renderer (pipelines).
    pub(crate) graph_set_pipelines: u32,
    /// A burst is running: the last frame deferred something.
    in_burst: bool,
    /// Frames in the running burst.
    burst_frames: u32,
}

impl LoadBudgetState {
    /// Starts a frame's bookkeeping: logs a burst edge from the previous frame's counters, then
    /// clears them. `budgeted`: this frame may defer (not a warm job or a blocking scope).
    pub(crate) fn begin_frame(&mut self, budgeted: bool, pipelines_now: u64) {
        let last = self.report;
        let deferred = last.deferred_draws > 0 || last.deferred_meshes > 0;
        if deferred && !self.in_burst {
            self.in_burst = true;
            self.burst_frames = 0;
            self.report.bursts += 1;
            tracing::debug!(
                target: "regia_render",
                deferred_meshes = last.deferred_meshes,
                deferred_mib = last.deferred_bytes as f64 / 1_048_576.0,
                deferred_draws = last.deferred_draws,
                async_graph_sets = last.async_graph_sets,
                "load budget: deferring late work over the next frames"
            );
        } else if !deferred && self.in_burst {
            self.in_burst = false;
            tracing::debug!(
                target: "regia_render",
                frames = self.burst_frames,
                "load budget: deferred work resident"
            );
        }
        if self.in_burst {
            self.burst_frames = self.burst_frames.saturating_add(1);
        }
        self.report = LoadBudgetReport {
            bursts: self.report.bursts,
            ..LoadBudgetReport::default()
        };
        self.admitted.clear();
        self.gate_uploads = budgeted && self.budget.upload_cap().is_some();
        self.gate_pipelines = budgeted && self.budget.pipeline_cap().is_some();
        self.pipelines_at_start = pipelines_now;
    }

    /// Whether a material-graph set may be built on the frame thread now, `created` pipelines
    /// into the frame.
    pub(crate) fn graph_set_fits(&self, created: u64) -> bool {
        let Some(cap) = self.budget.pipeline_cap().filter(|_| self.gate_pipelines) else {
            return true;
        };
        let set = if self.graph_set_pipelines == 0 {
            GRAPH_SET_PIPELINES_GUESS
        } else {
            self.graph_set_pipelines
        };
        created.saturating_add(u64::from(set)) <= u64::from(cap)
    }

    /// Takes the host's budget from a warm's stand-in renderer when the host set one there.
    pub(crate) fn absorb(&mut self, stand_in: &Self) {
        if stand_in.explicit {
            self.budget = stand_in.budget;
            self.explicit = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(key: u64, bytes: u64, distance_sq: f32, order: u32) -> PendingUpload {
        PendingUpload {
            key,
            bytes,
            distance_sq,
            order,
        }
    }

    #[test]
    fn admission_is_nearest_first_and_stops_after_the_mesh_that_crosses_the_cap() {
        let mut pending = vec![
            p(1, 40, 9.0, 0),
            p(2, 40, 1.0, 1),
            p(3, 40, 4.0, 2),
            p(4, 40, 1.0, 3),
        ];
        let mut admitted = KeyMap::default();
        let (n, bytes, deferred, deferred_bytes) = admit(&mut pending, 100, &mut admitted);
        // 2 and 4 tie at distance 1 (submission order), then 3 crosses 100 and is still admitted.
        assert_eq!((n, bytes, deferred, deferred_bytes), (3, 120, 1, 40));
        assert!(admitted.contains_key(&2) && admitted.contains_key(&4) && admitted.contains_key(&3));
        assert!(!admitted.contains_key(&1), "the farthest waits");
    }

    #[test]
    fn a_mesh_larger_than_the_cap_lands_alone() {
        let mut pending = vec![p(7, 500, 0.0, 0), p(8, 1, 1.0, 1)];
        let mut admitted = KeyMap::default();
        assert_eq!(admit(&mut pending, 64, &mut admitted), (1, 500, 1, 1));
    }

    #[test]
    fn the_default_budget_gates_nothing() {
        let budget = FrameLoadBudget::default();
        assert!(!budget.is_active());
        let block_with_caps = FrameLoadBudget {
            late: LateDraw::Block,
            ..FrameLoadBudget::RECOMMENDED_60FPS
        };
        assert!(!block_with_caps.is_active(), "Block ignores the caps");
        assert!(FrameLoadBudget::RECOMMENDED_60FPS.is_active());
        let mut state = LoadBudgetState {
            budget: block_with_caps,
            ..LoadBudgetState::default()
        };
        state.begin_frame(true, 0);
        assert!(!state.gate_uploads && !state.gate_pipelines);
        assert!(state.graph_set_fits(1_000));
    }

    #[test]
    fn graph_sets_fit_by_the_largest_measured_set() {
        let mut state = LoadBudgetState {
            budget: FrameLoadBudget {
                new_pipelines_per_frame: Some(24),
                late: LateDraw::Defer,
                ..FrameLoadBudget::default()
            },
            ..LoadBudgetState::default()
        };
        state.begin_frame(true, 0);
        assert!(!state.graph_set_fits(0), "unmeasured: the guess (32) is over 24");
        state.graph_set_pipelines = 10;
        assert!(state.graph_set_fits(14));
        assert!(!state.graph_set_fits(15));
        state.begin_frame(false, 0);
        assert!(state.graph_set_fits(1_000), "a warm job is never budgeted");
    }

    #[test]
    fn bursts_are_counted_once_per_run_of_deferring_frames() {
        let mut state = LoadBudgetState::default();
        for deferring in [true, true, true, false, false, true, false] {
            state.begin_frame(true, 0);
            if deferring {
                state.report.deferred_draws = 3;
            }
        }
        state.begin_frame(true, 0);
        assert_eq!(state.report.bursts, 2);
        assert_eq!(state.report.deferred_draws, 0, "cleared each frame");
    }

    #[test]
    fn late_draw_names_round_trip() {
        for mode in [LateDraw::Block, LateDraw::Defer] {
            assert_eq!(LateDraw::from_name(mode.name()), Some(mode));
        }
        assert_eq!(LateDraw::from_name("Defer"), None);
    }
}
