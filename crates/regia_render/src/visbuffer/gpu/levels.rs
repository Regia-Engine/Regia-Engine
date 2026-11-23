// SPDX-License-Identifier: MPL-2.0
//! Per-mesh LOD level runs for the instance cull (ADR-3915): which part of a mesh's cluster list an
//! instance's distance can select, so a near instance does not spend cull work on the coarse
//! levels (voxel far levels included) and a far one not on the fine levels.
//!
//! A mesh's clusters are stored level by level (the cooker appends each simplification level, and
//! [`crate::visbuffer::VoxelFarProxy::extend_meshlets`] appends the voxel levels after the DAG), so
//! each run of equal `level` is a contiguous range. Per run the table keeps the bounds
//! `vis_cull.wgsl` `instance_range` tests: the smallest own error with the farthest reach of the
//! runs's LOD spheres, and the largest parent error with the farthest reach of its parent spheres.
//! The test is conservative, so the selected clusters, and with them the frame, do not change.

use super::super::meshlet::Meshlet;
use crate::device::{TrackedDevice, TrackedQueue};

/// Runs kept per mesh; a mesh with more merges its last runs into one (still conservative).
pub(super) const MAX_RUNS_PER_MESH: usize = 32;
/// Records the table's buffer holds at first (32 bytes each), so a level does not grow it.
const INITIAL_RECORDS: u64 = 1024;
/// `parent_error` at or above this is a root (`ROOT` in `vis_cull.wgsl`).
const ROOT: f32 = 3.0e38;

/// One run (`LevelRun` in `vis_cull.wgsl`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(super) struct LevelRun {
    pub(super) first: u32,
    pub(super) end: u32,
    pub(super) min_error: f32,
    pub(super) reach: f32,
    pub(super) max_parent_error: f32,
    pub(super) parent_reach: f32,
    pad: [u32; 2],
}

