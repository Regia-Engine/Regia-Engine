// SPDX-License-Identifier: MPL-2.0
//! Visibility-buffer settings, meshlets, instance sets and their counters (moved out of
//! `frame.rs` unchanged, ADR-1552).

use super::*;

/// How much rigid opaque geometry `meshes` submits (ADR-2071): the whole list, in or out of view,
/// so turning the camera does not flip the path; with or without meshlets, because a chain's
/// coarser levels have none until the path draws them.
pub(super) fn dense_scene_count(meshes: &[FrameMesh<'_>]) -> crate::visbuffer::SceneDensity {
    let mut density = crate::visbuffer::SceneDensity::default();
    for mesh in meshes {
        if mesh.joints.is_empty() && mesh.material.alpha_mode == crate::ALPHA_OPAQUE {
            density.add(mesh.indices.len());
        }
    }
    density
}

impl FrameRenderer {
    /// Overrides the material sampler's anisotropy clamp before the first frame, for path
    /// comparisons on a driver whose anisotropic filter ignores explicit gradients
    /// (`tests/visbuffer_differential.rs`). Not for games.
    #[doc(hidden)]
    pub fn set_material_anisotropy_for_tests(&mut self, clamp: u16) {
        self.material_anisotropy = Some(clamp.max(1));
    }

    /// What the visibility path's selection did ([`crate::VisibilitySelect::Dense`], ADR-2071).
    #[must_use]
    pub fn visbuffer_dense_report(&self) -> crate::visbuffer::DenseGateReport {
        self.visbuffer.dense_report()
    }

    /// What the last visibility frame's resolve did (one draw or one per material, and why,
    /// ADR-2071).
    #[must_use]
    pub fn visbuffer_resolve_report(&self) -> crate::visbuffer::VisResolveReport {
        self.visbuffer.resolve_report()
    }

    /// Game-frame warms of a profile that selects by density (ADR-2071): the path's pipelines for
    /// the HDR targets the warm drew, so the frame the scene turns dense compiles nothing. Asks
    /// for nothing on any other profile.
    pub(crate) fn prewarm_visbuffer_path(&mut self, device: &RenderDevice) -> Result<usize, RenderError> {
        let (on, path) = self.visbuffer.profile_path();
        if !on || !matches!(path.select, crate::visbuffer::VisibilitySelect::Dense(_)) {
            return Ok(0);
        }
        let d = &device.device;
        if !crate::visbuffer::VisBufferPass::supported(d) {
            return Ok(0);
        }
        let Some(pipes) = self.pipes.as_ref() else {
            return Ok(0);
        };
        let mut targets: Vec<(wgpu::TextureFormat, bool, bool)> = pipes
            .sets
            .keys()
            .filter(|key| key.samples == 1 && is_hdr_target(key.format))
            .map(|key| (key.format, key.encode, key.sun_rt))
            .collect();
        targets.sort_by_key(|&(format, encode, sun_rt)| (format!("{format:?}"), encode, sun_rt));
        targets.dedup();
        let (view_bgl, mat_bgl) = (pipes.view_bgl.clone(), pipes.mat_bgl.clone());
        let stride = self.material_arena.as_ref().map(|arena| arena.stats().stride);
        self.visbuffer.warm_path(d, &view_bgl, &mat_bgl, stride, &targets)
    }

    /// Installs the cooker the visibility-buffer path calls once for each new rigid opaque mesh
    /// (`None` removes it). Meshes it declines, and every mesh while no builder or cooked data
    /// exists, draw forward. See [`crate::visbuffer`].
    pub fn set_meshlet_builder(&mut self, builder: Option<crate::visbuffer::MeshletBuilder>) {
        self.visbuffer.set_builder(builder);
    }

    /// Supplies cooked clusters for the mesh drawn with `mesh_key` (used when the
    /// visibility-buffer path is on; validated against the mesh on first use).
    pub fn insert_meshlets(&mut self, mesh_key: u64, mesh: std::sync::Arc<crate::visbuffer::MeshletMesh>) {
        self.visbuffer.insert_cooked(mesh_key, mesh);
    }

    /// Removes the clusters of `mesh_key` ([`Self::insert_meshlets`], or the builder's): the
    /// mesh draws forward from the next frame (unless a builder is installed, which builds them
    /// again), and its cluster-arena ranges are reused by the next mesh, so a streaming map
    /// creates no buffer for them (ADR-353). A persistent instance set that drew the mesh stops
    /// drawing it. False when the key had no meshlets.
    pub fn remove_meshlets(&mut self, mesh_key: u64) -> bool {
        self.visbuffer.remove_meshlets(mesh_key)
    }

    /// True when `mesh_key` has meshlets: inserted and waiting for the mesh's first draw, or
    /// resident.
    #[must_use]
    pub fn has_meshlets(&self, mesh_key: u64) -> bool {
        self.visbuffer.has_meshlets(mesh_key)
    }

    /// Occupancy of the visibility buffer's cluster arena: resident meshes and clusters, bytes
    /// used, free and held, growths and buffer creates since the renderer was made.
    #[must_use]
    pub fn visbuffer_arena_stats(&self) -> crate::visbuffer::VisBufferArenaStats {
        self.visbuffer.arena_stats()
    }

    /// GPU objects (pipelines, bind groups, buffers, textures) and upload bytes of the
    /// visibility-buffer path, in total and in the last frame, and the run of frames that created
    /// none (ADR-355). Zero per frame once warm, streaming included.
    #[must_use]
    pub fn visbuffer_create_stats(&self) -> crate::visbuffer::VisBufferCreateStats {
        self.visbuffer.create_stats()
    }

    /// Visibility-buffer knobs (LOD threshold, cluster capacity, occlusion).
    pub fn set_visbuffer_settings(&mut self, settings: crate::visbuffer::VisBufferSettings) {
        self.visbuffer.settings = settings;
    }

    /// The visibility-buffer knobs in use ([`Self::set_visbuffer_settings`]).
    #[must_use]
    pub fn visbuffer_settings(&self) -> crate::visbuffer::VisBufferSettings {
        self.visbuffer.settings
    }

    /// True when the last frame of the main view took the visibility-buffer path: the profile
    /// had [`crate::RenderProfile::visibility_buffer`] on, the device supports it, the frame was
    /// single-sample linear HDR with a depth prepass, and some mesh had meshlets (whether or not
    /// any instance then drew). Secondary views do not change it.
    #[must_use]
    pub fn visbuffer_active(&self) -> bool {
        self.visbuffer.active()
    }

    /// True when `mesh_key`'s cluster hierarchy picks its level of detail (ADR-1148): the last
    /// main-view frame drew through the visibility buffer ([`Self::visbuffer_active`]) and the
    /// mesh has meshlets ([`Self::has_meshlets`]) that were neither declined (they do not match
    /// the mesh) nor refused at the cluster arena's cap. [`crate::select_mesh_lods`] then leaves
    /// that mesh's [`crate::MeshLodChain`] at LOD 0, so it stays on the path at every distance
    /// instead of drawing its coarser levels forward.
    #[must_use]
    pub fn visbuffer_cluster_lod(&self, mesh_key: u64) -> bool {
        self.visbuffer.takes_cluster_lod(mesh_key)
    }

    /// Test hook: the epoch that joins the static-screen key.
    #[cfg(test)]
    pub(crate) fn material_epoch_for_tests(&self) -> u64 {
        self.material_epoch
    }

    /// Test hook: the visibility pass, to preset state that takes thousands of frames to reach.
    #[cfg(test)]
    pub(crate) fn visbuffer_pass_mut(&mut self) -> &mut crate::visbuffer::VisBufferPass {
        &mut self.visbuffer
    }

    /// Sets [`Self::visbuffer_active`] without a frame (device-less selector tests).
    #[cfg(test)]
    pub(crate) fn force_visbuffer_active(&mut self, active: bool) {
        self.visbuffer.set_active(active);
    }

    /// What the visibility-buffer path did in the last frame that used it.
    #[must_use]
    pub fn visbuffer_stats(&self) -> crate::visbuffer::VisBufferStats {
        self.visbuffer.stats()
    }

    /// Stores (or replaces) a persistent instance set under `set_key`: many instances of one
    /// rigid opaque mesh, uploaded once and drawn every frame through the visibility buffer with
    /// no per-instance CPU work (`docs/specs/27` § 4.2 stage 3). Sets draw only while the path
    /// runs ([`crate::RenderProfile::visibility_buffer`] and a supporting device); games keep
    /// [`FrameMesh`]es for frames without it.
    ///
    /// Returns false in two cases. When the mesh has no clusters (no cooked meshlets and no
    /// builder, or the builder declined), the set under `set_key` is removed. When the models
    /// do not fit the remaining persistent instance cap or the cluster-bit cursor, nothing from
    /// this call is stored and the previous set under `set_key` stays. A set that is accepted
    /// is drawn whole.
    pub fn set_instance_set(&mut self, set_key: u64, set: &crate::visbuffer::VisInstanceSet<'_>) -> bool {
        self.visbuffer.set_instance_set(set_key, set)
    }

    /// Removes the persistent instance set `set_key`. False when there was none.
    pub fn remove_instance_set(&mut self, set_key: u64) -> bool {
        self.visbuffer.remove_instance_set(set_key)
    }

    /// Reads the last frame's shadow-cluster counters back: work items, clusters drawn over all
    /// shadow jobs, clusters dropped at capacity, work items dropped at capacity. Blocks until
    /// the GPU is idle (tests and benches). `None` before the first frame with cluster shadows.
    ///
    /// # Errors
    ///
    /// The readback poll or map failed.
    pub fn visbuffer_shadow_counters(&self, device: &RenderDevice) -> Result<Option<[u32; 4]>, RenderError> {
        self.visbuffer.read_shadow_counters(device)
    }

    /// Reads the last frame's shadow-cluster lists back (test hook): the header counters, every
    /// shadow job's record (its work range and cluster ranges and counts) and the whole
    /// work-item and cluster buffers. Blocks until the GPU is idle (tests). `None` before the
    /// first frame with cluster shadows.
    ///
    /// # Errors
    ///
    /// The readback poll or map failed.
    #[cfg(test)]
    pub(crate) fn visbuffer_shadow_lists(
        &self,
        device: &RenderDevice,
    ) -> Result<Option<crate::visbuffer::gpu::ShadowLists>, RenderError> {
        self.visbuffer.read_shadow_lists(device)
    }

    /// Reads the last visibility frame's GPU cull counters. Blocks until the GPU is idle: a
    /// diagnostic for tests and benches, not for the frame loop. `None` before the first
    /// visibility frame.
    ///
    /// # Errors
    ///
    /// The readback poll or map failed.
    pub fn visbuffer_counters(
        &self,
        device: &RenderDevice,
    ) -> Result<Option<crate::visbuffer::VisBufferCounters>, RenderError> {
        self.visbuffer.read_counters(device)
    }
}

/// The formats a visibility frame's colour can have (`render_into`'s `color_format`).
fn is_hdr_target(format: wgpu::TextureFormat) -> bool {
    matches!(
        format,
        wgpu::TextureFormat::Rgba16Float | wgpu::TextureFormat::Rg11b10Ufloat
    )
}
