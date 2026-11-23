// SPDX-License-Identifier: MPL-2.0
//! Counted asset meshes drawn from their GPU upload (ADR-7284, `docs/specs/12-assets.md` § 3).
//!
//! A [`FrameMesh`] whose key is a [`crate::asset_mesh_key`] and whose `vertices` and `indices` are
//! empty draws the buffers the renderer's [`crate::AssetMeshUploads`] holds for that asset: the
//! mesh cache adopts them as an entry (no copy, no CPU vertices) at the start of the first frame
//! that draws the key, and drops the entry at the start of the first frame after the asset
//! unloaded or was uploaded again (a hot reload). Residency is read once, at the start of the
//! frame: an asset that unloads while a frame records stays drawable (the entry holds its buffer
//! handles) until the next frame. The asset server owns the buffers and counts their bytes; the
//! renderer only borrows them.
//!
//! Such a draw is the whole mesh, or one level of its [`crate::MeshLodChain`] (ADR-7428): a LOD,
//! shadow-LOD or cross-fade variant key of a counted mesh is an entry that shares the adopted
//! vertex buffer and binds the level's own index list, uploaded once per level (LOD 0 binds the
//! upload's indices). Morph targets, the compute-skinning twin and the GI proxy brick draw from the
//! upload too (ADR-7721): a morphed counted draw deforms a GPU copy of the adopted vertex buffer
//! (`FrameRenderer::ensure_counted_morph_twin`) with the targets the upload packed
//! ([`crate::AssetMeshUploads::morph_targets`]), the compute twin skins from the adopted buffer, and
//! the probes bake its brick from a GPU readback of the upload (`CountedGiSource`). The camera-cut
//! content check, which reads CPU vertices, reads it as empty, as it does for any mesh it cannot
//! read. On the visibility-buffer profile a counted
//! whole draw clusters its upload (ADR-7462, `FrameRenderer::ensure_vis_clusters`): cooked
//! meshlets under its key, its vertices repacked into the cluster arena on the GPU. A counted mesh with a skin is uploaded with its four
//! strongest influences per vertex, and the skin checks that read CPU vertices (the joint reach
//! against the palette, the per-joint bind boxes, the one-joint fold) read [`AssetMeshSkin`],
//! measured once at the upload: drawn with joints it skins on the vertex-shader path from the
//! upload (the joints stay CPU), drawn without, it reads identities and stands at its rest pose
//! (ADR-8610). A second UV set the draw brings with one entry per uploaded vertex
//! (a lightmap receiver's UV2, a `MeshUv1`) becomes the entry's `uv1` buffer, so lightmap
//! receivers and material graphs that read it draw the counted mesh (ADR-7330); without one, a
//! graph that reads it does not draw it and a receiver keeps the probe light.

use super::*;

/// What the frame needs of a skinned counted mesh once its CPU vertices are gone (ADR-8610),
/// measured at the upload from the vertices it uploads.
#[derive(Debug)]
pub(crate) struct AssetMeshSkin {
    /// The largest joint a weighted lane reads (`None`: no vertex is skinned).
    pub(super) reach: Option<u32>,
    /// Per-joint bind boxes (`None`: the weights allow no conservative bound).
    pub(super) bind: Option<std::sync::Arc<super::skin_bounds::SkinBindBounds>>,
    /// Every vertex follows joint 0 alone (a one-joint skin folds to a rigid draw).
    pub(super) rigid_to_joint_zero: bool,
    /// The rest-pose bound, for a rigid or folded draw.
    pub(super) rest_bounds: Option<MeshBounds>,
}

/// [`AssetMeshSkin`] of skinned `vertices` (the counted upload's).
pub(crate) fn asset_mesh_skin(vertices: &[LabVertex]) -> AssetMeshSkin {
    AssetMeshSkin {
        reach: super::skin_bounds::weighted_joint_reach(vertices),
        bind: super::skin_bounds::SkinBindBounds::from_vertices(vertices).map(std::sync::Arc::new),
        rigid_to_joint_zero: super::mesh_cache::rigid_to_joint_zero(vertices),
        rest_bounds: mesh_bounds(vertices),
    }
}