fn offset(center: [f32; 4], sphere: [f32; 4]) -> f32 {
    let d = [
        sphere[0] - center[0],
        sphere[1] - center[1],
        sphere[2] - center[2],
    ];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

impl LevelRun {
    fn of(first: usize, meshlets: &[Meshlet], center: [f32; 4]) -> Self {
        let mut run = Self {
            first: first as u32,
            end: (first + meshlets.len()) as u32,
            min_error: f32::MAX,
            reach: 0.0,
            max_parent_error: 0.0,
            parent_reach: 0.0,
            pad: [0; 2],
        };
        for m in meshlets {
            // The errors the cull reads (the uploaded records fold them the same way): a NaN
            // parent is a root and a non-finite own error is 0. Raw, `f32::max` drops a NaN
            // parent and an all-infinite run keeps `f32::MAX`, so the run would be skipped while
            // its clusters are selected.
            let (lod_error, parent_error) =
                crate::visbuffer::residency::finite_errors(m.lod_error, m.parent_error);
            run.min_error = run.min_error.min(lod_error.max(0.0));
            run.reach = run.reach.max(offset(center, m.lod));
            run.max_parent_error = if parent_error >= ROOT || run.max_parent_error >= ROOT {
                ROOT
            } else {
                run.max_parent_error.max(parent_error)
            };
            run.parent_reach = run
                .parent_reach
                .max(offset(center, m.parent) + m.parent[3].max(0.0));
        }
        // The bounds are conservative only when finite; anything else keeps the run.
        if !run.min_error.is_finite() || !run.reach.is_finite() {
            run.min_error = 0.0;
            run.reach = 0.0;
        }
        if !run.parent_reach.is_finite() || run.max_parent_error.is_nan() {
            run.max_parent_error = ROOT;
        }
        run
    }

    /// `self` grown to cover `other` too.
    fn merged(self, other: Self) -> Self {
        Self {
            first: self.first.min(other.first),
            end: self.end.max(other.end),
            min_error: self.min_error.min(other.min_error),
            reach: self.reach.max(other.reach),
            max_parent_error: if self.max_parent_error >= ROOT || other.max_parent_error >= ROOT {
                ROOT
            } else {
                self.max_parent_error.max(other.max_parent_error)
            },
            parent_reach: self.parent_reach.max(other.parent_reach),
            pad: [0; 2],
        }
    }
}

/// The runs of equal `level` of one mesh's clusters (`bounds`: the mesh's bounding sphere), at most
/// [`MAX_RUNS_PER_MESH`]. One run (or none) means nothing can be skipped: the caller keeps the
/// whole list.
pub(super) fn runs_of(meshlets: &[Meshlet], bounds: [f32; 4]) -> Vec<LevelRun> {
    let mut runs: Vec<LevelRun> = Vec::new();
    let mut start = 0;
    while start < meshlets.len() {
        let level = meshlets[start].level;
        let len = meshlets[start..].iter().take_while(|m| m.level == level).count();
        let run = LevelRun::of(start, &meshlets[start..start + len], bounds);
        if runs.len() == MAX_RUNS_PER_MESH
            && let Some(last) = runs.last_mut()
        {
            *last = last.merged(run);
        } else {
            runs.push(run);
        }
        start += len;
    }
    if runs.len() <= 1 {
        runs.clear();
    }
    runs
}

/// Every resident mesh's runs, in one GPU buffer bound to the cull (binding 9).
#[derive(Default)]
pub(super) struct LevelTable {
    records: Vec<LevelRun>,
    /// Records no mesh uses any more (compacted when they are over half the table).
    holes: usize,
    dirty: bool,
    /// The records written since the last upload, `[start, end)` (ADR-3118: a residency flip
    /// rewrites one mesh's runs and uploads only them); `None` with `dirty` uploads the table.
    dirty_span: Option<(usize, usize)>,
    buffer: Option<(wgpu::Buffer, u64)>,
}

impl LevelTable {
    /// Appends a mesh's runs; returns their `(first record, count)` (`(0, 0)` without runs).
    pub(super) fn insert(&mut self, runs: &[LevelRun]) -> (u32, u32) {
        if runs.is_empty() {
            return (0, 0);
        }
        let first = self.records.len() as u32;
        self.records.extend_from_slice(runs);
        self.mark(first as usize, self.records.len());
        (first, runs.len() as u32)
    }

    /// Marks `[start, end)` for the next upload.
    fn mark(&mut self, start: usize, end: usize) {
        self.dirty_span = if self.dirty && self.dirty_span.is_none() {
            None
        } else {
            Some(match self.dirty_span {
                Some((s, e)) if self.dirty => (s.min(start), e.max(end)),
                _ => (start, end),
            })
        };
        self.dirty = true;
    }

    /// Rewrites a mesh's runs in place (`first` from [`Self::insert`], same count); false (nothing
    /// written) when the range is not the table's. Returns whether any record changed.
    pub(super) fn overwrite(&mut self, first: u32, runs: &[LevelRun]) -> bool {
        let start = first as usize;
        let Some(target) = self.records.get_mut(start..start + runs.len()) else {
            return false;
        };
        if target == runs {
            return false;
        }
        target.copy_from_slice(runs);
        self.mark(start, start + runs.len());
        true
    }

    /// A mesh's runs are no longer used.
    pub(super) fn release(&mut self, count: u32) {
        self.holes += count as usize;
    }

    /// True when the holes are over half the table: the caller rebuilds it from its meshes
    /// ([`Self::rebuild`]) and their instances.
    pub(super) fn wants_compaction(&self) -> bool {
        self.holes > 64 && self.holes * 2 > self.records.len()
    }

    /// The records, for tests.
    #[cfg(test)]
    pub(super) fn records(&self) -> &[LevelRun] {
        &self.records
    }

    /// Empties the table for a rebuild; the caller inserts every resident mesh again.
    pub(super) fn rebuild(&mut self) {
        self.records.clear();
        self.holes = 0;
        self.dirty = true;
        self.dirty_span = None;
    }

    /// Records in the table (holes included).
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.records.len()
    }

    /// Makes the buffer hold the table and uploads it when it changed. True when the buffer was
    /// replaced (bind groups over it are rebuilt).
    pub(super) fn sync(&mut self, d: &TrackedDevice, queue: &TrackedQueue) -> bool {
        let needed = (self.records.len() as u64).max(1);
        let replaced = self.buffer.as_ref().is_none_or(|(_, have)| *have < needed);
        if replaced {
            let capacity = needed.next_power_of_two().max(INITIAL_RECORDS);
            let buffer = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("vis level runs"),
                size: capacity * std::mem::size_of::<LevelRun>() as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.buffer = Some((buffer, capacity));
            self.dirty = true;
            self.dirty_span = None;
        }
        if self.dirty {
            if let Some((buffer, _)) = &self.buffer
                && !self.records.is_empty()
            {
                let (start, end) = self
                    .dirty_span
                    .map_or((0, self.records.len()), |(s, e)| (s, e.min(self.records.len())));
                if start < end {
                    queue.write_buffer(
                        buffer,
                        (start * std::mem::size_of::<LevelRun>()) as u64,
                        bytemuck::cast_slice(&self.records[start..end]),
                    );
                }
            }
            self.dirty = false;
            self.dirty_span = None;
        }
        replaced
    }

    /// The table's buffer (after [`Self::sync`]).
    pub(super) fn buffer(&self) -> Option<&wgpu::Buffer> {
        self.buffer.as_ref().map(|(buffer, _)| buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meshlet(level: u32, lod: [f32; 4], lod_error: f32, parent: [f32; 4], parent_error: f32) -> Meshlet {
        Meshlet {
            lod,
            lod_error,
            parent,
            parent_error,
            level,
            ..bytemuck::Zeroable::zeroed()
        }
    }

    #[test]
    fn runs_follow_the_levels_and_keep_conservative_bounds() {
        let c = [0.0, 0.0, 0.0, 2.0];
        let meshlets = [
            meshlet(0, [1.0, 0.0, 0.0, 0.5], 0.0, [0.0, 0.0, 0.0, 1.5], 0.1),
            meshlet(0, [0.0, 2.0, 0.0, 0.5], 0.0, [0.0, 0.5, 0.0, 1.0], 0.2),
            meshlet(1, [0.0, 0.0, 0.0, 1.5], 0.1, [0.0, 0.0, 0.0, 2.0], 0.4),
            meshlet(1, [0.0, 0.0, 3.0, 1.0], 0.2, [0.0, 0.0, 0.0, 2.0], ROOT),
        ];
        let runs = runs_of(&meshlets, c);
        assert_eq!(runs.len(), 2);
        assert_eq!((runs[0].first, runs[0].end), (0, 2));
        assert_eq!((runs[1].first, runs[1].end), (2, 4));
        assert_eq!(runs[0].min_error, 0.0);
        assert_eq!(runs[0].reach, 2.0, "the farthest lod center");
        assert!((runs[0].max_parent_error - 0.2).abs() < 1e-6);
        assert_eq!(
            runs[0].parent_reach, 1.5,
            "center offset plus radius, the largest"
        );
        assert!((runs[1].min_error - 0.1).abs() < 1e-6);
        assert_eq!(runs[1].reach, 3.0);
        assert_eq!(
            runs[1].max_parent_error, ROOT,
            "a root in the run keeps it for every far view"
        );
    }

    #[test]
    fn a_single_level_mesh_has_no_runs_and_many_levels_merge() {
        let c = [0.0; 4];
        let one = [meshlet(0, c, 0.0, c, ROOT), meshlet(0, c, 0.0, c, ROOT)];
        assert!(runs_of(&one, c).is_empty(), "nothing to skip");
        let many: Vec<Meshlet> = (0..40)
            .map(|level| meshlet(level, c, level as f32, c, level as f32 + 1.0))
            .collect();
        let runs = runs_of(&many, c);
        assert_eq!(runs.len(), MAX_RUNS_PER_MESH);
        let last = runs[MAX_RUNS_PER_MESH - 1];
        assert_eq!(
            (last.first, last.end),
            (31, 40),
            "the tail merges into the last run"
        );
        assert_eq!(last.min_error, 31.0);
        assert_eq!(last.max_parent_error, 40.0);
    }

    /// A run's bounds read the errors the cull reads (`finite_errors`): a NaN parent is a root
    /// and an infinite own error is 0. Raw, the coarsest run of a mesh with a NaN root parent got
    /// `max_parent_error` 0 and a run of infinite own errors kept `f32::MAX`, so the cull skipped
    /// runs whose clusters it selects (a mesh with holes, or gone at distance).
    #[test]
    fn corr_visbuffer_non_finite_errors_fold_like_the_records() {
        let c = [0.0; 4];
        let runs = runs_of(&[meshlet(0, c, 0.0, c, 1.0), meshlet(1, c, 1.0, c, f32::NAN)], c);
        assert_eq!(runs.len(), 2);
        assert!(
            runs[1].max_parent_error >= ROOT,
            "a NaN parent is a root: {}",
            runs[1].max_parent_error
        );
        let runs = runs_of(
            &[
                meshlet(0, c, 0.0, c, 1.0),
                meshlet(1, c, f32::INFINITY, c, f32::INFINITY),
            ],
            c,
        );
        assert_eq!(
            runs[1].min_error, 0.0,
            "an infinite own error is uploaded as 0, so the run's smallest is 0"
        );
    }

    /// A rewrite in place marks only its records; appends extend the span; a rebuild uploads all.
    #[test]
    fn overwrites_mark_only_their_records() {
        let mut table = LevelTable::default();
        let c = [0.0; 4];
        let runs = runs_of(&[meshlet(0, c, 0.0, c, 1.0), meshlet(1, c, 1.0, c, ROOT)], c);
        table.insert(&runs);
        table.insert(&runs);
        table.dirty = false;
        table.dirty_span = None;
        let mut other = runs.clone();
        other[0].min_error = 5.0;
        assert!(table.overwrite(2, &other));
        assert_eq!(table.dirty_span, Some((2, 4)));
        assert!(!table.overwrite(2, &other), "unchanged runs write nothing");
        assert!(!table.overwrite(3, &other), "out of the table");
        table.insert(&runs);
        assert_eq!(table.dirty_span, Some((2, 6)));
        table.rebuild();
        assert_eq!(table.dirty_span, None);
        assert!(table.dirty);
    }

    #[test]
    fn the_table_appends_and_compacts_by_holes() {
        let mut table = LevelTable::default();
        let c = [0.0; 4];
        let runs = runs_of(&[meshlet(0, c, 0.0, c, 1.0), meshlet(1, c, 1.0, c, ROOT)], c);
        assert_eq!(table.insert(&runs), (0, 2));
        assert_eq!(table.insert(&runs), (2, 2));
        assert_eq!(table.insert(&[]), (0, 0));
        table.release(2);
        assert!(!table.wants_compaction(), "few holes stay");
        for _ in 0..100 {
            table.insert(&runs);
        }
        for _ in 0..80 {
            table.release(2);
        }
        assert!(table.wants_compaction());
        table.rebuild();
        assert_eq!(table.len(), 0);
    }
}
