// SPDX-License-Identifier: MPL-2.0
//! Mesh and object uploads, skin palettes, the rigid-skin fold and cache budgets with eviction
//! (moved out of `frame.rs` unchanged, ADR-1552).

use super::*;

/// The derived cache budget ([`CacheResidency::budget_bytes`] `0`): the texture streaming
/// resident budget (`u64::MAX`: none, [`CACHE_FALLBACK_BUDGET`]) plus [`CACHE_MESH_ALLOWANCE`],
/// at most half the adapter's device-local VRAM when known. On `android` the smaller fallback
/// and allowance apply, capped at [`ANDROID_CACHE_CAP`].
pub(super) fn derived_cache_budget(texture_budget: u64, vram_mib: Option<u64>, android: bool) -> u64 {
    let (fallback, allowance, cap) = if android {
        (
            ANDROID_CACHE_FALLBACK_BUDGET,
            ANDROID_CACHE_MESH_ALLOWANCE,
            ANDROID_CACHE_CAP,
        )
    } else {
        (CACHE_FALLBACK_BUDGET, CACHE_MESH_ALLOWANCE, u64::MAX)
    };
    let base = if texture_budget == u64::MAX {
        fallback
    } else {
        texture_budget
    };
    let vram_cap = vram_mib.map_or(u64::MAX, |mib| (mib << 20) >> CACHE_VRAM_SHARE_SHIFT);
    base.saturating_add(allowance).min(vram_cap).min(cap)
}

/// The derived `(mesh, material)` cache budgets ([`CacheResidency::budget_bytes`] `0`). The
/// material budget is [`derived_cache_budget`] (the streaming budget plus the allowance for
/// whole uploads, at most half the VRAM); the mesh budget is its own share of the VRAM
/// ([`CACHE_MESH_VRAM_DIVISOR`], [`CACHE_MESH_ALLOWANCE`] when unknown), lowered so both stay
/// within [`CACHE_TOTAL_VRAM_EIGHTHS`] of it. On `android` the mesh budget is
/// [`ANDROID_CACHE_MESH_ALLOWANCE`] and both stay within [`ANDROID_CACHE_CAP`].
pub(super) fn derived_cache_budgets(texture_budget: u64, vram_mib: Option<u64>, android: bool) -> (u64, u64) {
    let material = derived_cache_budget(texture_budget, vram_mib, android);
    if android {
        let mesh = ANDROID_CACHE_MESH_ALLOWANCE;
        return (mesh, material.min(ANDROID_CACHE_CAP - mesh));
    }
    let Some(vram) = vram_mib.map(|mib| mib << 20) else {
        return (CACHE_MESH_ALLOWANCE, material);
    };
    let mesh = (vram / CACHE_MESH_VRAM_DIVISOR).clamp(CACHE_MESH_ALLOWANCE, CACHE_MESH_MAX);
    let room = (vram / 8 * CACHE_TOTAL_VRAM_EIGHTHS).saturating_sub(material);
    (mesh.min(room).max(CACHE_MESH_FLOOR), material)
}

/// Whether every vertex is skinned to joint 0 alone: weights summing to one (the shaders skip
/// skinning under `1e-5` and do not normalise), every weighted lane on joint 0. A one-joint
/// skin like that is `model × joint` applied to the rest pose, so it can draw rigid.
pub(super) fn rigid_to_joint_zero(vertices: &[LabVertex]) -> bool {
    vertices.iter().all(|v| {
        let w = v.weights;
        // The shaders' sum order.
        let sum = w[0] + w[1] + w[2] + w[3];
        (sum - 1.0).abs() <= 1e-6 && (0..4).all(|lane| w[lane] == 0.0 || v.joints[lane] == 0)
    })
}

/// `model × joint` as a column-major matrix (the joint's 3×4 rows widened with `0 0 0 1`).
pub(super) fn fold_joint(model: [[f32; 4]; 4], joint: &JointMatrix) -> [[f32; 4]; 4] {
    let m = joint.m;
    let joint = regia_core::glam::Mat4::from_cols_array_2d(&[
        [m[0][0], m[1][0], m[2][0], 0.0],
        [m[0][1], m[1][1], m[2][1], 0.0],
        [m[0][2], m[1][2], m[2][2], 0.0],
        [m[0][3], m[1][3], m[2][3], 1.0],
    ]);
    (regia_core::glam::Mat4::from_cols_array_2d(&model) * joint).to_cols_array_2d()
}

/// GPU bytes of a cached mesh's vertex, index and second-UV buffers.
pub(super) fn mesh_entry_bytes(entry: &MeshEntry) -> u64 {
    (entry.vertex_bytes + entry.index_bytes) as u64 + entry.uv1.as_ref().map_or(0, wgpu::Buffer::size)
}

/// The bytes mesh-cache entry `key` adds to the cache's own (`mesh_bytes`). A LOD, shadow-LOD or
/// cross-fade variant of an adopted counted mesh (ADR-7428) shares the upload's vertex buffer, which
/// the asset server owns: it adds only the index list it uploaded, once per list (none when it binds
/// the upload's own list or the list an earlier variant of its LOD holds: colour, then faded, then
/// shadow). Its `vertex_bytes` stays the shared buffer's size, which structures built from the entry
/// read (rev-claude-56 counted-meshes finding 1).
fn cache_entry_bytes(
    meshes: &crate::key_hash::KeyMap<u64, MeshEntry>,
    adopted: &std::collections::HashSet<u64>,
    key: u64,
    entry: &MeshEntry,
) -> u64 {
    let Some((base, lod)) = crate::asset_upload::counted_base_key(key)
        .filter(|&(base, _)| base != key && adopted.contains(&base))
    else {
        return mesh_entry_bytes(entry);
    };
    let uv1 = entry.uv1.as_ref().map_or(0, wgpu::Buffer::size);
    let colour = crate::mesh_lod::lod_variant_key(base, crate::mesh_lod::mesh_lod_cache_key(lod, false));
    let shadow = crate::mesh_lod::lod_variant_key(base, crate::mesh_lod::mesh_lod_cache_key(lod, true));
    let owner = [base, colour, colour ^ LOD_DITHER_MESH_TAG, shadow]
        .into_iter()
        .find(|k| meshes.get(k).is_some_and(|other| other.ib == entry.ib));
    if owner.is_some_and(|owner| owner != key) {
        uv1
    } else {
        entry.index_bytes as u64 + uv1
    }
}

/// GPU bytes of a cached material's textures (the streamed sets' resident and pending levels,
/// and the textures uploaded whole).
pub(super) fn material_entry_bytes(entry: &MaterialEntry) -> u64 {
    let streamed = entry
        .stream
        .as_ref()
        .map_or(0, |stream| stream.resident_bytes() + stream.pending_bytes());
    let fixed: u64 = entry
        .fixed
        .iter()
        .map(|(_, texture)| {
            let size = texture.size();
            crate::texture_budget::texture_bytes(
                texture.format(),
                size.width,
                size.height,
                texture.mip_level_count(),
            )
        })
        .sum();
    streamed + fixed
}

impl FrameRenderer {
    /// Sets the per-frame load budget ([`crate::load_budget`], ADR-7884): with
    /// [`crate::LateDraw::Defer`], meshes past the upload cap are uploaded on the following frames
    /// (nearest first) and material-graph sets past the pipeline cap build on worker threads; the
    /// draws that need them are skipped until they are ready. The default
    /// ([`crate::FrameLoadBudget::default`]) changes nothing. Applies from the next frame; not
    /// persisted.
    pub fn set_load_budget(&mut self, budget: crate::FrameLoadBudget) {
        self.load_budget.budget = budget;
        self.load_budget.explicit = true;
    }