impl FrameRenderer {
    /// Gives the renderer the uploader of counted meshes it draws from (`None`: none). The
    /// mesh-cache entries it adopted from an earlier uploader are dropped.
    pub fn set_asset_meshes(&mut self, uploads: Option<crate::AssetMeshUploads>) {
        self.asset_mesh_skins.clear();
        let keys: Vec<u64> = self.asset_mesh_keys.drain().collect();
        self.release_meshes(&keys);
        // The probes bake a counted mesh's brick from a readback of its upload (ADR-7721).
        self.ddgi
            .set_counted(uploads.clone().map(crate::asset_upload::CountedGiSource::new));
        self.asset_meshes = uploads;
    }

    /// The uploader set with [`Self::set_asset_meshes`].
    #[must_use]
    pub fn asset_meshes(&self) -> Option<&crate::AssetMeshUploads> {
        self.asset_meshes.as_ref()
    }

    /// Counted meshes whose buffers the mesh cache holds now.
    #[must_use]
    pub fn adopted_asset_mesh_count(&self) -> usize {
        self.asset_mesh_keys.len()
    }

    /// Of [`Self::adopted_asset_mesh_count`], the counted meshes uploaded skinned (drawn with
    /// joints from their upload, ADR-8610).
    #[must_use]
    pub fn adopted_skinned_asset_mesh_count(&self) -> usize {
        self.asset_mesh_skins.len()
    }

    /// Material entries rebuilt so far because a counted texture they bind was uploaded again (a
    /// hot reload, or the first upload after a load) while the material kept its key (ADR-8610).
    #[must_use]
    pub fn counted_material_rebinds(&self) -> u64 {
        self.counted_rebinds
    }

    /// Whether `mesh` has geometry to draw this frame: CPU vertices and indices, or (no CPU
    /// vertices) a counted mesh the mesh cache adopted at the start of the frame
    /// ([`Self::sweep_asset_meshes`]): drawn whole (rigid, or with joints when its upload is
    /// skinned, ADR-8610), or as a LOD, shadow-LOD or cross-fade variant of a [`crate::MeshLodChain`]
    /// (rigid, ADR-7428).
    pub(super) fn has_geometry(&self, mesh: &FrameMesh<'_>) -> bool {
        if !mesh.vertices.is_empty() && !mesh.indices.is_empty() {
            return true;
        }
        let Some((base, _)) = Self::counted_draw(mesh) else {
            return false;
        };
        let whole = mesh.mesh_key == base && mesh.lod_dither == LOD_DITHER_NONE;
        (mesh.joints.is_empty() || (whole && self.asset_mesh_skins.contains_key(&base)))
            // A graph that reads the second UV stream draws when the draw brings one for every
            // uploaded vertex (ADR-7330).
            && (!mesh.material.surface_graph.as_ref().is_some_and(|graph| graph.uses_uv1)
                || self.asset_uv1_fits(mesh))
            && self.asset_mesh_keys.contains(&base)
    }

    /// Vertices of the adopted counted mesh `key` (0 when it is not adopted).
    pub(super) fn asset_vertex_count(&self, key: u64) -> usize {
        if !self.asset_mesh_keys.contains(&key) {
            return 0;
        }
        self.meshes
            .get(&key)
            .map_or(0, |entry| entry.vertex_bytes / std::mem::size_of::<LabVertex>())
    }

    /// Whether counted draw `mesh` brings a second UV set with one entry per uploaded vertex
    /// ([`FrameMesh::uv1`]: a lightmap receiver's UV2, a [`crate::MeshUv1`]).
    pub(super) fn asset_uv1_fits(&self, mesh: &FrameMesh<'_>) -> bool {
        let base = Self::counted_draw(mesh).map_or(mesh.mesh_key, |(base, _)| base);
        !mesh.uv1.is_empty() && mesh.uv1.len() == self.asset_vertex_count(base)
    }

