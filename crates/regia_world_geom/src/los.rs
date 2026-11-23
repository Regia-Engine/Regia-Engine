// SPDX-License-Identifier: MPL-2.0
//! The deterministic AI's line of sight over this crate's geometry (plan row S-10, ADR-8395).
//!
//! `regia_detsim::ai::LineOfSight` (re-exported as `regia_ai::det::LineOfSight`) is the seam the sighting board
//! and the influence queries ask. This module implements it for the Stage A queries:
//!
//! - [`TerrainSampler`]: the heightfield rule (`regia_detsim::ai::heightfield_los_clear` over
//!   [`TerrainSampler::height_at_world`]): Direct Order 27's `los_clear` bits, so its sim keeps its checksums.
//!   A heightfield cannot see into a tunnel: the ground over a covered gallery blocks.
//! - [`TunnelCollisionMesh`]: rock between the two points: clear unless the segment crosses a wall or lid
//!   triangle ([`TunnelCollisionMesh::segment_blocker`]; floors are not tested, both faces count). A line
//!   through an open mouth, down a ramp or along a tube is clear; a line through a tube wall is not.
//!
//! The native query of plan row W-11 (one ground, mouths, blockers and segments with no allocation) replaces
//! both with one implementation; until then a host that needs both rules composes them with
//! `regia_detsim::ai::LosFn`. `segment_blocker` gathers its candidate triangles into a `Vec` (Stage A code as
//! copied), so the mesh rule allocates per call.

use regia_detsim::ai::{LineOfSight, heightfield_los_clear};

use crate::compat::mesh::TunnelCollisionMesh;
use crate::compat::sampler::TerrainSampler;

impl LineOfSight for TerrainSampler {
    fn clear(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        heightfield_los_clear(|x, z| self.height_at_world(x, z), from, to)
    }
}

impl LineOfSight for TunnelCollisionMesh {
    fn clear(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        self.segment_blocker(from, to).is_none()
    }
}