    /// The per-frame load budget ([`Self::set_load_budget`]).
    #[must_use]
    pub fn load_budget(&self) -> crate::FrameLoadBudget {
        self.load_budget.budget
    }

    /// What the load budget did on the last [`Self::render_into`]: deferred meshes, bytes and
    /// draws, and the pipeline builds waiting off the frame thread (read now).
    #[must_use]
    pub fn load_budget_report(&self) -> crate::LoadBudgetReport {
        let pending = self.graph_cache.stats().pending + crate::pipeline_warm::deferred_pending();
        crate::LoadBudgetReport {
            async_pipelines_pending: u32::try_from(pending).unwrap_or(u32::MAX),
            ..self.load_budget.report
        }
    }

    /// Whether `mesh` (under its draw-list key) would be uploaded by [`Self::ensure_mesh`]: no
    /// entry, or one of another size, and not a dithered draw sharing its resident whole mesh.
    fn mesh_needs_upload(&self, mesh: &FrameMesh<'_>) -> bool {
        let vertex_bytes = std::mem::size_of_val(mesh.vertices);
        let index_bytes = std::mem::size_of_val(mesh.indices);
        let sized =
            |entry: &MeshEntry| entry.vertex_bytes == vertex_bytes && entry.index_bytes == index_bytes;
        if self.meshes.get(&mesh.mesh_key).is_some_and(sized) {
            return false;
        }
        let shares = mesh.lod_dither != LOD_DITHER_NONE
            && self
                .meshes
                .get(&(mesh.mesh_key ^ LOD_DITHER_MESH_TAG))
                .is_some_and(sized);
        !shares
    }

    /// The load budget's upload plan for this frame ([`crate::load_budget::admit`]): the meshes
    /// `scene` references that the cache does not hold, nearest first, admitted up to the upload
    /// cap. A warm job or a blocking scope is never budgeted. Called once at the start of the
    /// draw list.
    pub(super) fn plan_mesh_uploads(&mut self, scene: &FrameScene<'_>) {
        let budgeted = !crate::pipeline_warm::in_warm_or_blocking_scope();
        self.load_budget
            .begin_frame(budgeted, crate::frame_stats::thread_pipeline_creates());
        let Some(cap) = self.load_budget.budget.upload_cap().filter(|_| budgeted) else {
            return;
        };
        let _span = tracing::info_span!("load_budget_plan").entered();
        let mut pending = std::mem::take(&mut self.load_budget.pending);
        let mut seen = std::mem::take(&mut self.load_budget.seen);
        pending.clear();
        seen.clear();
        let eye = scene.eye;
        for (order, submitted) in scene.meshes.iter().enumerate() {
            // Ephemeral meshes upload every frame and are never deferred; empty ones draw nothing.
            if submitted.mesh_key == 0 || submitted.vertices.is_empty() || submitted.indices.is_empty() {
                continue;
            }
            let key = if submitted.lod_dither == LOD_DITHER_NONE {
                submitted.mesh_key
            } else {
                submitted.mesh_key ^ LOD_DITHER_MESH_TAG
            };
            let keyed = FrameMesh {
                mesh_key: key,
                ..*submitted
            };
            if !self.mesh_needs_upload(&keyed) || seen.insert(key, ()).is_some() {
                continue;
            }
            let origin = [
                submitted.model[3][0],
                submitted.model[3][1],
                submitted.model[3][2],
            ];
            let distance_sq = (0..3)
                .map(|i| (origin[i] - eye[i]) * (origin[i] - eye[i]))
                .sum::<f32>();
            pending.push(crate::load_budget::PendingUpload {
                key,
                bytes: (std::mem::size_of_val(submitted.vertices) + std::mem::size_of_val(submitted.indices))
                    as u64,
                // A NaN origin sorts last.
                distance_sq: if distance_sq.is_nan() {
                    f32::INFINITY
                } else {
                    distance_sq
                },
                order: u32::try_from(order).unwrap_or(u32::MAX),
            });
        }
        let (admitted, admitted_bytes, deferred, deferred_bytes) =
            crate::load_budget::admit(&mut pending, cap, &mut self.load_budget.admitted);
        let report = &mut self.load_budget.report;
        report.admitted_meshes = admitted;
        report.admitted_bytes = admitted_bytes;
        report.deferred_meshes = deferred;
        report.deferred_bytes = deferred_bytes;
        self.load_budget.gate_uploads = deferred > 0;
        self.load_budget.pending = pending;
        self.load_budget.seen = seen;
    }

    /// Whether the draw list skips `mesh` (its draw-list key, before any morph copy) this frame
    /// because the upload plan deferred it ([`Self::plan_mesh_uploads`]). Counts the skip.
    pub(super) fn defer_mesh_draw(&mut self, mesh: &FrameMesh<'_>) -> bool {
        if !self.load_budget.gate_uploads
            || self.load_budget.admitted.contains_key(&mesh.mesh_key)
            || !self.mesh_needs_upload(mesh)
        {
            return false;
        }
        self.load_budget.report.deferred_draws += 1;
        true
    }

    /// Drops the cache entries of meshes that will never be drawn again, now instead of at
    /// the idle eviction: pass [`crate::ExtractedFrame::released_mesh_keys`] each frame (keys
    /// from [`crate::mesh_key`] whose last `Arc` dropped). Every LOD, shadow-LOD and
    /// cross-fade variant of a key goes with it; its vertex and index ranges return to the mesh
    /// arenas for the next upload (no buffer is created for it). Pinned entries
    /// ([`Self::prewarm`]) are unpinned. The DDGI brick pool and the visibility buffer forget the
    /// keys too (generation keys never come back, so their per-key memos would otherwise stay).
    /// Keys this renderer never cached are ignored.
    pub fn release_meshes(&mut self, keys: &[u64]) {
        if keys.is_empty() {
            return;
        }
        let mut released = 0u64;
        let mut variants = Vec::with_capacity(keys.len() * 16);
        for &key in keys {
            for lod in 0..4usize {
                for shadow_only in [false, true] {
                    let variant = crate::mesh_lod::lod_variant_key(
                        key,
                        crate::mesh_lod::mesh_lod_cache_key(lod, shadow_only),
                    );
                    variants.extend([variant, variant ^ LOD_DITHER_MESH_TAG]);
                }
            }
        }
        for variant in &variants {
            if self.meshes.remove(variant).is_some() {
                released += 1;
            }
            self.rigid_skin.remove(variant);
            self.pinned.remove(variant);
            // A counted chain level's second UV set (ADR-7428) goes with its entry.
            if !self.asset_uv1_sources.is_empty() {
                self.asset_uv1_sources.remove(variant);
            }
        }
        self.ddgi.release_meshes(&variants);
        self.visbuffer.release_meshes(&variants);
        self.release_skin_twins(&variants);
        self.skin_bounds.forget(&variants);
        self.cache_stats.meshes_released += released;
        if let Some(arenas) = self.mesh_arenas.as_mut() {
            arenas.vertices.reclaim();
            arenas.indices.reclaim();
        }
    }