    /// Builds the `uv1` buffer of adopted counted mesh `mesh` from the draw's second UV set
    /// (ADR-7330); returns whether the entry now holds that set. The buffer is per mesh key, as
    /// on the CPU path: a second draw of the same counted mesh with another set in the same frame
    /// is refused (false) instead of rewriting a buffer an earlier draw binds; in a later frame
    /// the new set replaces it.
    pub(super) fn ensure_asset_uv1(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        mesh: &FrameMesh<'_>,
    ) -> bool {
        if !self.asset_uv1_fits(mesh) {
            return false;
        }
        let source = mesh.uv1.as_ptr() as usize;
        let now = self.frame_index;
        let has_buffer = self
            .meshes
            .get(&mesh.mesh_key)
            .is_some_and(|entry| entry.uv1.is_some());
        if has_buffer {
            match self.asset_uv1_sources.get(&mesh.mesh_key) {
                Some(&(built, _)) if built == source => return true,
                Some(&(_, frame)) if frame == now => return false,
                _ => {}
            }
        }
        let Some(entry) = self.meshes.get_mut(&mesh.mesh_key) else {
            return false;
        };
        let bytes: &[u8] = bytemuck::cast_slice(mesh.uv1);
        let buf = make_buffer(
            d,
            "asset mesh uv1",
            bytes.len() as u64,
            wgpu::BufferUsages::VERTEX,
        );
        q.write_buffer(&buf, 0, bytes);
        entry.uv1 = Some(buf);
        self.asset_uv1_sources.insert(mesh.mesh_key, (source, now));
        true
    }

    /// Whether `mesh` draws a counted mesh from its upload (no CPU vertices), whole or as a
    /// LOD, shadow-LOD or cross-fade variant (ADR-7428).
    pub(super) fn is_asset_draw(mesh: &FrameMesh<'_>) -> bool {
        Self::counted_draw(mesh).is_some()
    }

    /// The counted mesh key `mesh` draws from and its LOD index (`None`: not a counted draw).
    pub(super) fn counted_draw(mesh: &FrameMesh<'_>) -> Option<(u64, usize)> {
        if !mesh.vertices.is_empty() {
            return None;
        }
        crate::asset_upload::counted_base_key(mesh.mesh_key)
    }

    /// Makes `mesh`'s clusters resident on a visibility-buffer frame: from its CPU vertices, or,
    /// for a counted whole draw (LOD 0, no dither), from the adopted upload's vertex buffer
    /// (ADR-7462). False keeps the draw forward. A counted LOD, shadow-LOD or cross-fade variant
    /// has no clusters of its own, as a CPU chain's variant keys without meshlets do not.
    pub(super) fn ensure_vis_clusters(&mut self, mesh: &FrameMesh<'_>) -> bool {
        if !mesh.vertices.is_empty() {
            return self
                .visbuffer
                .ensure_mesh(mesh.mesh_key, mesh.vertices, mesh.indices);
        }
        let Some((base, _)) = Self::counted_draw(mesh) else {
            return false;
        };
        if base != mesh.mesh_key || mesh.lod_dither != LOD_DITHER_NONE {
            return false;
        }
        let entry = self
            .meshes
            .get(&base)
            .filter(|_| self.asset_mesh_keys.contains(&base));
        let Some(entry) = entry else {
            self.visbuffer.note_counted_fallback();
            return false;
        };
        let vertex_count = entry.vertex_bytes / std::mem::size_of::<LabVertex>();
        self.visbuffer
            .ensure_counted_mesh(base, &entry.vb, vertex_count, entry.index_count as usize)
    }

    /// Counted meshes on the visibility buffer's cluster path (ADR-7462): resident from their
    /// upload, the vertex repacks run, and the counted whole draws of the last visibility-buffer
    /// frame that drew forward for want of clusters.
    #[must_use]
    pub fn counted_vis_stats(&self) -> crate::visbuffer::CountedVisStats {
        self.visbuffer.counted_stats()
    }

    /// The uploaded vertex and index counts of the resident counted mesh `key`
    /// ([`crate::asset_mesh_key`]), `None` when it is not resident or no uploader is set: what
    /// cooked meshlets for it must match (`MeshletMesh::base_vertex_count`,
    /// `MeshletMesh::source_index_count`; ADR-7462).
    #[must_use]
    pub fn asset_mesh_counts(&self, key: u64) -> Option<(usize, usize)> {
        let resident = self.asset_meshes.as_ref()?.resident_by_key(key)?;
        Some((
            resident.layout.vertex_count,
            resident.buffers.index_count as usize,
        ))
    }

