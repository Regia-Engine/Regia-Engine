// SPDX-License-Identifier: MPL-2.0
//! Resident draws: a mesh whose host dropped its CPU copy draws from its GPU buffers (ADR-7891).
//!
//! A [`FrameMesh`] with empty `vertices` and `indices`, a host key (not `0`, not
//! [`super::EPHEMERAL_KEY`]), no joints and no LOD dither is a *resident draw* when the mesh cache
//! holds its key: the colour, shadow and reflection passes draw the cached buffers, culled by the
//! rest-pose bound computed at upload. Without a resident entry it is skipped, as an empty mesh
//! always was. [`FrameRenderer::host_mesh_bytes_needed`] tells a host (`crate::host_memory`) when
//! it must pass the bytes instead.
//!
//! The renderer readers of CPU vertices and how each is served for a resident draw:
//! - upload, in-place rewrite, rest bound, texture-stream UV span: once, at upload, from the bytes
//!   (a non-resident mesh needs bytes);
//! - frustum and shadow culling, blended sort depth: the entry's rest bound;
//! - camera-cut detection: object keys (stable for a host's objects), so a resident draw's empty
//!   content signature does not cut;
//! - the visibility buffer's meshlets, DDGI probe geometry and brick bakes, the traced sun and
//!   ReSTIR GI casters, emissive mesh lights, lightmap and graph second-UV streams, glass canopy
//!   tests: they walk vertices on later frames, so a frame with any of them on needs bytes
//!   ([`FrameRenderer::gpu_only_draw_supported`]);
//! - skinning, morph targets, mask decals and scalar overlays: per-object CPU consumers the host
//!   knows about; it pins those meshes (`crate::host_memory::HostMeshStore::pin`).

use super::{EPHEMERAL_KEY, FrameMesh, FrameRenderer, LOD_DITHER_NONE};
use crate::lab_frame::LabMaterial;
use crate::profile::FrameQuality;

impl FrameRenderer {
    /// Whether the mesh cache holds `mesh_key`'s GPU vertex and index buffers now (drawn whole,
    /// LOD 0). A resident mesh can be drawn without CPU bytes when
    /// [`Self::gpu_only_draw_supported`] holds; eviction, [`Self::release_meshes`] and device loss
    /// clear it.
    #[must_use]
    pub fn mesh_buffers_resident(&self, mesh_key: u64) -> bool {
        mesh_key != 0 && mesh_key & EPHEMERAL_KEY != EPHEMERAL_KEY && self.meshes.contains_key(&mesh_key)
    }

    /// Whether a frame drawn with `quality` can draw a rigid mesh of `material` from its GPU
    /// buffers alone: no feature of that frame walks the mesh's CPU vertices (module docs of
    /// `frame::host_geometry` list them).
    #[must_use]
    pub fn gpu_only_draw_supported(&self, material: &LabMaterial, quality: &FrameQuality) -> bool {
        let frame_walks_vertices = quality.visibility_buffer
            || quality.gi_probes
            || self.ddgi.enabled == Some(true)
            || self.ddgi.active
            || quality.sun_shadow_trace != crate::hwrt::RtShadowRequest::Off
            || quality.restir_gi
            || self.lightmap_forward.active();
        let material_walks_vertices = (quality.emissive_lights > 0
            && crate::emissive_lights::material_emits(material))
            || material.surface_graph.is_some()
            || material.transmission > 0.0;
        !frame_walks_vertices && !material_walks_vertices
    }

    /// Whether a host must pass `mesh_key`'s CPU vertices and indices in the next frame drawn with
    /// `quality`: the mesh is not resident ([`Self::mesh_buffers_resident`]), or the frame cannot draw it
    /// GPU-only ([`Self::gpu_only_draw_supported`]). `false` means a resident draw (empty slices)
    /// draws the same pixels.
    #[must_use]
    pub fn host_mesh_bytes_needed(
        &self,
        mesh_key: u64,
        material: &LabMaterial,
        quality: &FrameQuality,
    ) -> bool {
        !self.mesh_buffers_resident(mesh_key) || !self.gpu_only_draw_supported(material, quality)
    }

    /// Whether `mesh` is a resident draw (module docs).
    pub(super) fn is_resident_draw(&self, mesh: &FrameMesh<'_>) -> bool {
        mesh.vertices.is_empty()
            && mesh.indices.is_empty()
            && mesh.joints.is_empty()
            && mesh.lod_dither == LOD_DITHER_NONE
            && self.mesh_buffers_resident(mesh.mesh_key)
    }

    /// Whether the frame draws `mesh`: it has bytes or is a counted draw (`has_geometry`,
    /// ADR-7284), or it is a resident draw (ADR-7891).
    pub(super) fn frame_draws_mesh(&self, mesh: &FrameMesh<'_>) -> bool {
        self.has_geometry(mesh) || self.is_resident_draw(mesh)
    }
}