    pub(super) fn write_skin(&mut self, d: &wgpu::Device, joints: &[JointMatrix]) -> Result<(), RenderError> {
        let needed = joints.len().max(1);
        if self.skin_buf.is_none() || self.skin_capacity < needed {
            let mut capacity = self.skin_capacity.max(1024);
            while capacity < needed {
                capacity *= 2;
            }
            self.skin_buf = Some(make_buffer(
                d,
                "skin palette",
                (size_of::<JointMatrix>() * capacity) as u64,
                wgpu::BufferUsages::STORAGE,
            ));
            self.skin_capacity = capacity;
            self.skin_generation += 1;
            self.skin_bytes.clear();
            // Object bind groups hold the old buffer: `ensure_object` rebinds each on its next
            // use (generation check) instead of dropping the whole cache.
        }
        let buf = self.skin_buf.clone().expect("skin buffer");
        // The palette bytes are the caller's slice (or one stack matrix). No second heap copy.
        if joints.is_empty() {
            let identity = [JointMatrix::identity()];
            let bytes: &[u8] = bytemuck::cast_slice(&identity);
            #[cfg(test)]
            note_palette_upload(false, bytes.len() == std::mem::size_of::<JointMatrix>());
            crate::staging::defer_if_changed(&mut self.belt, d, &mut self.skin_bytes, &buf, bytes);
        } else {
            let bytes: &[u8] = bytemuck::cast_slice(joints);
            #[cfg(test)]
            {
                let direct = bytemuck::cast_slice::<JointMatrix, u8>(joints);
                note_palette_upload(std::ptr::eq(bytes.as_ptr(), direct.as_ptr()), false);
            }
            crate::staging::defer_if_changed(&mut self.belt, d, &mut self.skin_bytes, &buf, bytes);
        }
        Ok(())
    }

    /// Whether `mesh` is a one-joint skin whose every vertex is bound rigidly to that joint
    /// ([`rigid_to_joint_zero`]), cached per mesh key and vertex count: like the mesh cache
    /// ([`Self::ensure_mesh`]), a key names its content, so a host that rebuilds the vertex
    /// array each frame is not rescanned. Key `0` (ephemeral) is never one.
    pub(super) fn rigid_skin_bound(&mut self, mesh: &FrameMesh<'_>) -> bool {
        if mesh.joints.len() != 1 || mesh.mesh_key == 0 {
            return false;
        }
        let count = mesh.vertices.len();
        // A skinned counted upload: measured at the upload (ADR-8610).
        if count == 0
            && let Some((base, _)) = Self::counted_draw(mesh)
            && let Some(skin) = self.asset_mesh_skins.get(&base)
        {
            return skin.rigid_to_joint_zero;
        }
        match self.rigid_skin.get(&mesh.mesh_key) {
            Some(&(checked, rigid)) if checked == count => rigid,
            _ => {
                let rigid = rigid_to_joint_zero(mesh.vertices);
                self.rigid_skin.insert(mesh.mesh_key, (count, rigid));
                rigid
            }
        }
    }

    /// The model matrix `mesh` draws with as a rigid mesh when its one-joint skin folds
    /// ([`Self::set_rigid_skin_fold`]): `model × joint`.
    pub(super) fn rigid_skin_model(&mut self, mesh: &FrameMesh<'_>) -> Option<[[f32; 4]; 4]> {
        if self.rigid_skin_fold_off || !self.rigid_skin_bound(mesh) {
            return None;
        }
        let joint = mesh.joints.first()?;
        Some(fold_joint(mesh.model, joint))
    }

    /// Copies `mesh`'s vertices and indices into ranges of the mesh arenas (a mesh too large
    /// for an arena chunk gets its own buffers). With `belt` (inside [`Self::render_into`],
    /// before the frame encoder's first flush) small meshes go through the staging belt when it
    /// has room without growing; otherwise each stream is one `queue.write_buffer`. Returns
    /// `(vb, ib, base_vertex, first_index, leases)` for the [`MeshEntry`].
    #[allow(clippy::type_complexity)]
    pub(super) fn upload_mesh_geometry(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        mesh: &FrameMesh<'_>,
        belt: bool,
    ) -> (
        wgpu::Buffer,
        wgpu::Buffer,
        i32,
        u32,
        [Option<std::sync::Arc<crate::mesh_storage::ArenaLease>>; 2],
    ) {
        let arenas = self.mesh_arenas.get_or_insert_with(|| MeshArenas::new(d));
        let vertices = arenas.vertices.alloc(d, mesh.vertices.len() as u32);
        let indices = arenas.indices.alloc(d, mesh.indices.len() as u32);
        let vertex_offset = u64::from(vertices.first) * arenas.vertices.stride();
        let index_offset = u64::from(indices.first) * arenas.indices.stride();
        let vertex_data: &[u8] = bytemuck::cast_slice(mesh.vertices);
        let index_data: &[u8] = bytemuck::cast_slice(mesh.indices);
        let bytes = (vertex_data.len() + index_data.len()) as u64;
        crate::frame_stats::note_mesh_upload(bytes, vertices.created + indices.created);
        let via_belt = belt
            && self.mesh_belt_bytes + bytes <= MESH_BELT_FRAME_BYTES
            && self.belt.has_room(bytes + MESH_BELT_HEADROOM);
        for (buffer, offset, data) in [
            (&vertices.buffer, vertex_offset, vertex_data),
            (&indices.buffer, index_offset, index_data),
        ] {
            if data.is_empty() {
                continue;
            }
            if via_belt {
                self.belt.defer(d, buffer, offset, data);
            } else {
                q.write_buffer(buffer, offset, data);
            }
        }
        if via_belt {
            self.mesh_belt_bytes += bytes;
            self.mesh_belt_keys.push(mesh.mesh_key);
        }
        (
            vertices.buffer,
            indices.buffer,
            vertices.first as i32,
            indices.first,
            [vertices.lease, indices.lease],
        )
    }