    /// The upload serial of the resident counted mesh `key` ([`crate::asset_mesh_key`]): it
    /// changes with every upload of the mesh (a hot reload uploads again). `None` when it is not
    /// resident or no uploader is set. The meshlet bridge inserts a counted mesh's cooked meshlets
    /// again when it changes, so they are checked against the new upload's counts (ADR-7542).
    #[must_use]
    pub fn asset_mesh_upload_serial(&self, key: u64) -> Option<u64> {
        self.asset_meshes.as_ref()?.serial_by_key(key)
    }

    /// LOD index lists uploaded for counted meshes' LOD variants so far (ADR-7428): one per LOD
    /// a chain draws, shared by its colour, shadow and cross-fade draws, again only after the
    /// entry was evicted or the mesh reloaded.
    #[must_use]
    pub fn counted_lod_index_uploads(&self) -> u64 {
        self.counted_lod_uploads
    }

    /// The entry of counted draw `mesh` under a LOD, shadow-LOD or cross-fade variant of adopted
    /// key `base` (ADR-7428): it shares the adopted vertex buffer and binds the draw's index list,
    /// uploaded once per LOD (its colour, shadow and faded variants share it; LOD 0 shares the
    /// upload's own indices). Returns its index count: 0 when `base` is not adopted, or when the
    /// list is not whole triangles inside the uploaded vertices (nothing is drawn).
    pub(super) fn ensure_counted_variant(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        mesh: &FrameMesh<'_>,
        (base, lod): (u64, usize),
        now: u64,
    ) -> u32 {
        if let Some(entry) = self.meshes.get_mut(&mesh.mesh_key) {
            entry.last_used = now;
            return entry.index_count;
        }
        let Some(source) = self.meshes.get(&base) else {
            return 0;
        };
        let vertex_count = source.vertex_bytes / std::mem::size_of::<LabVertex>();
        let sibling = if lod == 0 || mesh.indices.is_empty() {
            Some((source.ib.clone(), source.index_count))
        } else {
            let colour = crate::mesh_lod::lod_variant_key(base, crate::mesh_lod_cache_key(lod, false));
            let shadow = crate::mesh_lod::lod_variant_key(base, crate::mesh_lod_cache_key(lod, true));
            [colour, colour ^ LOD_DITHER_MESH_TAG, shadow]
                .iter()
                .filter_map(|key| self.meshes.get(key))
                .find(|entry| entry.index_count as usize == mesh.indices.len())
                .map(|entry| (entry.ib.clone(), entry.index_count))
        };
        let (ib, index_count) = match sibling {
            Some(shared) => shared,
            None => {
                let whole = mesh.indices.len().is_multiple_of(3)
                    && mesh.indices.iter().all(|&i| (i as usize) < vertex_count);
                let Ok(index_count) = u32::try_from(mesh.indices.len()) else {
                    return 0;
                };
                if !whole {
                    tracing::warn!(
                        target: "regia_render",
                        key = mesh.mesh_key,
                        "a counted LOD index list is not whole triangles inside its upload; not drawn"
                    );
                    return 0;
                }
                let bytes: &[u8] = bytemuck::cast_slice(mesh.indices);
                let _span = tracing::info_span!("counted_lod_upload", bytes = bytes.len()).entered();
                let ib = make_buffer(
                    d,
                    "counted lod indices",
                    bytes.len() as u64,
                    wgpu::BufferUsages::INDEX | crate::mesh_storage::blas_input_usage(d.features()),
                );
                q.write_buffer(&ib, 0, bytes);
                crate::frame_stats::note_mesh_upload(bytes.len() as u64, 1);
                self.counted_lod_uploads += 1;
                (ib, index_count)
            }
        };
        let Some(source) = self.meshes.get(&base) else {
            return 0;
        };
        self.mesh_generation += 1;
        let entry = MeshEntry {
            vb: source.vb.clone(),
            index_bytes: ib.size() as usize,
            ib,
            base_vertex: 0,
            first_index: 0,
            leases: [None, None],
            uv1: None,
            index_count,
            // The shared vertex buffer's size: what a structure built from this entry reads.
            vertex_bytes: source.vertex_bytes,
            bounds: source.bounds,
            static_bound: source.static_bound,
            uv_span: source.uv_span,
            last_used: now,
            dithered: mesh.lod_dither != LOD_DITHER_NONE,
            generation: self.mesh_generation,
            content_rev: source.content_rev,
        };
        self.meshes.insert(mesh.mesh_key, entry);
        index_count
    }

