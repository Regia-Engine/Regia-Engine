// SPDX-License-Identifier: MPL-2.0
//! Skinned instancing (ADR-1368): vertex-shader-skinned draws with a joint-palette bound
//! (ADR-248) join the GPU-driven colour batches, and the frame counts what skinned crowds cost.
//!
//! The technique is skinned instancing as Dudash describes it (NVIDIA, "Skinned Instancing",
//! 2007): every instance of one mesh is drawn by a single instanced draw, and each instance
//! finds its own bone matrices in a shared buffer through a per-instance offset. Here the shared
//! buffer is the frame's packed skin palette and the offset is the object's `skin.x`
//! (`joint_base`), which the depth prepass and the shadow maps already read per instance. This
//! module holds the switch and the counters; the routing is in `render_into` and
//! `crate::gpu_driven`.

use std::collections::HashSet;

use super::{DrawStats, FrameRenderer};

/// What one colour draw call can share: `(cull mode, mesh, material, graph code, solo object)`.
type GroupKey = (bool, u64, u64, Option<u64>, Option<u64>);

/// One skinned draw of the forward colour pass, as the counters see it.
#[derive(Clone, Copy, Debug)]
pub(super) struct SkinnedDraw {
    /// Drawn with the cull-none pipelines.
    pub(super) cull_off: bool,
    /// Mesh cache key.
    pub(super) mesh: u64,
    /// Material cache key.
    pub(super) material: u64,
    /// Material-graph code key whose pipeline set draws it (`None`: the standard surface).
    pub(super) graph: Option<u64>,
    /// A draw that can never share a draw call: a masked instance binds its own group 3
    /// (`mask_decals`). Its object key.
    pub(super) solo: Option<u64>,
    /// Drawn through the GPU-driven instanced batches.
    pub(super) instanced: bool,
}

/// The skinned counters of one frame ([`DrawStats::skinned_draws`] and the fields after it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct SkinnedCounts {
    /// Skinned draws in the colour pass.
    pub(super) draws: usize,
    /// Of [`Self::draws`], those in the instanced batches.
    pub(super) instanced: usize,
    /// Distinct `(cull mode, mesh, material, pipeline)` groups among [`Self::draws`].
    pub(super) batches_possible: usize,
}

impl SkinnedCounts {
    /// Counts `draws`. The pipeline of a standard draw follows from its mesh (LOD dither) and
    /// material (alpha mode); a graph draw adds its code key; a masked instance is its own group.
    pub(super) fn count(draws: impl Iterator<Item = SkinnedDraw>) -> Self {
        let mut groups: HashSet<GroupKey> = HashSet::new();
        let mut counts = Self::default();
        for draw in draws {
            counts.draws += 1;
            if draw.instanced {
                counts.instanced += 1;
            }
            groups.insert((draw.cull_off, draw.mesh, draw.material, draw.graph, draw.solo));
        }
        counts.batches_possible = groups.len();
        counts
    }

    /// Writes the counts into `stats`.
    pub(super) fn apply(self, stats: &mut DrawStats) {
        stats.skinned_draws = self.draws;
        stats.skinned_instanced = self.instanced;
        stats.skinned_batches_possible = self.batches_possible;
    }
}

impl FrameRenderer {
    /// Turns skinned instancing on or off (on by default, ADR-1368). On, a vertex-shader-skinned
    /// opaque or masked mesh with a joint-palette bound ([`Self::set_skinned_bounds`]) draws in
    /// the GPU-driven instanced batches of the colour pass, keyed by `(cull mode, mesh, material,
    /// skinned)`: a crowd of one skinned mesh and material is one indirect draw. The GPU cull
    /// tests each unit's palette sphere. Off, every skinned mesh draws one by one in the colour
    /// pass, as before v0.2.0. The depth prepass and the shadow maps instance skinned meshes
    /// either way.
    ///
    /// Only devices that run the GPU-driven batches (`INDIRECT_FIRST_INSTANCE` with multi-draw
    /// indirect, `FrameQuality::gpu_driven`) are affected. Compute-skinned twins
    /// ([`Self::set_compute_skinning`]), masked instances and skinned meshes without a bound
    /// always draw one by one.
    pub fn set_skinned_instancing(&mut self, enabled: bool) {
        self.skin_bounds.instancing = enabled;
    }

    /// Whether skinned meshes join the GPU-driven batches ([`Self::set_skinned_instancing`]).
    #[must_use]
    pub fn skinned_instancing(&self) -> bool {
        self.skin_bounds.instancing
    }
}

#[cfg(test)]
mod tests {
    use super::{SkinnedCounts, SkinnedDraw};

    fn draw(mesh: u64, material: u64) -> SkinnedDraw {
        SkinnedDraw {
            cull_off: false,
            mesh,
            material,
            graph: None,
            solo: None,
            instanced: false,
        }
    }

    #[test]
    fn a_crowd_of_one_mesh_and_material_is_one_possible_batch() {
        let crowd = (0..64).map(|_| draw(7, 3));
        let counts = SkinnedCounts::count(crowd);
        assert_eq!(counts.draws, 64);
        assert_eq!(counts.batches_possible, 1);
        assert_eq!(counts.instanced, 0);
    }

    #[test]
    fn cull_mode_material_graph_and_masked_instances_split_the_groups() {
        let draws = [
            draw(7, 3),
            draw(7, 3),
            // Another material.
            draw(7, 4),
            // The cull-none pipelines.
            SkinnedDraw {
                cull_off: true,
                ..draw(7, 3)
            },
            // A material graph's pipeline set.
            SkinnedDraw {
                graph: Some(11),
                ..draw(7, 3)
            },
            // Two masked instances: one group each.
            SkinnedDraw {
                solo: Some(100),
                ..draw(7, 3)
            },
            SkinnedDraw {
                solo: Some(101),
                ..draw(7, 3)
            },
            SkinnedDraw {
                instanced: true,
                ..draw(8, 3)
            },
        ];
        let counts = SkinnedCounts::count(draws.into_iter());
        assert_eq!(counts.draws, 8);
        assert_eq!(counts.instanced, 1);
        assert_eq!(counts.batches_possible, 7);
    }

    #[test]
    fn no_skinned_draws_count_zero() {
        assert_eq!(SkinnedCounts::count(std::iter::empty()), SkinnedCounts::default());
    }
}