    /// Returns the index count (uploads the mesh on first use or content change). `belt`: see
    /// [`Self::upload_mesh_geometry`].
    pub(super) fn ensure_mesh(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        mesh: &FrameMesh<'_>,
        now: u64,
        belt: bool,
    ) -> Result<u32, RenderError> {
        // A counted draw's deformed copy (morph targets, ADR-7721): a GPU copy of its upload.
        if mesh.vertices.is_empty()
            && let Some(source) = self.morph.counted_twin_source(mesh.mesh_key)
        {
            return self.ensure_counted_morph_twin(d, q, mesh, source, now, belt);
        }
        if let Some((base, lod)) = Self::counted_draw(mesh) {
            if mesh.mesh_key != base {
                // A LOD, shadow-LOD or cross-fade variant: the adopted vertex buffer with the
                // LOD's own index list (ADR-7428).
                self.ensure_asset_mesh(base, now);
                return Ok(self.ensure_counted_variant(d, q, mesh, (base, lod), now));
            }
            // A counted mesh: its uploaded buffers, adopted at the start of the frame (ADR-7284).
            let index_count = self.ensure_asset_mesh(mesh.mesh_key, now);
            // A skinned counted mesh drawn with joints has no static bound, as on the CPU path;
            // drawn rigid (or folded) it takes its rest bound back (ADR-8610).
            if !mesh.joints.is_empty() || self.asset_mesh_skins.contains_key(&mesh.mesh_key) {
                let static_bound = mesh.joints.is_empty() || self.rigid_skin_model(mesh).is_some();
                let rest = self
                    .asset_mesh_skins
                    .get(&mesh.mesh_key)
                    .and_then(|skin| skin.rest_bounds);
                if let Some(entry) = self.meshes.get_mut(&mesh.mesh_key)
                    && entry.static_bound != static_bound
                {
                    entry.static_bound = static_bound;
                    entry.bounds = if static_bound { rest } else { None };
                }
            }
            return Ok(index_count);
        }
        // A resident draw (`host_geometry`, ADR-7891): the cached buffers as they are. Its host
        // dropped the bytes; a rigid draw visiting marks the rest bound static, as below.
        if self.is_resident_draw(mesh)
            && let Some(entry) = self.meshes.get_mut(&mesh.mesh_key)
        {
            entry.last_used = now;
            entry.static_bound |= entry.bounds.is_some();
            return Ok(entry.index_count);
        }
        let vertex_bytes = std::mem::size_of_val(mesh.vertices);
        let index_bytes = std::mem::size_of_val(mesh.indices);
        let index_count = mesh.indices.len() as u32;
        // The entry always keeps the rest-pose sphere (`mesh_bounds`). A reader uses it only for
        // a static draw (rigid, or a folded one-joint skin). A skinned visit must not clear it:
        // the same mesh can be drawn both ways, and the GPU cull runs after every visit.
        // `static_bound` records that a static draw has visited. It is never cleared by a later
        // skinned visit (sun tracing builds a rest-pose structure from it). A folded one-joint
        // skin reaches here with its joints already emptied; prewarm still has the palette, so
        // `rigid_skin_model` marks that visit static too.
        let visitor_static = mesh.joints.is_empty() || self.rigid_skin_model(mesh).is_some();
        let rev = crate::runtime::mesh_content_revision(mesh.mesh_key);
        let resident = self.meshes.get(&mesh.mesh_key);
        let size_changed = resident
            .is_none_or(|entry| entry.vertex_bytes != vertex_bytes || entry.index_bytes != index_bytes);
        let applied = resident.map(|entry| entry.content_rev).unwrap_or(0);
        let refresh = crate::runtime::mesh_refresh(size_changed, rev, applied);
        if refresh == crate::runtime::MeshRefresh::Rewrite {
            let next_gen = self.mesh_generation + 1;
            let mut wrote = false;
            if let Some(entry) = self.meshes.get_mut(&mesh.mesh_key) {
                let vertex_data: &[u8] = bytemuck::cast_slice(mesh.vertices);
                let index_data: &[u8] = bytemuck::cast_slice(mesh.indices);
                let vertex_stride = std::mem::size_of::<crate::lab_frame::LabVertex>() as u64;
                let vertex_offset = u64::from(entry.base_vertex.max(0) as u32) * vertex_stride;
                let index_offset = u64::from(entry.first_index) * std::mem::size_of::<u32>() as u64;
                if !vertex_data.is_empty() {
                    q.write_buffer(&entry.vb, vertex_offset, vertex_data);
                }
                if !index_data.is_empty() {
                    q.write_buffer(&entry.ib, index_offset, index_data);
                }
                entry.generation = next_gen;
                entry.content_rev = rev;
                // New content, new rest sphere, whoever visits (the readers pick it only for a
                // static draw); `static_bound` is updated with the visit below.
                entry.bounds = mesh_bounds(mesh.vertices);
                // The second UV stream is a copy of the old vertices: the next draw that reads
                // it uploads it again (ADR-5594).
                entry.uv1 = None;
                wrote = true;
            }
            if wrote {
                self.mesh_generation = next_gen;
                self.forget_mesh_derivations(mesh.mesh_key);
            }
        }
        let needs_upload = refresh == crate::runtime::MeshRefresh::Upload;
        if needs_upload {
            let _span = tracing::info_span!("mesh_upload", bytes = vertex_bytes + index_bytes).entered();
            let bounds = mesh_bounds(mesh.vertices);
            self.cache_stats.mesh_uploads += 1;
            // A dithered draw's entry (LOD cross-fade, its own key) shares the GPU buffers of
            // the whole mesh when that is resident: a fade creates no buffers on the frame path
            // (hitch-hunt: prewarmed scatter variants stay the only uploads).
            let shared = (mesh.lod_dither != LOD_DITHER_NONE)
                .then(|| self.meshes.get(&(mesh.mesh_key ^ LOD_DITHER_MESH_TAG)))
                .flatten()
                .filter(|base| base.vertex_bytes == vertex_bytes && base.index_bytes == index_bytes)
                .map(|base| {
                    (
                        base.vb.clone(),
                        base.ib.clone(),
                        base.base_vertex,
                        base.first_index,
                        base.leases.clone(),
                    )
                });
            // The old entry's ranges go back to the arenas before the new ones are taken.
            self.meshes.remove(&mesh.mesh_key);
            let (vb, ib, base_vertex, first_index, leases) = match shared {
                Some(shared) => shared,
                None => self.upload_mesh_geometry(d, q, mesh, belt),
            };
            self.mesh_generation += 1;
            let generation = self.mesh_generation;
            self.meshes.insert(
                mesh.mesh_key,
                MeshEntry {
                    vb,
                    ib,
                    base_vertex,
                    first_index,
                    leases,
                    uv1: None,
                    index_count,
                    vertex_bytes,
                    index_bytes,
                    bounds,
                    static_bound: bounds.is_some() && visitor_static,
                    uv_span: crate::texture_stream::uv_span(mesh.vertices, mesh.indices),
                    last_used: now,
                    dithered: mesh.lod_dither != LOD_DITHER_NONE,
                    generation,
                    content_rev: rev,
                },
            );
        } else if let Some(entry) = self.meshes.get_mut(&mesh.mesh_key) {
            entry.last_used = now;
            // A later static visit may mark the entry. The rest sphere stays whatever this
            // visit is: a skinned draw does not overwrite it, and a folded one does not
            // recompute it (the vertices did not change).
            entry.static_bound |= entry.bounds.is_some() && visitor_static;
        }
        // Test hook: the cache before ADR-4577 (v0.1.13), where a skinned visit cleared the
        // entry's cull bound and a static visit put it back (`lifecycle` tests, the DO27
        // hand-built pilot).
        #[cfg(test)]
        if self.entries.legacy_skinned_visit_clears_bound
            && let Some(entry) = self.meshes.get_mut(&mesh.mesh_key)
        {
            entry.bounds = if visitor_static {
                mesh_bounds(mesh.vertices)
            } else {
                None
            };
        }
        Ok(self
            .meshes
            .get(&mesh.mesh_key)
            .map(|entry| entry.index_count)
            .unwrap_or(index_count))
    }

    /// Drops what the renderer's other caches derived from `key`'s bytes, after those bytes were
    /// rewritten in place under the same key (ADR-5594): the visibility buffer's clusters (built
    /// from the old triangles; cooked meshlets for the key describe the old mesh too), the
    /// compute-skinning twin, the skinned bind-pose bounds and joint reach, and the rigid-skin
    /// fold memo. Each is rebuilt from the new bytes on the mesh's next use. Mesh keys whose
    /// content never changes in place never come here.
    fn forget_mesh_derivations(&mut self, key: u64) {
        let keys = [key];
        self.rigid_skin.remove(&key);
        self.skin_bounds.forget(&keys);
        self.release_skin_twins(&keys);
        // Only a key the path holds: releasing marks the cluster LOD history for a clear.
        if self.visbuffer.has_meshlets(key) {
            self.visbuffer.release_meshes(&keys);
        }
    }

