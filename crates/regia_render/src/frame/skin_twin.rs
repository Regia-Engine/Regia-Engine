// SPDX-License-Identifier: MPL-2.0
//! [`FrameRenderer`]'s compute skinning hooks (Tincal B2, `crate::skin_compute`).
//!
//! A skinned draw on the compute path is drawn from its twin: a mesh entry under the twin's
//! key that shares the source mesh's index range and points at the twin's skinned vertex
//! buffer. Every pass looks the draw's mesh up by key, so shadows, depth, colour and decal
//! stamps draw the skinned vertices without skinning them again.

use super::{FrameMesh, FrameRenderer, MeshEntry};
use crate::RenderDevice;
use crate::skin_compute::{SkinningStats, TwinRequest};
use crate::skinning::SkinningOptions;

impl FrameRenderer {
    /// Puts every skinned mesh on the compute path (`crate::skinning`): skinned once per frame
    /// in a compute pass and drawn from that result by every pass. Off by default; meshes with
    /// [`SkinningOptions`] that want it ([`Self::set_skinning_options`]) take the compute path
    /// either way. A device without compute keeps the vertex-shader path
    /// ([`Self::skinning_stats`]).
    pub fn set_compute_skinning(&mut self, enabled: bool) {
        self.skin_compute.compute_all = enabled;
    }

    /// Whether every skinned mesh takes the compute path ([`Self::set_compute_skinning`]).
    #[must_use]
    pub fn compute_skinning(&self) -> bool {
        self.skin_compute.compute_all
    }

    /// Replaces the per-instance skinning options (dual quaternions, eight influences, compute),
    /// keyed by [`FrameMesh::object_key`]. Pass [`crate::ExtractedSkinning::keys`] every frame;
    /// an empty iterator clears them. Options that change nothing are dropped.
    pub fn set_skinning_options(&mut self, options: impl IntoIterator<Item = (u64, SkinningOptions)>) {
        self.skin_compute.options.clear();
        for (key, value) in options {
            if value.wants_compute() {
                self.skin_compute.options.insert(key, value);
            }
        }
    }

    /// What the compute skinning path did on the last frame.
    #[must_use]
    pub fn skinning_stats(&self) -> SkinningStats {
        self.skin_compute.stats()
    }

    /// GPU bytes the compute skinning path holds (source vertex copies, each instance's two
    /// skinned output buffers, palettes and influence tables). They are not part of the mesh
    /// cache's budget; idle ones are freed after about 240 frames, or on the next frame after
    /// [`Self::trim_memory`].
    #[must_use]
    pub fn skinning_gpu_bytes(&self) -> u64 {
        self.skin_compute.gpu_bytes()
    }

    /// Starts the frame's compute skinning: drops idle twins' mesh entries.
    pub(super) fn begin_skin_twins(&mut self) {
        for key in self.skin_compute.begin_frame() {
            self.meshes.remove(&key);
        }
    }

    /// Forgets the twins of released source meshes (`release_meshes`).
    pub(super) fn release_skin_twins(&mut self, source_keys: &[u64]) {
        for key in self.skin_compute.release(source_keys) {
            self.meshes.remove(&key);
        }
    }

    /// The mesh key `mesh` draws with: its compute twin's when it is a skinned instance on the
    /// compute path (the twin's entry refreshed for this frame), else `None` (its own key, the
    /// vertex-shader path). Call after [`Self::ensure_mesh`] (and the second UV upload) for
    /// `mesh`.
    pub(super) fn skin_twin(
        &mut self,
        device: &RenderDevice,
        q: &crate::device::TrackedQueue,
        mesh: &FrameMesh<'_>,
        now: u64,
    ) -> Option<u64> {
        if mesh.joints.is_empty() || !self.skin_compute.wants(mesh.object_key) {
            return None;
        }
        // A counted mesh drawn whole skins from its adopted vertex buffer, which holds its four
        // strongest influences (ADR-8610, ADR-7721); a LOD or cross-fade variant keeps the
        // vertex-shader path.
        let counted = match Self::counted_draw(mesh) {
            Some((base, _)) if base != mesh.mesh_key || !self.asset_mesh_keys.contains(&base) => return None,
            Some(_) => {
                let entry = self.meshes.get(&mesh.mesh_key)?;
                let count =
                    u32::try_from(entry.vertex_bytes / std::mem::size_of::<crate::LabVertex>()).ok()?;
                Some((entry.vb.clone(), count))
            }
            None => None,
        };
        let joint_count = mesh.joints.len().min(crate::skin::MAX_JOINTS_PER_SKIN);
        let prev_ok = self
            .prev_palettes
            .get(&mesh.object_key)
            .is_some_and(|&(_, count)| count as usize == joint_count);
        let options = self.skin_compute.options.get(&mesh.object_key).cloned();
        let request = TwinRequest {
            mesh_key: mesh.mesh_key,
            object_key: mesh.object_key,
            vertices: mesh.vertices,
            buffer: counted.as_ref().map(|(vb, count)| (vb, *count)),
            joints: &mesh.joints[..joint_count],
            options: options.as_ref(),
            prev_ok,
            ephemeral: mesh.mesh_key >> 56 == super::EPHEMERAL_KEY >> 56,
        };
        let source = self.meshes.get(&mesh.mesh_key)?;
        let entry = MeshEntry {
            vb: source.vb.clone(),
            ib: source.ib.clone(),
            base_vertex: 0,
            first_index: source.first_index,
            leases: source.leases.clone(),
            uv1: source.uv1.clone(),
            index_count: source.index_count,
            // The source entry counts the arena bytes; the twin's buffers are its own.
            vertex_bytes: 0,
            index_bytes: 0,
            bounds: None,
            static_bound: false,
            uv_span: source.uv_span,
            last_used: now,
            dithered: source.dithered,
            generation: source.generation,
            content_rev: 0,
        };
        let (key, vb) = self.skin_compute.prepare(device, q, &request)?;
        self.meshes.insert(key, MeshEntry { vb, ..entry });
        Some(key)
    }

    /// Last frame's skinned vertices of a twin key (the velocity pass), `None` for any other
    /// mesh key.
    pub(super) fn skin_twin_prev(&self, key: u64) -> Option<wgpu::Buffer> {
        if key >> 56 != crate::skin_compute::SKIN_TWIN_KEY_TAG >> 56 {
            return None;
        }
        self.skin_compute.prev_vertices(key)
    }
}
