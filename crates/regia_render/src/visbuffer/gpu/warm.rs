// SPDX-License-Identifier: MPL-2.0
//! What a game-frame warm leaves ready for the first dense frame of play (ADR-2689).
//!
//! The warm builds the path's pipelines (`warm_path`, ADR-2071); its lists and targets are sized by
//! the frame that first draws through the path. A profile that selects by density meets that frame
//! mid-play, the frame the scene turns dense, and before this every list and target of the path
//! was created on it. Two parts close that:
//!
//! - **The warm draws two window-size frames through the path** (`pipeline_warm.rs`,
//!   `warm_visibility_targets`): the ID, material-depth and Hi-Z targets, the per-pixel words of the
//!   software raster, the parameter blocks and every bind group over them exist at the window's size.
//! - **The lists are reserved** ([`ListReserve`]): the warm scene is not the dense one, so lists
//!   sized by it would be replaced when the scene turns dense. While a profile selects by density,
//!   the instance, previous-model, work-item, scan and visibility-bit lists (and the shadow cull's
//!   work items) hold at least what [`ListReserve::for_select`] gives, so the warmed buffers are the
//!   ones the dense frame draws with. A scene past the reserve still grows a list, once, on the
//!   frame it passes it, as any scene that outgrows its lists does.
//!
//! The warm frame runs after the warm's own frames, which noted the profile's selection (so the
//! reserve is set), and before its window-size frame, which leaves the path's active flag and the
//! mesh LOD hand-off as a warm without it does. Forcing the path resets the density gate's streak:
//! a warm scene that is itself dense enters the path up to two frames later than it would have.
//!
//! The reserve is a floor on capacities only: what the cull reads (counts, offsets, dispatch
//! sizes) is unchanged, so frames are the same with or without it. Profiles that do not select by
//! density reserve nothing.

use super::VisBufferPass;
use crate::visbuffer::{DenseSceneSelect, VisibilitySelect};

/// Instances the reserve never goes under (147 KiB of instances).
const MIN_INSTANCES: u64 = 1 << 10;
/// Instances the reserve never goes over (9 MiB of instances).
const MAX_INSTANCES: u64 = 1 << 16;
/// Clusters the reserve never goes over (128 KiB of visibility bits).
const MAX_CLUSTERS: u64 = 1 << 20;
/// Triangles per cluster the reserve assumes (the cooker fills clusters to 124; coarser levels
/// and partial clusters make the mean lower).
const TRIANGLES_PER_CLUSTER: u64 = 32;

/// Floors on the path's list capacities while a profile selects by density.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ListReserve {
    /// Instances (persistent and this frame's).
    pub(crate) instances: u64,
    /// 64-cluster work items.
    pub(crate) chunks: u64,
    /// Candidate clusters (one visibility bit each).
    pub(crate) clusters: u64,
}

impl ListReserve {
    /// The reserve of a dense-scene selection: four times the instance threshold (between 1024
    /// and 65536), and the larger of the triangle threshold's clusters at 32 triangles each and 16
    /// per reserved instance (at most 2^20). A scene enters the path at either threshold, so this
    /// covers the scenes that enter near them; denser scenes grow once.
    pub(crate) fn for_select(select: DenseSceneSelect) -> Self {
        let instances = (u64::from(select.enter_instances) * 4).clamp(MIN_INSTANCES, MAX_INSTANCES);
        let clusters = (select.enter_triangles / TRIANGLES_PER_CLUSTER).clamp(instances * 16, MAX_CLUSTERS);
        Self {
            instances,
            chunks: instances + clusters.div_ceil(64),
            clusters,
        }
    }

    /// The reserve of a profile's selection (nothing unless it selects by density).
    pub(crate) fn for_path(select: VisibilitySelect) -> Self {
        match select {
            VisibilitySelect::Always => Self::default(),
            VisibilitySelect::Dense(select) => Self::for_select(select),
        }
    }

    /// Both reserves' larger capacities.
    pub(crate) fn max(self, other: Self) -> Self {
        Self {
            instances: self.instances.max(other.instances),
            chunks: self.chunks.max(other.chunks),
            clusters: self.clusters.max(other.clusters),
        }
    }
}

impl VisBufferPass {
    /// The reserve the lists keep ([`ListReserve`]).
    pub(crate) fn list_reserve(&self) -> ListReserve {
        self.reserve
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reserve_follows_the_thresholds_within_its_bounds() {
        let default = ListReserve::for_select(DenseSceneSelect::DEFAULT);
        assert_eq!(default.instances, 4096 * 4);
        // 16 clusters per reserved instance outweighs the triangle threshold's 62,500 here.
        assert_eq!(default.clusters, 4096 * 4 * 16);
        let by_triangles = ListReserve::for_select(DenseSceneSelect {
            enter_triangles: 20_000_000,
            ..DenseSceneSelect::DEFAULT
        });
        assert_eq!(by_triangles.clusters, 20_000_000 / 32);
        assert_eq!(default.chunks, default.instances + default.clusters.div_ceil(64));
        // A selection by instances only (no triangle threshold) caps the clusters.
        let instances_only = ListReserve::for_select(DenseSceneSelect {
            enter_triangles: u64::MAX,
            enter_instances: 16,
            enter_frames: 2,
            leave_frames: 2,
        });
        assert_eq!(instances_only.instances, MIN_INSTANCES);
        assert_eq!(instances_only.clusters, MAX_CLUSTERS);
        let huge = ListReserve::for_select(DenseSceneSelect {
            enter_triangles: 0,
            enter_instances: u32::MAX,
            ..DenseSceneSelect::DEFAULT
        });
        assert_eq!(huge.instances, MAX_INSTANCES);
        assert_eq!(huge.clusters, MAX_INSTANCES * 16);
        // Nothing for a profile that does not select by density.
        assert_eq!(
            ListReserve::for_path(VisibilitySelect::Always),
            ListReserve::default()
        );
    }

    /// Only a density-selecting profile's frames set the reserve, and it stays a floor once set.
    #[test]
    fn a_dense_profile_reserves_and_an_always_frame_keeps_it() {
        let mut pass = VisBufferPass::default();
        let path = |select| crate::visbuffer::VisibilityPath {
            select,
            single_pass_resolve: false,
        };
        pass.note_path(true, path(VisibilitySelect::Always));
        assert_eq!(pass.list_reserve(), ListReserve::default());
        let dense = VisibilitySelect::Dense(DenseSceneSelect::DEFAULT);
        pass.note_path(true, path(dense));
        assert_eq!(pass.list_reserve(), ListReserve::for_path(dense));
        // The warm's forced frame notes `Always`: the reserve stays.
        pass.note_path(true, path(VisibilitySelect::Always));
        assert_eq!(pass.list_reserve(), ListReserve::for_path(dense));
    }
}