    /// Uploads the second UV stream of `mesh` (its [`FrameMesh::uv1`], or the first set when it
    /// has none) once, for a graph material that reads it.
    ///
    /// The stream is its own buffer from vertex 0, and a draw's `base_vertex` offsets every
    /// vertex buffer slot: a mesh in a vertex-arena range (`base_vertex` > 0) first gets its own
    /// vertex buffer so both slots start at its first vertex. It keeps its arena range until the
    /// entry drops (a belt copy into the range may still be pending this frame; freeing it now
    /// could hand the range to a `queue.write_buffer` the copy would then overwrite).
    pub(super) fn ensure_mesh_uv1(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        mesh: &FrameMesh<'_>,
    ) {
        if Self::is_asset_draw(mesh) {
            // No CPU vertices: the stream comes from the draw's own second UV set (ADR-7330).
            self.ensure_asset_uv1(d, q, mesh);
            return;
        }
        let Some(entry) = self.meshes.get_mut(&mesh.mesh_key) else {
            return;
        };
        if entry.uv1.is_some() {
            return;
        }
        if entry.base_vertex != 0 {
            let vertex_data: &[u8] = bytemuck::cast_slice(mesh.vertices);
            let vb = make_buffer(
                d,
                "mesh vb (uv1)",
                vertex_data.len().max(4) as u64,
                wgpu::BufferUsages::VERTEX | crate::mesh_storage::blas_input_usage(d.features()),
            );
            crate::frame_stats::note_mesh_upload(vertex_data.len() as u64, 1);
            if !vertex_data.is_empty() {
                q.write_buffer(&vb, 0, vertex_data);
            }
            entry.vb = vb;
            entry.base_vertex = 0;
        }
        let uvs: Vec<[f32; 2]> = if mesh.uv1.len() == mesh.vertices.len() {
            mesh.uv1.to_vec()
        } else {
            mesh.vertices.iter().map(|v| v.uv).collect()
        };
        let bytes: &[u8] = bytemuck::cast_slice(&uvs);
        let buf = make_buffer(
            d,
            "mesh uv1",
            bytes.len().max(8) as u64,
            wgpu::BufferUsages::VERTEX,
        );
        if !bytes.is_empty() {
            q.write_buffer(&buf, 0, bytes);
        }
        entry.uv1 = Some(buf);
    }

    pub(super) fn ensure_object(&mut self, d: &wgpu::Device, key: ObjectUniformKey, now: u64) {
        let d = &crate::device::TrackedDevice::observe(d);
        let generation = self.skin_generation;
        if let Some(entry) = self.objects.get_mut(&key) {
            entry.last_used = now;
            if entry.skin_generation == generation {
                return;
            }
            // The palette grew into a new buffer: rebind it, keeping the uniform and its
            // contents (no new buffer, no re-upload).
            let (Some(pipes), Some(skin_buf)) = (self.pipes.as_ref(), self.skin_buf.as_ref()) else {
                return;
            };
            entry.bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("object bg"),
                layout: &pipes.object_bgl,
                entries: &[
                    binding(0, entry.buf.as_entire_binding()),
                    binding(1, skin_buf.as_entire_binding()),
                ],
            });
            entry.skin_generation = generation;
            return;
        }
        let Some(pipes) = self.pipes.as_ref() else { return };
        let Some(skin_buf) = self.skin_buf.as_ref() else {
            return;
        };
        let buf = make_buffer(
            d,
            "object uniform",
            size_of::<ObjectData>() as u64,
            wgpu::BufferUsages::UNIFORM,
        );
        let bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("object bg"),
            layout: &pipes.object_bgl,
            entries: &[
                binding(0, buf.as_entire_binding()),
                binding(1, skin_buf.as_entire_binding()),
            ],
        });
        self.objects.insert(
            key,
            ObjectEntry {
                buf,
                bg,
                skin_generation: generation,
                last_used: now,
                uploaded: None,
            },
        );
    }

    /// The byte budget of the mesh and material caches ([`CacheResidency::budget_bytes`]): the
    /// host's, or the sum of the derived mesh and material budgets.
    pub(super) fn cache_budget_bytes(&self) -> u64 {
        if self.residency.budget_bytes != 0 {
            return self.residency.budget_bytes;
        }
        let (mesh, material) = self.derived_budgets();
        mesh.saturating_add(material)
    }

    /// The derived `(mesh, material)` budgets ([`CacheResidency::budget_bytes`] `0`).
    pub(super) fn derived_budgets(&self) -> (u64, u64) {
        #[cfg(test)]
        if let Some((mesh, material, _)) = self.derived_override {
            return (mesh, material);
        }
        derived_cache_budgets(
            self.stream_frame.policy.resident_budget_bytes(),
            self.adapter_vram_mib.flatten(),
            cfg!(target_os = "android"),
        )
    }

    /// The newest frame of a sweep at least [`CACHE_MIN_IDLE_SECS`] ago (0 when none is that
    /// old): entries drawn after it are kept by the derived budgets. Records this sweep.
    pub(super) fn sweep_clock_cutoff(&mut self, now: u64) -> u64 {
        let at = std::time::Instant::now();
        #[cfg(test)]
        let floor = self
            .derived_override
            .map_or(CACHE_MIN_IDLE_SECS, |(_, _, secs)| secs);
        #[cfg(not(test))]
        let floor = CACHE_MIN_IDLE_SECS;
        let old_enough = |t: &std::time::Instant| at.duration_since(*t).as_secs_f32() >= floor;
        // Its room is made once, by `FrameRenderer::new` (here for a renderer made otherwise):
        // grown lazily, it allocated on whichever later sweep found it full, a lone allocation
        // in a steady frame (ADR-2299).
        if self.sweep_clock.capacity() < SWEEP_CLOCK_SAMPLES {
            self.sweep_clock
                .reserve_exact(SWEEP_CLOCK_SAMPLES - self.sweep_clock.len());
        }
        if self.sweep_clock.back().is_none_or(|(frame, _)| *frame < now) {
            if self.sweep_clock.len() >= SWEEP_CLOCK_SAMPLES {
                // Full (more than SWEEP_CLOCK_SAMPLES sweeps within the floor: a very high frame
                // rate): the newest sample moves on. The older ones, the cutoff among them, stay,
                // so the cutoff can only be older than it would be, which keeps more.
                self.sweep_clock.pop_back();
            }
            self.sweep_clock.push_back((now, at));
        }
        // Keep one sample at least that old: the cutoff.
        while self.sweep_clock.len() >= 2 && self.sweep_clock.get(1).is_some_and(|(_, t)| old_enough(t)) {
            self.sweep_clock.pop_front();
        }
        match self.sweep_clock.front() {
            Some((frame, t)) if old_enough(t) => *frame,
            _ => 0,
        }
    }

    /// Evicts idle object slots, meshes and materials ([`CacheResidency`]). Under memory
    /// `pressure` ([`Self::trim_memory`]) every unpinned entry idle [`PRESSURE_MIN_IDLE_FRAMES`]
    /// goes, whatever the budget.
    pub(super) fn evict(&mut self, now: u64, pressure: bool) {
        let _span = tracing::info_span!("cache_evict", pressure).entered();
        let policy = self.residency;
        let min_idle = if pressure {
            policy.min_idle_frames.min(PRESSURE_MIN_IDLE_FRAMES)
        } else {
            policy.min_idle_frames
        };
        let max_idle = policy.max_idle_frames;
        let clock_cutoff = self.sweep_clock_cutoff(now);
        // Taken for the sweep (material evictions release shared textures through `&mut self`).
        let pinned = std::mem::take(&mut self.pinned);
        // A dithered draw's entry (LOD cross-fade) is pinned with its whole mesh.
        // Counted meshes drawn from their upload (ADR-7330): the asset server owns and frees their
        // buffers, so evicting one would free nothing; they are never candidates and their bytes
        // are reported apart from the cache's own.
        let adopted = std::mem::take(&mut self.asset_mesh_keys);
        let mesh_pinned = |key: &u64| {
            pinned.contains(key) || pinned.contains(&(key ^ LOD_DITHER_MESH_TAG)) || adopted.contains(key)
        };
        let old = now.saturating_sub(max_idle);
        let (meshes_before, materials_before) = (self.meshes.len(), self.materials.len());
        self.meshes
            .retain(|key, entry| entry.last_used >= old || mesh_pinned(key));
        let aged_materials: Vec<u64> = self
            .materials
            .iter()
            .filter(|(key, entry)| entry.last_used < old && !pinned.contains(*key))
            .map(|(key, _)| *key)
            .collect();
        for key in aged_materials {
            self.forget_material(key);
        }
        let object_cutoff = now.saturating_sub(EVICT_AFTER_FRAMES);
        self.objects.retain(|_, entry| entry.last_used >= object_cutoff);
        let mut adopted_bytes = 0u64;
        let mut mesh_bytes = 0u64;
        for (key, entry) in &self.meshes {
            if adopted.contains(key) {
                adopted_bytes += mesh_entry_bytes(entry);
            } else {
                mesh_bytes += cache_entry_bytes(&self.meshes, &adopted, *key, entry);
            }
        }
        let mut material_bytes: u64 = self.materials.values().map(material_entry_bytes).sum();
        let before = mesh_bytes + material_bytes;
        // (last drawn, is a material, key, bytes) of the entries idle before `idle`.
        let candidates = |this: &Self, idle: u64, meshes: bool, materials: bool| {
            let mut out: Vec<(u64, bool, u64, u64)> = Vec::new();
            if meshes {
                out.extend(
                    this.meshes
                        .iter()
                        .filter(|(key, entry)| entry.last_used < idle && !mesh_pinned(key))
                        .map(|(key, entry)| {
                            let bytes = cache_entry_bytes(&this.meshes, &adopted, *key, entry);
                            (entry.last_used, false, *key, bytes)
                        }),
                );
            }
            if materials {
                out.extend(
                    this.materials
                        .iter()
                        .filter(|(key, entry)| entry.last_used < idle && !pinned.contains(*key))
                        .map(|(key, entry)| (entry.last_used, true, *key, material_entry_bytes(entry))),
                );
            }
            out.sort_unstable();
            out
        };
        let budget = self.cache_budget_bytes();
        if policy.budget_bytes != 0 {
            // The host's budget: one least-recently-drawn sweep over both caches (ADR-183).
            let mut resident = before;
            if resident > budget || pressure {
                let idle = now.saturating_sub(min_idle);
                for (_, material, key, bytes) in candidates(self, idle, true, true) {
                    if resident <= budget && !pressure {
                        break;
                    }
                    if material {
                        self.forget_material(key);
                        material_bytes = material_bytes.saturating_sub(bytes);
                    } else {
                        self.meshes.remove(&key);
                        mesh_bytes = mesh_bytes.saturating_sub(bytes);
                    }
                    resident = resident.saturating_sub(bytes);
                }
            }
        } else {
            // Derived budgets: each cache against its own, so textures never evict meshes; an
            // entry drawn in the last CACHE_MIN_IDLE_SECS stays (pressure aside); a cache over
            // its budget goes down to its low-water mark.
            let (mesh_budget, material_budget) = self.derived_budgets();
            let idle = if pressure {
                now.saturating_sub(min_idle)
            } else {
                let frame_idle = now.saturating_sub(min_idle);
                // Complete frames (an offline capture) never read the wall clock: eviction
                // follows the frame-index gate alone, so two capture runs on machines of
                // different speed keep and drop the same entries.
                if self.complete_frames.is_some() {
                    frame_idle
                } else {
                    frame_idle.min(clock_cutoff)
                }
            };
            let low = |b: u64| b / 8 * CACHE_LOW_WATER_EIGHTHS;
            if mesh_bytes > mesh_budget || pressure {
                let target = low(mesh_budget);
                for (_, _, key, bytes) in candidates(self, idle, true, false) {
                    if mesh_bytes <= target && !pressure {
                        break;
                    }
                    self.meshes.remove(&key);
                    mesh_bytes = mesh_bytes.saturating_sub(bytes);
                }
            }
            if material_bytes > material_budget || pressure {
                let target = low(material_budget);
                for (_, _, key, bytes) in candidates(self, idle, false, true) {
                    if material_bytes <= target && !pressure {
                        break;
                    }
                    self.forget_material(key);
                    material_bytes = material_bytes.saturating_sub(bytes);
                }
            }
        }
        let resident = mesh_bytes + material_bytes;
        // Evicted entries returned their ranges to the arenas; under pressure the chunks left
        // with nothing in them go too (the next mesh after that creates one again).
        let mut arena_freed = 0;
        if let Some(arenas) = self.mesh_arenas.as_mut() {
            arenas.vertices.reclaim();
            arenas.indices.reclaim();
            if pressure {
                arena_freed = arenas.vertices.release_empty() + arenas.indices.release_empty();
            }
        }
        self.pinned = pinned;
        let adopted_meshes = adopted.len();
        self.asset_mesh_keys = adopted;
        let meshes = &self.meshes;
        self.rigid_skin
            .retain(|key, _| meshes.contains_key(key) || meshes.contains_key(&(key ^ LOD_DITHER_MESH_TAG)));
        self.skin_bounds.retain_resident(meshes);
        self.may_stream = self.materials.values().any(|entry| entry.stream.is_some());
        if pressure {
            tracing::info!(
                target: "regia_render",
                "memory pressure: evicted {} idle meshes and {} idle materials ({} bytes), {} bytes of empty mesh-arena chunks",
                meshes_before - self.meshes.len(),
                materials_before - self.materials.len(),
                before.saturating_sub(resident),
                arena_freed,
            );
        }
        self.cache_stats.meshes_evicted += (meshes_before - self.meshes.len()) as u64;
        self.cache_stats.materials_evicted += (materials_before - self.materials.len()) as u64;
        self.cache_stats.resident_bytes = resident;
        self.cache_stats.budget_bytes = budget;
        self.cache_budgets.mesh_bytes = mesh_bytes;
        self.cache_budgets.material_bytes = material_bytes;
        self.cache_budgets.adopted_meshes = adopted_meshes;
        self.cache_budgets.adopted_mesh_bytes = adopted_bytes;
    }

    /// Drop a material cache entry and the image-identity textures only it still holds.
    pub(super) fn forget_material(&mut self, key: u64) {
        if let Some(entry) = self.materials.remove(&key) {
            let MaterialEntry {
                consts,
                shares,
                pinned,
                table,
                ..
            } = entry;
            consts.release(self.material_arena.as_mut(), self.frame_index);
            if let (Some(texture_table), Some(indices)) = (self.texture_table.as_mut(), table) {
                texture_table.release_set(indices);
            }
            for share in shares {
                self.shared_textures.release(share);
            }
            drop(pinned);
        }
    }
}