    /// The entry of a counted draw's deformed copy `mesh` (morph targets, ADR-7721): its own
    /// vertex buffer, a GPU copy of the adopted upload (recorded by the morph pre-pass before the
    /// deformed span is copied in), with the index list of the draw's entry `source` (the whole
    /// mesh, or a LOD or cross-fade variant). Made again when the source was uploaded again or the
    /// copy was evicted. Returns its index count (0: the source has nothing to draw).
    pub(super) fn ensure_counted_morph_twin(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        mesh: &FrameMesh<'_>,
        source: u64,
        now: u64,
        belt: bool,
    ) -> Result<u32, RenderError> {
        let source_mesh = FrameMesh {
            mesh_key: source,
            ..*mesh
        };
        let index_count = self.ensure_mesh(d, q, &source_mesh, now, belt)?;
        let Some((src_rev, src_ib)) = self
            .meshes
            .get(&source)
            .map(|entry| (entry.content_rev, entry.ib.clone()))
        else {
            return Ok(0);
        };
        // The copy of this upload with this index list: touched, not made again.
        if let Some(entry) = self.meshes.get_mut(&mesh.mesh_key)
            && entry.content_rev == src_rev
            && entry.ib == src_ib
        {
            entry.last_used = now;
            return Ok(index_count);
        }
        let Some(src) = self.meshes.get(&source) else {
            return Ok(0);
        };
        let bytes = src.vertex_bytes as u64;
        let vb = make_buffer(
            d,
            "counted morph copy",
            bytes.max(std::mem::size_of::<LabVertex>() as u64),
            wgpu::BufferUsages::VERTEX
                | wgpu::BufferUsages::COPY_SRC
                | crate::mesh_storage::blas_input_usage(d.features()),
        );
        let src_vb = src.vb.clone();
        let entry = MeshEntry {
            vb: vb.clone(),
            ib: src.ib.clone(),
            base_vertex: 0,
            first_index: src.first_index,
            leases: [None, None],
            uv1: None,
            index_count: src.index_count,
            // Its own vertex buffer; the index list is the source entry's (counted there).
            vertex_bytes: src.vertex_bytes,
            index_bytes: 0,
            bounds: src.bounds,
            static_bound: src.static_bound,
            uv_span: src.uv_span,
            last_used: now,
            dithered: src.dithered,
            generation: 0,
            content_rev: src.content_rev,
        };
        self.morph.seed(&src_vb, 0, &vb, 0, bytes);
        self.mesh_generation += 1;
        self.meshes.insert(
            mesh.mesh_key,
            MeshEntry {
                generation: self.mesh_generation,
                ..entry
            },
        );
        Ok(index_count)
    }

    /// The index count of counted mesh `key`: its adopted entry, or its uploaded buffers adopted
    /// into the mesh cache now (0 when it is not resident).
    pub(super) fn ensure_asset_mesh(&mut self, key: u64, now: u64) -> u32 {
        if let Some(entry) = self.meshes.get_mut(&key) {
            entry.last_used = now;
            return entry.index_count;
        }
        let Some(resident) = self
            .asset_meshes
            .as_ref()
            .and_then(|uploads| uploads.resident_by_key(key))
        else {
            return 0;
        };
        self.mesh_generation += 1;
        let index_count = resident.buffers.index_count;
        let vertex_bytes = resident.buffers.vertex.size() as usize;
        let index_bytes = resident.buffers.index.size() as usize;
        self.meshes.insert(
            key,
            MeshEntry {
                vb: resident.buffers.vertex,
                ib: resident.buffers.index,
                base_vertex: 0,
                first_index: 0,
                leases: [None, None],
                uv1: None,
                index_count,
                vertex_bytes,
                index_bytes,
                bounds: resident.bounds,
                static_bound: true,
                uv_span: resident.uv_span,
                last_used: now,
                dithered: false,
                generation: self.mesh_generation,
                // The upload serial: a re-upload (hot reload) is a new entry.
                content_rev: resident.serial,
            },
        );
        self.asset_mesh_keys.insert(key);
        if let Some(skin) = resident.skin {
            self.asset_mesh_skins.insert(key, skin);
        }
        index_count
    }