impl FrameRenderer {
    /// Packs the frame's joint palette (identity first, then each skinned mesh's joints) and
    /// writes it; returns the palette, each non-empty mesh's `(joint base, model, folded)` span
    /// and how many one-joint rigid skins were folded. Moved out of `render_into` unchanged
    /// (ADR-1552 split, line m).
    pub(super) fn pack_skin_palette(
        &mut self,
        d: &crate::TrackedDevice,
        scene: &FrameScene<'_>,
    ) -> Result<(Vec<JointMatrix>, Vec<scratch::SkinSpan>, usize), RenderError> {
        self.begin_skin_twins();
        // Counted meshes drawn from their upload: residency is read once, here (ADR-7284).
        self.sweep_asset_meshes(scene);
        let mut packed_joints = self.scratch.joints.take();
        packed_joints.push(JointMatrix::identity());
        // A skinned counted upload drawn rigid (no joints) reads its weighted joints from
        // the palette's start: as many identities as the farthest of them reads draw it at its
        // rest pose (ADR-8610).
        let rest = self.counted_rest_palette(scene);
        if rest > packed_joints.len() {
            packed_joints.resize(rest, JointMatrix::identity());
        }
        // (joint base, model, a folded one-joint rigid skin)
        let mut spans = self.scratch.spans.take();
        for mesh in scene.meshes.iter().copied() {
            // Bytes, or a resident draw (`host_geometry`, ADR-7891): the draw list's rule.
            if !self.frame_draws_mesh(&mesh) {
                continue;
            }
            if let Some(model) = self.rigid_skin_model(&mesh) {
                spans.push((0, model, true));
                continue;
            }
            let joint_base = if mesh.joints.is_empty() {
                0
            } else {
                let joint_base = packed_joints.len() as u32;
                let count = mesh.joints.len().min(MAX_JOINTS_PER_SKIN);
                packed_joints.extend_from_slice(&mesh.joints[..count]);
                joint_base
            };
            spans.push((joint_base, mesh.model, false));
        }
        let rigid_skin_folded = spans.iter().filter(|span| span.2).count();
        self.write_skin(d, &packed_joints)?;
        Ok((packed_joints, spans, rigid_skin_folded))
    }

    /// Pointer and capacity of the palette byte cache (tests).
    #[cfg(test)]
    fn skin_cache_ptr_cap(&self) -> (*const u8, usize) {
        (self.skin_bytes.as_ptr(), self.skin_bytes.capacity())
    }
}

#[cfg(test)]
fn note_palette_upload(aliased: bool, stack_identity: bool) {
    PALETTE_ALIASED.with(|cell| cell.set(aliased));
    PALETTE_STACK.with(|cell| cell.set(stack_identity));
}

#[cfg(test)]
fn palette_aliased() -> bool {
    PALETTE_ALIASED.with(std::cell::Cell::get)
}

#[cfg(test)]
fn palette_stack_identity() -> bool {
    PALETTE_STACK.with(std::cell::Cell::get)
}

#[cfg(test)]
thread_local! {
    static PALETTE_ALIASED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static PALETTE_STACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Sweep samples the eviction clock keeps (`FrameRenderer::sweep_clock_cutoff`): with a sweep
/// every `EVICT_SWEEP_EVERY` (256) frames, 32 cover the 20 s floor up to about 400 frames per
/// second; past that the newest sample is replaced instead of a new one kept.
pub(super) const SWEEP_CLOCK_SAMPLES: usize = 32;

#[cfg(test)]
mod sweep_clock_tests {
    use super::{FrameRenderer, SWEEP_CLOCK_SAMPLES};

    /// ADR-2299: the sweep clock's room is made with the renderer and never grows after it,
    /// however many sweeps fall within the floor; the oldest sample (the cutoff to come) stays.
    #[test]
    fn the_sweep_clock_keeps_a_fixed_room() {
        let mut renderer = FrameRenderer::new();
        // The room is there before the first sweep.
        let room = renderer.sweep_clock.capacity();
        assert!(room >= SWEEP_CLOCK_SAMPLES);
        // No sample is ever old enough: every sweep's sample is kept, as at a high frame rate.
        renderer.derived_override = Some((1, 1, 3600.0));
        assert_eq!(renderer.sweep_clock_cutoff(0), 0);
        for k in 1..200u64 {
            assert_eq!(renderer.sweep_clock_cutoff(k * 256), 0);
        }
        assert_eq!(renderer.sweep_clock.capacity(), room, "the sweep clock grew");
        assert_eq!(renderer.sweep_clock.len(), SWEEP_CLOCK_SAMPLES);
        assert_eq!(renderer.sweep_clock.front().map(|s| s.0), Some(0));
        assert_eq!(renderer.sweep_clock.back().map(|s| s.0), Some(199 * 256));
        // Samples stay in frame order.
        let frames: Vec<u64> = renderer.sweep_clock.iter().map(|s| s.0).collect();
        assert!(frames.windows(2).all(|w| w[0] < w[1]), "{frames:?}");
    }

    /// With no floor every sample is old enough at once: the cutoff is this sweep's frame and
    /// one sample is kept.
    #[test]
    fn with_no_floor_the_cutoff_is_this_sweep() {
        let mut renderer = FrameRenderer::new();
        renderer.derived_override = Some((1, 1, 0.0));
        for k in 0..50u64 {
            assert_eq!(renderer.sweep_clock_cutoff(k * 256), k * 256);
            assert_eq!(renderer.sweep_clock.len(), 1);
        }
    }
}

#[cfg(test)]
mod skin_upload_tests {
    use super::{palette_aliased, palette_stack_identity};
    use crate::skin::JointMatrix;

    /// A second write of the same joints aliases the caller's slice and does not reallocate
    /// the byte cache. An empty skin uses one stack identity matrix.
    #[test]
    fn write_skin_does_not_allocate_a_second_palette() {
        let device = crate::test_device();
        let mut renderer = super::FrameRenderer::new();
        let joints = [JointMatrix::identity(), JointMatrix::translation(1.0, 0.0, 0.0)];
        renderer
            .write_skin(&device.device, &joints)
            .expect("first palette");
        assert!(palette_aliased(), "the upload must alias the caller's joints");
        let (ptr, cap) = renderer.skin_cache_ptr_cap();
        assert!(cap >= std::mem::size_of_val(&joints));
        renderer
            .write_skin(&device.device, &joints)
            .expect("second palette");
        assert!(palette_aliased());
        let (ptr2, cap2) = renderer.skin_cache_ptr_cap();
        assert_eq!(ptr2, ptr, "the second write reallocated the palette cache");
        assert_eq!(cap2, cap);
        renderer.write_skin(&device.device, &[]).expect("empty palette");
        assert!(palette_stack_identity(), "an empty skin heap-copied the identity");
        let (empty_ptr, empty_cap) = renderer.skin_cache_ptr_cap();
        renderer.write_skin(&device.device, &[]).expect("empty again");
        assert!(palette_stack_identity());
        let (empty_ptr2, empty_cap2) = renderer.skin_cache_ptr_cap();
        assert_eq!(empty_ptr2, empty_ptr);
        assert_eq!(empty_cap2, empty_cap);
    }
}

#[cfg(test)]
mod rewrite_tests {
    use std::sync::Arc;

    use super::FrameRenderer;
    use crate::frame::FrameMesh;
    use crate::skin::JointMatrix;
    use crate::{CpuMesh, LabMaterial, LabVertex, ShadowCast};

    /// One triangle whose vertices weight joint `joint` fully.
    fn triangle(joint: u32) -> Vec<LabVertex> {
        [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]
            .into_iter()
            .map(|p| LabVertex {
                joints: [joint, 0, 0, 0],
                weights: [1.0, 0.0, 0.0, 0.0],
                ..LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], [p[0], p[1]])
            })
            .collect()
    }

    fn frame_mesh<'a>(
        key: u64,
        cpu: &'a CpuMesh,
        material: &'a LabMaterial,
        joints: &'a [JointMatrix],
    ) -> FrameMesh<'a> {
        FrameMesh {
            mesh_key: key,
            material_key: 1,
            object_key: 1,
            vertices: &cpu.vertices,
            indices: &cpu.indices,
            material,
            model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            joints,
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            is_static: false,
            lod_dither: 0.0,
        }
    }

    /// ADR-5594 (correctness audit, render frame): a mesh whose bytes are rewritten in place under
    /// one key (`with_unique_mesh_mut` + `note_mesh_content_changed`, same byte size) must not keep
    /// what other caches built from the old bytes. Before the fix only the vertex and index
    /// buffers were rewritten: the second UV stream, the visibility buffer's meshlets, the rigid
    /// skin memo and the skinned joint reach all stayed, so the rewritten mesh drew old clusters
    /// and UVs and a skin that now weights a joint past its palette was not refused.
    #[test]
    fn an_in_place_rewrite_drops_what_was_derived_from_the_old_bytes() {
        let device = crate::test_device();
        let d = &device.device;
        let q = crate::device::TrackedQueue::observe(&device.queue);
        let material = LabMaterial::default();
        let palette = [JointMatrix::identity()];
        let mut cpu = Arc::new(CpuMesh {
            vertices: triangle(0),
            indices: vec![0, 1, 2],
        });
        let key = crate::mesh_key(&cpu);
        let mut renderer = FrameRenderer::new();
        renderer
            .ensure_mesh(d, &q, &frame_mesh(key, &cpu, &material, &palette), 0, false)
            .expect("first upload");
        renderer.ensure_mesh_uv1(d, &q, &frame_mesh(key, &cpu, &material, &palette));
        assert!(renderer.meshes.get(&key).is_some_and(|entry| entry.uv1.is_some()));
        renderer
            .visbuffer
            .insert_cooked(key, Arc::new(crate::MeshletMesh::default()));
        assert!(renderer.visbuffer.has_meshlets(key));
        renderer.rigid_skin.insert(key, (3, true));
        // Joint 0 of a one-joint palette: drawn, and the reach is memoized for this key and size.
        assert!(!renderer.refuse_skin_past_its_palette(key, &frame_mesh(key, &cpu, &material, &palette)));

        // Same size, new bytes: the triangle now weights joint 3.
        crate::with_unique_mesh_mut(&mut cpu, |mesh| mesh.vertices = triangle(3)).expect("unique owner");
        crate::note_mesh_content_changed(&cpu);
        renderer
            .ensure_mesh(d, &q, &frame_mesh(key, &cpu, &material, &palette), 1, false)
            .expect("rewrite");
        let entry = renderer.meshes.get(&key).expect("still resident");
        assert_eq!(entry.content_rev, crate::mesh_content_revision(key));
        assert!(
            entry.uv1.is_none(),
            "the old second UV stream survived the rewrite"
        );
        assert!(
            !renderer.visbuffer.has_meshlets(key),
            "the old meshlets survived the rewrite"
        );
        assert!(
            !renderer.rigid_skin.contains_key(&key),
            "the rigid-skin memo of the old bytes survived the rewrite"
        );
        assert!(
            renderer.refuse_skin_past_its_palette(key, &frame_mesh(key, &cpu, &material, &palette)),
            "the rewritten skin weights joint 3 of a one-joint palette and must be refused"
        );
        // The second UV stream comes back from the new bytes on the next draw that reads it.
        renderer.ensure_mesh_uv1(d, &q, &frame_mesh(key, &cpu, &material, &palette));
        assert!(renderer.meshes.get(&key).is_some_and(|entry| entry.uv1.is_some()));
    }

    /// A key that was never rewritten keeps its derived state across draws (no churn).
    #[test]
    fn a_mesh_drawn_again_unchanged_keeps_its_derived_state() {
        let device = crate::test_device();
        let d = &device.device;
        let q = crate::device::TrackedQueue::observe(&device.queue);
        let material = LabMaterial::default();
        let cpu = Arc::new(CpuMesh {
            vertices: triangle(0),
            indices: vec![0, 1, 2],
        });
        let key = crate::mesh_key(&cpu);
        let mut renderer = FrameRenderer::new();
        for now in 0..3 {
            renderer
                .ensure_mesh(d, &q, &frame_mesh(key, &cpu, &material, &[]), now, false)
                .expect("draw");
            renderer.ensure_mesh_uv1(d, &q, &frame_mesh(key, &cpu, &material, &[]));
            if now == 0 {
                renderer
                    .visbuffer
                    .insert_cooked(key, Arc::new(crate::MeshletMesh::default()));
            }
            assert!(renderer.meshes.get(&key).is_some_and(|entry| entry.uv1.is_some()));
            assert!(renderer.visbuffer.has_meshlets(key));
        }
    }
}