    /// The palette length a rigid draw (no joints) of a skinned counted upload needs at the
    /// palette's start: one past the farthest joint such a draw's vertices read (0: none this
    /// frame). Those slots hold identities, so the draw stands at its rest pose (ADR-8610).
    pub(super) fn counted_rest_palette(&self, scene: &FrameScene<'_>) -> usize {
        if self.asset_mesh_skins.is_empty() {
            return 0;
        }
        scene
            .meshes
            .iter()
            .filter(|mesh| mesh.joints.is_empty() && Self::is_asset_draw(mesh) && self.has_geometry(mesh))
            .filter_map(|mesh| {
                let (base, _) = Self::counted_draw(mesh)?;
                self.asset_mesh_skins.get(&base)?.reach
            })
            .map(|reach| reach as usize + 1)
            .max()
            .unwrap_or(0)
            .min(MAX_JOINTS_PER_SKIN + 1)
    }

    /// Start of a frame: drops the mesh-cache entries of counted meshes that unloaded or were
    /// uploaded again (their old buffers are freed with the entry), then adopts every counted mesh
    /// `scene` draws that the uploader holds. Allocates nothing while every adopted mesh is still
    /// current.
    pub(super) fn sweep_asset_meshes(&mut self, scene: &FrameScene<'_>) {
        self.drop_stale_asset_meshes();
        if self.asset_meshes.is_none() {
            return;
        }
        let now = self.frame_index;
        for mesh in &scene.meshes {
            if let Some((base, _)) = Self::counted_draw(mesh)
                && !self.asset_mesh_keys.contains(&base)
            {
                self.ensure_asset_mesh(base, now);
            }
        }
    }

    /// Drops the mesh-cache entries of counted meshes that unloaded or were uploaded again, so
    /// their buffers are freed now rather than at the next frame that draws a mesh (a frame with
    /// nothing to draw does not run). [`crate::CountedMeshPlugin`] calls it every frame after the
    /// asset server's update; allocates nothing while every adopted mesh is current.
    pub fn release_unloaded_asset_meshes(&mut self) {
        self.drop_stale_asset_meshes();
    }

    fn drop_stale_asset_meshes(&mut self) {
        if self.asset_mesh_keys.is_empty() {
            return;
        }
        let mut gone = std::mem::take(&mut self.asset_mesh_gone);
        gone.clear();
        for &key in &self.asset_mesh_keys {
            let current = self
                .asset_meshes
                .as_ref()
                .and_then(|uploads| uploads.serial_by_key(key));
            let cached = self.meshes.get(&key).map(|entry| entry.content_rev);
            if cached.is_none() || current != cached {
                gone.push(key);
            }
        }
        if !gone.is_empty() {
            for key in &gone {
                self.asset_mesh_keys.remove(key);
                self.asset_uv1_sources.remove(key);
                self.asset_mesh_skins.remove(key);
            }
            // A counted mesh uploaded again (a hot reload) keeps cooked clusters: the fresher of
            // the ones inserted since it was admitted (the meshlet bridge's for a `.fmlt` that
            // reloaded in the same update, pending) and the ones it drew with. The next draw checks
            // them against the new upload's counts (ADR-7462, ADR-7542).
            let reloaded: Vec<(u64, std::sync::Arc<crate::visbuffer::MeshletMesh>)> = gone
                .iter()
                .filter(|&&key| {
                    self.asset_meshes
                        .as_ref()
                        .is_some_and(|uploads| uploads.is_resident_key(key))
                })
                .filter_map(|&key| self.visbuffer.take_reload_meshlets(key).map(|mesh| (key, mesh)))
                .collect();
            self.release_meshes(&gone);
            for (key, meshlets) in reloaded {
                self.visbuffer.insert_cooked(key, meshlets);
            }
        }
        self.asset_mesh_gone = gone;
    }
}