#[cfg(test)]
mod counted_variant_bytes_tests {
    use super::{FrameRenderer, MeshEntry};
    use crate::frame::LOD_DITHER_MESH_TAG;
    use crate::mesh_lod::{lod_variant_key, mesh_lod_cache_key};

    fn entry(vb: &wgpu::Buffer, ib: &wgpu::Buffer) -> MeshEntry {
        MeshEntry {
            vb: vb.clone(),
            ib: ib.clone(),
            base_vertex: 0,
            first_index: 0,
            leases: [None, None],
            uv1: None,
            index_count: (ib.size() / 4) as u32,
            vertex_bytes: vb.size() as usize,
            index_bytes: ib.size() as usize,
            bounds: None,
            static_bound: true,
            uv_span: 1.0,
            last_used: 1,
            dithered: false,
            generation: 0,
            content_rev: 0,
        }
    }

    /// rev-claude-56 counted-meshes finding 1: the LOD, shadow-LOD and cross-fade variants of an
    /// adopted counted mesh share its vertex buffer, which the asset server owns. The mesh cache's
    /// own bytes count only the index lists the variants uploaded, once per list (a variant on the
    /// upload's own list, or on a list its LOD's colour variant holds, adds nothing); the entries
    /// keep the shared buffer's `vertex_bytes` (what a structure built from them reads). Before the
    /// fix every variant added the whole vertex buffer to `mesh_bytes` again.
    #[test]
    fn counted_variants_count_only_the_index_lists_they_uploaded() {
        let device = crate::test_device();
        let buffer = |size: u64| {
            device.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("counted variant bytes"),
                size,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::INDEX,
                mapped_at_creation: false,
            })
        };
        let (vb, base_ib, lod1_ib, lod2_shadow_ib) = (
            buffer(96 * 1000),
            buffer(4 * 3000),
            buffer(4 * 600),
            buffer(4 * 120),
        );
        let (cpu_vb, cpu_ib) = (buffer(96 * 10), buffer(4 * 30));
        let base = crate::asset_mesh_key(regia_asset::AssetId::new_random());
        let colour1 = lod_variant_key(base, mesh_lod_cache_key(1, false));
        let shadow1 = lod_variant_key(base, mesh_lod_cache_key(1, true));
        let shadow2 = lod_variant_key(base, mesh_lod_cache_key(2, true));
        let mut renderer = FrameRenderer::new();
        renderer.meshes.insert(base, entry(&vb, &base_ib));
        renderer.asset_mesh_keys.insert(base);
        // LOD 0 faded and shadow: the upload's own list. LOD 1: one list for colour, faded and
        // shadow. LOD 2: a shadow-only list of its own.
        renderer
            .meshes
            .insert(base ^ LOD_DITHER_MESH_TAG, entry(&vb, &base_ib));
        renderer.meshes.insert(
            lod_variant_key(base, mesh_lod_cache_key(0, true)),
            entry(&vb, &base_ib),
        );
        renderer.meshes.insert(colour1, entry(&vb, &lod1_ib));
        renderer
            .meshes
            .insert(colour1 ^ LOD_DITHER_MESH_TAG, entry(&vb, &lod1_ib));
        renderer.meshes.insert(shadow1, entry(&vb, &lod1_ib));
        renderer.meshes.insert(shadow2, entry(&vb, &lod2_shadow_ib));
        renderer.meshes.insert(0x1234_5678, entry(&cpu_vb, &cpu_ib));
        renderer.evict(1, false);
        let budgets = renderer.cache_budgets();
        assert_eq!(budgets.adopted_meshes, 1);
        assert_eq!(budgets.adopted_mesh_bytes, vb.size() + base_ib.size());
        assert_eq!(
            budgets.mesh_bytes,
            lod1_ib.size() + lod2_shadow_ib.size() + cpu_vb.size() + cpu_ib.size(),
            "the variants count the lists they uploaded, once each, and no vertex buffer"
        );
        assert_eq!(renderer.meshes.len(), 8, "nothing evicted under the budget");
        assert_eq!(
            renderer.meshes[&colour1].vertex_bytes,
            vb.size() as usize,
            "a variant keeps the shared buffer's size"
        );
    }
}
