// SPDX-License-Identifier: MPL-2.0
//! Material uploads, bind groups and texture streaming with its budgets and reports (moved out
//! of `frame.rs` unchanged, ADR-1552).

use super::*;

/// The streamed slots of `mat`, in [`crate::texture_stream::STREAMED_SLOTS`] order: a cooked
/// block-compressed chain the device samples, else the RGBA8 map. (A cooked chain the device
/// cannot sample is not a source: its CPU-decoded fallback uploads whole.)
pub(super) fn stream_sources(
    mat: &LabMaterial,
    features: wgpu::Features,
) -> [Option<crate::texture_stream::SlotSource<'_>>; crate::texture_stream::STREAMED_SLOTS] {
    fn slot(t: Option<&crate::LabTexture>, srgb: bool) -> Option<crate::texture_stream::SlotSource<'_>> {
        t.map(|t| crate::texture_stream::SlotSource {
            width: t.width,
            height: t.height,
            rgba: &t.rgba,
            srgb,
            block: None,
        })
    }
    let rgba = [
        slot(mat.albedo.as_ref(), true),
        slot(mat.normal_map.as_ref(), false),
        slot(mat.occlusion_map.as_ref(), false),
        slot(mat.emissive_map.as_ref(), true),
    ];
    std::array::from_fn(|i| {
        // An image-identity slot is uploaded whole below; streaming it as well would keep
        // a second copy.
        if mat.images.get(i).is_some() {
            return None;
        }
        match crate::compressed_texture::usable(mat.compressed.as_ref(), i, features) {
            Some(block) => Some(crate::texture_stream::SlotSource {
                width: block.width,
                height: block.height,
                rgba: &[],
                srgb: crate::compressed_texture::slot_is_srgb(i),
                block: Some(block),
            }),
            None => rgba[i],
        }
    })
}

pub(super) fn keep_slot(
    bind: crate::texture_residency::SlotBind,
    textures: &mut Vec<(&'static str, wgpu::Texture)>,
    shares: &mut Vec<crate::texture_residency::ShareKey>,
) -> wgpu::TextureView {
    if let Some(fixed) = bind.fixed {
        textures.push(fixed);
    }
    if let Some(share) = bind.share {
        shares.push(share);
    }
    bind.view
}

/// Shrinks streamed sets to free at least `need` bytes in [`crate::texture_budget::plan_eviction`]
/// order (`protect` is the material growing, never a candidate); copies go into `encoder`
/// (created on first use). Rebuilds each shrunk material's bind group (on the texture table path,
/// rewrites its record's texture indices instead). Returns the bytes freed.
#[allow(clippy::too_many_arguments)] // the frame's streaming context
pub(super) fn evict_texture_levels(
    materials: &mut crate::key_hash::KeyMap<u64, MaterialEntry>,
    pipes: &Pipes,
    mut arena: Option<&mut crate::bindless::MaterialArena>,
    mut table: Option<&mut crate::texture_table::TextureTable>,
    d: &wgpu::Device,
    q: &crate::device::TrackedQueue,
    encoder: &mut Option<wgpu::CommandEncoder>,
    need: u64,
    protect: Option<u64>,
    in_scene: &crate::key_hash::KeyMap<u64, u32>,
    meshes: &[FrameMesh<'_>],
    policy: crate::TextureStreaming,
    now: u64,
) -> u64 {
    let _span = tracing::info_span!("texture_evict").entered();
    let candidates: Vec<_> = materials
        .iter()
        .filter(|(key, _)| Some(**key) != protect)
        .filter_map(|(key, entry)| entry.stream.as_ref()?.evict_candidate(*key, policy, now))
        .collect();
    let features = d.features();
    let mut freed = 0;
    for (key, skip) in crate::texture_budget::plan_eviction(candidates, need) {
        let Some(entry) = materials.get_mut(&key) else {
            continue;
        };
        let Some(stream) = entry.stream.as_mut() else {
            continue;
        };
        let before = stream.resident_bytes();
        let sources = in_scene
            .get(&key)
            .and_then(|&ix| meshes.get(ix as usize))
            .map(|mesh| stream_sources(mesh.material, features));
        let encoder = encoder.get_or_insert_with(|| {
            d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("texture eviction copies"),
            })
        });
        if let Some(new_views) = stream.shrink(d, q, encoder, skip, sources)
            && let Some(views) = entry.views.as_mut()
        {
            for (slot, view) in new_views.into_iter().enumerate() {
                if let Some(view) = view {
                    views[slot] = view;
                }
            }
            if let (Some(texture_table), Some(old)) = (table.as_deref_mut(), entry.table) {
                // Texture table path (ADR-863): the shrunk textures keep their indices
                // (ADR-1110); the record changes only for a slot that had to move.
                let indices = texture_table.swap_set(old, views);
                entry.table = Some(indices);
                if indices != old
                    && let (MaterialConsts::Slot(_, slot, _), Some(arena)) =
                        (&entry.consts, arena.as_deref_mut())
                {
                    arena.write_at(
                        q,
                        *slot,
                        size_of::<GpuMaterial>() as u64,
                        bytemuck::cast_slice(&indices.words()),
                    );
                }
            } else if let Some(resource) = entry.consts.binding(arena.as_deref())
                && let Some(bg) = crate::texture_stream::oom_scoped(d, "material bind group", || {
                    material_bind_group(d, pipes, resource, views)
                })
            {
                entry.bg = bg;
            }
        }
        let after = stream.resident_bytes();
        if after < before {
            regia_core::telemetry::add(regia_core::telemetry::Counter::TextureEvictions, 1);
        }
        freed += before.saturating_sub(after);
    }
    freed
}

/// Whether a cached graph block must be written again.
///
/// A live clock (`material_time` or foliage wind is set) owns slot 0, so a change that is only
/// that slot does not upload. Any later slot still does. An unset clock compares the whole block.
pub(super) fn graph_block_needs_upload(
    old: Option<&[[f32; 4]]>,
    new: &[[f32; 4]],
    uses_time: bool,
    clock_live: bool,
) -> bool {
    let Some(old) = old else {
        return true;
    };
    if old == new {
        return false;
    }
    if uses_time && clock_live && old.len() == new.len() && old[1..] == new[1..] {
        return false;
    }
    true
}

/// A standard material's bind group: its constants (`consts`: its own buffer, or its arena slot)
/// and `views` in bind order (base colour, normal, ORM, emissive, detail, detail normal, aux).
pub(super) fn material_bind_group(
    d: &wgpu::Device,
    pipes: &Pipes,
    consts: wgpu::BindingResource<'_>,
    views: &[wgpu::TextureView; 7],
) -> wgpu::BindGroup {
    crate::device::TrackedDevice::observe(d).create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("material bg"),
        layout: &pipes.mat_bgl,
        entries: &[
            binding(0, consts),
            texture_binding(1, &views[0]),
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(&pipes.sampler),
            },
            texture_binding(3, &views[1]),
            texture_binding(4, &views[2]),
            texture_binding(5, &views[3]),
            texture_binding(6, &views[4]),
            texture_binding(7, &views[5]),
            texture_binding(8, &views[6]),
        ],
    })
}

impl FrameRenderer {
    pub(super) fn ensure_material(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        key: u64,
        mat: &LabMaterial,
        now: u64,
    ) -> Result<(), RenderError> {
        let d = &crate::device::TrackedDevice::observe(d);
        let Some(pipes) = self.pipes.as_ref() else {
            return Ok(());
        };
        let mut params = pack_material(mat);
        if let Some(shading) = self.material_shading.get(&key) {
            params.shading = *shading;
        }
        // Subsurface scattering (`crate::subsurface`): the words only the `SUBSURFACE` variant
        // reads. The eye's block uses `shading_c`; it does not scatter (`Draw::subsurface`).
        let (subsurface_word, subsurface_block) = mat.subsurface.pack();
        if subsurface_word > 0.0 && params.shading[0][0] != crate::shading_model::MODEL_EYE {
            params.shading[0][2] = subsurface_word;
            params.shading[3] = subsurface_block;
        }
        // The normal slot's channel layout depends on what this device samples (a cooked
        // BC5 / ASTC normal map, or its RGBA8 decode). It shares `alpha.z` with the unlit
        // flag (bit 0) as bits 1-2; `transmit.w` is the refraction thickness.
        params.alpha[2] += 2.0
            * crate::compressed_texture::normal_layout(mat.compressed.as_ref(), d.features()).shader_code();
        // Bit 3: a cooked normal map. Its normal-variance roughness is already baked into the
        // cooked ORM mips (ADR-091), so the runtime Toksvig term must not add it again — also
        // when the device lacks BC and samples the RGBA8 CPU decode, whose filtered length
        // would otherwise count the same variance twice.
        if mat.compressed.as_ref().is_some_and(|maps| maps.normal.is_some()) {
            params.alpha[2] += 8.0;
        }
        let graph = mat.surface_graph.as_deref();
        let graph_key = graph.map(|g| g.key);
        // The graph block with the material clock in slot 0 (time, previous time) and the
        // scene wind beside it (`zw`, m/s on X and Z; ADR-2028).
        let graph_block = graph.map(|g| {
            let mut block = g.params;
            if g.uses_time {
                if let Some((now, prev)) = self.material_time {
                    block[0][0] = now;
                    block[0][1] = prev;
                }
                if let Some([x, z]) = self.foliage_wind {
                    block[0][2] = x;
                    block[0][3] = z;
                }
            }
            // The floating origin for graphs that read positions (ADR-2435).
            g.write_origin(&mut block, self.origin_offset);
            block
        });
        // A key that switches between graph and standard (or between graphs) changes the
        // uniform size and the texture set: rebuild the entry. Field borrows only: `pipes`
        // is already held, so this cannot go through [`Self::forget_material`].
        // So does an arena slot of an arena this renderer no longer holds.
        let arena = self.material_arena.as_ref();
        // Counted textures (ADR-8610): an ECS material keeps its key across a texture's hot
        // reload, so the entry compares the upload serials it was built with.
        let counted = crate::texture_residency::counted_stamp(mat);
        let counted_moved = self
            .materials
            .get(&key)
            .is_some_and(|entry| entry.counted != counted);
        let rebuild = counted_moved
            || self
                .materials
                .get(&key)
                .is_some_and(|entry| entry.graph_key != graph_key || entry.consts.foreign(arena));
        if counted_moved {
            self.counted_rebinds = self.counted_rebinds.wrapping_add(1);
        }
        if rebuild && let Some(entry) = self.materials.remove(&key) {
            // Release the GPU row first. `pinned` drops after that, so the allocation stays
            // reserved until both the cache entry and the material entry are gone.
            let MaterialEntry {
                consts,
                shares,
                pinned,
                table,
                ..
            } = entry;
            consts.release(self.material_arena.as_mut(), now);
            if let (Some(texture_table), Some(indices)) = (self.texture_table.as_mut(), table) {
                texture_table.release_set(indices);
            }
            for share in shares {
                self.shared_textures.release(share);
            }
            drop(pinned);
            // What the material draws changed under the same key (a counted texture's new
            // upload): the held-view key must miss too (`screen_reuse`).
            self.material_epoch = self.material_epoch.wrapping_add(1);
        }
        // Parameters-only change: rewrite the uniform, keep the bind group and
        // textures. Texture content is keyed by the caller's material key: the
        // runtime path shares immutable `Arc<CpuMaterial>`s, so a changed
        // texture arrives under a changed key. Content-hashing every texture
        // every frame would cost milliseconds on scenes like Sponza.
        // A shared image is keyed by the image instead, and a dynamic texture
        // writes into the resident one here, so neither needs a new bind group.
        // Touch borrows `shares` in place. The empty case (every material that does not opt
        // into sharing) allocates nothing.
        if let Some(entry) = self.materials.get(&key)
            && !entry.shares.is_empty()
        {
            self.shared_textures.touch(&entry.shares, now);
        }
        // A dynamic slot (a video picture) rewritten in place changes what the material draws
        // under the same key: like a clock-only graph change, it moves the epoch that keys the
        // static-screen reuse, or a held view kept last frame's screen passes or composite over
        // the new picture (ADR-9603).
        if self.materials.contains_key(&key)
            && crate::texture_residency::flush_material_dynamics(&mut self.shared_textures, q, mat)
        {
            self.material_epoch = self.material_epoch.wrapping_add(1);
        }
        if let Some(entry) = self.materials.get_mut(&key) {
            let mut params = params;
            if let Some(stream) = entry.stream.as_ref() {
                // The fade and the residency advance in `stream_materials`, after the draw
                // loop has measured this frame's need.
                params.detail[3] = stream.min_lod();
            }
            // Bytes first (one memcmp); the float compare decides only when they differ,
            // so the upload decisions match the plain `!=` (zero signs, NaNs).
            let changed =
                bytemuck::bytes_of(&entry.params) != bytemuck::bytes_of(&params) && entry.params != params;
            if changed {
                entry
                    .consts
                    .write(q, self.material_arena.as_mut(), bytemuck::bytes_of(&params));
                entry.params = params;
                self.material_epoch = self.material_epoch.wrapping_add(1);
            }
            // The per-slot sampler choice lives beside the table indices (ADR-2071).
            if let Some(indices) = entry.table.as_mut()
                && indices.1 != mat.samplers.bits()
            {
                indices.1 = mat.samplers.bits();
                let words = indices.words();
                if let (MaterialConsts::Slot(_, slot, _), Some(arena)) =
                    (&entry.consts, self.material_arena.as_mut())
                {
                    arena.write_at(
                        q,
                        *slot,
                        size_of::<GpuMaterial>() as u64,
                        bytemuck::cast_slice(&words),
                    );
                }
                self.material_epoch = self.material_epoch.wrapping_add(1);
            }
            // A graph material always owns its buffer (constants, then the graph block).
            // A live clock owns slot 0, so that slot alone does not upload (ADR-5160).
            if let Some(block) = graph_block {
                let uses_time = graph.is_some_and(|g| g.uses_time);
                let clock_live = self.material_time.is_some() || self.foliage_wind.is_some();
                // Against what the GPU holds: a slot 0 seen under a live clock was not written
                // (corr-render-frame-upscaler LOW 10).
                let needs = graph_block_needs_upload(
                    entry.graph_uploaded.as_ref().map(|stored| stored.as_slice()),
                    &block,
                    uses_time,
                    clock_live,
                );
                if needs && let MaterialConsts::Own(buf) = &entry.consts {
                    q.write_buffer(buf, size_of::<GpuMaterial>() as u64, bytemuck::cast_slice(&block));
                    entry.graph_uploaded = Some(block);
                    #[cfg(test)]
                    {
                        self.graph_block_writes = self.graph_block_writes.wrapping_add(1);
                    }
                    self.material_epoch = self.material_epoch.wrapping_add(1);
                } else if entry
                    .graph_params
                    .as_ref()
                    .is_some_and(|stored| stored[..] != block[..])
                {
                    // A clock-only change: the shaders read the frame clock, so nothing uploads,
                    // but what this material draws still moved. The epoch keys the static-screen
                    // reuse (`screen_reuse`): without the bump a held view under a moving clock
                    // kept last frame's AO, SSGI and SSR (or its whole composite) over swaying
                    // plants (ADR-5554).
                    self.material_epoch = self.material_epoch.wrapping_add(1);
                }
                if matches!(entry.consts, MaterialConsts::Own(_)) {
                    entry.graph_params = Some(block);
                }
            }
            entry.last_used = now;
            return Ok(());
        }
        // A material first seen on the frame path: its uniform, textures and bind group.
        let _span = tracing::info_span!("material_upload", graph = graph.is_some()).entered();
        let graph_bytes = if graph.is_some() {
            crate::surface_graph::GRAPH_PARAM_BYTES
        } else {
            0
        };
        // A standard material on the storage path takes an arena slot; graph materials, and
        // every material on the uniform path, own their buffer (`bindless`, ADR-316).
        let consts = if pipes.material_ssbo && graph.is_none() {
            // On the texture table path the record also carries the texture indices (ADR-863).
            let record = if pipes.table_bgl.is_some() {
                TABLE_RECORD_BYTES
            } else {
                size_of::<GpuMaterial>() as u64
            };
            let arena = self
                .material_arena
                .get_or_insert_with(|| crate::bindless::MaterialArena::new(d, record));
            let epoch = arena.epoch();
            let slot = arena.allocate(d, q, bytemuck::bytes_of(&params));
            if arena.epoch() != epoch {
                regroup_materials(&mut self.materials, arena, d, pipes, &mut self.record_group);
            }
            MaterialConsts::Slot(arena.id(), slot, arena.epoch())
        } else {
            let usage = if pipes.material_ssbo {
                wgpu::BufferUsages::STORAGE
            } else {
                wgpu::BufferUsages::UNIFORM
            };
            let buf = make_buffer(
                d,
                "material uniform",
                (size_of::<GpuMaterial>() + graph_bytes) as u64,
                usage,
            );
            q.write_buffer(&buf, 0, bytemuck::bytes_of(&params));
            MaterialConsts::Own(buf)
        };
        if let Some(graph) = graph {
            let MaterialConsts::Own(buf) = &consts else {
                return Err(RenderError::Shader(
                    "graph material without its own buffer".into(),
                ));
            };
            let block = graph_block.unwrap_or(graph.params);
            q.write_buffer(buf, size_of::<GpuMaterial>() as u64, bytemuck::cast_slice(&block));
            #[cfg(test)]
            {
                self.graph_block_writes = self.graph_block_writes.wrapping_add(1);
            }
            self.graph_cache.register(graph.key, &graph.code());
            let name = crate::texture_budget::material_name(mat);
            let env = crate::texture_residency::UploadEnv {
                features: d.features(),
                material_key: key,
                name: name.as_ref(),
                now,
                device: d,
                queue: q,
            };
            let mut textures = Vec::new();
            let mut shares = Vec::new();
            let views: Vec<wgpu::TextureView> = graph
                .textures
                .iter()
                .enumerate()
                .map(|(i, slot)| match slot {
                    Some(tex) => {
                        // RGBA8 keeps the full mip chain. A cooked or shared slot uploads once
                        // and is reused by every material that holds the same image.
                        let bind = crate::texture_residency::bind_graph_texture(
                            &mut self.shared_textures,
                            &env,
                            tex,
                            GRAPH_SLOT_NAMES[i],
                            &pipes.white_view,
                        );
                        keep_slot(bind, &mut textures, &mut shares)
                    }
                    None => pipes.white_view.clone(),
                })
                .collect();
            let bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("graph material bg"),
                layout: &pipes.graph_mat_bgl,
                entries: &[
                    binding(0, buf.as_entire_binding()),
                    texture_binding(1, &views[0]),
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&pipes.sampler),
                    },
                    texture_binding(3, &views[1]),
                    texture_binding(4, &views[2]),
                    texture_binding(5, &views[3]),
                    texture_binding(6, &views[4]),
                    texture_binding(7, &views[5]),
                    texture_binding(8, &views[6]),
                ],
            });
            self.cache_stats.material_uploads += 1;
            self.materials.insert(
                key,
                MaterialEntry {
                    consts,
                    bg,
                    params,
                    graph_key,
                    graph_params: Some(block),
                    graph_uploaded: Some(block),
                    stream: None,
                    views: None,
                    table: None,
                    dyn_offset: None,
                    fixed: textures,
                    shares,
                    pinned: crate::texture_residency::pin_material(mat),
                    name,
                    last_used: now,
                    texture_set: None,
                    film: None,
                    counted,
                },
            );
            return Ok(());
        }
        let mut textures = Vec::new();
        // Streamed materials upload their low mips now and the rest by need
        // ([`crate::texture_stream`]); a slot the stream could not take falls through.
        let features = d.features();
        let policy = self.stream_frame.policy;
        let sources = stream_sources(mat, features);
        let (stream, mut stream_views) = if policy.enabled && sources.iter().any(Option::is_some) {
            // Eager first use takes what the resident budget has room for; the stream pass
            // evicts and re-totals every frame.
            let headroom = policy
                .resident_budget_bytes()
                .saturating_sub(self.stream_frame.resident);
            let (stream, views) =
                crate::texture_stream::MaterialStream::new(d, q, policy, sources, headroom, now);
            self.stream_frame.resident += stream.resident_bytes();
            (Some(Box::new(stream)), views)
        } else {
            (None, [None, None, None, None])
        };
        // Pre-compressed slots of an unstreamed material upload whole (a streamed one's are
        // the stream's). A slot overridden by `LabMaterial::images` is bound below instead.
        for (slot, view) in stream_views.iter_mut().enumerate() {
            if mat.images.get(slot).is_some() {
                continue;
            }
            if view.is_none()
                && let Some(map) = crate::compressed_texture::usable(mat.compressed.as_ref(), slot, features)
            {
                let srgb = crate::compressed_texture::slot_is_srgb(slot);
                let (t, v) = map.upload(d, q, 0, srgb);
                textures.push((STREAM_SLOT_NAMES[slot], t));
                *view = Some(v);
            }
        }
        // A device that cannot sample a cooked block format gets its RGBA8 decode instead
        // (only where the material has no RGBA8 map of its own; not streamed).
        let rgba_present = [
            mat.albedo.is_some(),
            mat.normal_map.is_some(),
            mat.occlusion_map.is_some(),
            mat.emissive_map.is_some(),
        ];
        for (slot, view) in stream_views.iter_mut().enumerate() {
            if mat.images.get(slot).is_some() {
                continue;
            }
            if view.is_none()
                && let Some(chain) = crate::compressed_texture::fallback(
                    mat.compressed.as_ref(),
                    slot,
                    features,
                    rgba_present[slot],
                )
                && let Some((t, v)) =
                    upload_rgba8_chain(d, q, &chain, crate::compressed_texture::slot_is_srgb(slot))
            {
                textures.push((STREAM_SLOT_NAMES[slot], t));
                *view = Some(v);
            }
        }
        // Colour slots (base colour, emissive) are sRGB-encoded (glTF 2.0 § 3.9.2):
        // an sRGB view makes the sampler decode them. Data slots stay Unorm.
        // `images` wins for a slot; otherwise this is the path the lab has always used.
        let name = crate::texture_budget::material_name(mat);
        let env = crate::texture_residency::UploadEnv {
            features,
            material_key: key,
            name: name.as_ref(),
            now,
            device: d,
            queue: q,
        };
        let mut shares = Vec::new();
        let albedo_view = keep_slot(
            crate::texture_residency::bind_rgba_slot(
                &mut self.shared_textures,
                &env,
                crate::texture_residency::RgbaSource {
                    image: mat.images.albedo.as_ref(),
                    rgba: mat.albedo.as_ref(),
                    srgb: true,
                    slot: "base_color",
                    fallback: &pipes.white_view,
                },
                stream_views[0].take(),
            ),
            &mut textures,
            &mut shares,
        );
        let normal_view = keep_slot(
            crate::texture_residency::bind_rgba_slot(
                &mut self.shared_textures,
                &env,
                crate::texture_residency::RgbaSource {
                    image: mat.images.normal.as_ref(),
                    rgba: mat.normal_map.as_ref(),
                    srgb: false,
                    slot: "normal",
                    fallback: &pipes.flat_view,
                },
                stream_views[1].take(),
            ),
            &mut textures,
            &mut shares,
        );
        let occ_view = keep_slot(
            crate::texture_residency::bind_rgba_slot(
                &mut self.shared_textures,
                &env,
                crate::texture_residency::RgbaSource {
                    image: mat.images.occlusion.as_ref(),
                    rgba: mat.occlusion_map.as_ref(),
                    srgb: false,
                    slot: "orm",
                    fallback: &pipes.white_view,
                },
                stream_views[2].take(),
            ),
            &mut textures,
            &mut shares,
        );
        let em_view = keep_slot(
            crate::texture_residency::bind_rgba_slot(
                &mut self.shared_textures,
                &env,
                crate::texture_residency::RgbaSource {
                    image: mat.images.emissive.as_ref(),
                    rgba: mat.emissive_map.as_ref(),
                    srgb: true,
                    slot: "emissive",
                    fallback: &pipes.white_view,
                },
                stream_views[3].take(),
            ),
            &mut textures,
            &mut shares,
        );
        let detail_compressed = mat.compressed.as_ref().and_then(|maps| maps.detail.as_ref());
        let detail_usable = detail_compressed.is_some_and(|map| map.supported(features));
        let detail_image = if mat.images.detail.is_some() || detail_usable {
            None
        } else {
            crate::hard_surface::pack_detail_image(mat.detail_albedo.as_ref(), mat.detail_roughness.as_ref())
        };
        let detail_view = keep_slot(
            crate::texture_residency::bind_packed_slot(
                &mut self.shared_textures,
                &env,
                crate::texture_residency::PackedSource {
                    image: mat.images.detail.as_ref(),
                    compressed: detail_compressed,
                    rgba: detail_image.as_ref(),
                    slot: "detail",
                    fallback: &pipes.mid_view,
                },
            ),
            &mut textures,
            &mut shares,
        );
        let detail_n_compressed = mat
            .compressed
            .as_ref()
            .and_then(|maps| maps.detail_normal.as_ref());
        let detail_n_usable = detail_n_compressed.is_some_and(|map| map.supported(features));
        let detail_n = if mat.images.detail_normal.is_some() || detail_n_usable {
            None
        } else {
            crate::hard_surface::pack_detail_normal(mat.detail_normal.as_ref(), mat.detail_mask.as_ref())
        };
        let detail_n_view = keep_slot(
            crate::texture_residency::bind_packed_slot(
                &mut self.shared_textures,
                &env,
                crate::texture_residency::PackedSource {
                    image: mat.images.detail_normal.as_ref(),
                    compressed: detail_n_compressed,
                    rgba: detail_n.as_ref(),
                    slot: "detail_normal",
                    fallback: &pipes.flat_view,
                },
            ),
            &mut textures,
            &mut shares,
        );
        let aux_compressed = mat.compressed.as_ref().and_then(|maps| maps.aux.as_ref());
        let aux_usable = aux_compressed.is_some_and(|map| map.supported(features));
        let aux_image = if mat.images.aux.is_some() || aux_usable {
            None
        } else {
            crate::hard_surface::pack_aux(mat.height_map.as_ref(), mat.curvature_map.as_ref())
        };
        let aux_view = keep_slot(
            crate::texture_residency::bind_packed_slot(
                &mut self.shared_textures,
                &env,
                crate::texture_residency::PackedSource {
                    image: mat.images.aux.as_ref(),
                    compressed: aux_compressed,
                    rgba: aux_image.as_ref(),
                    slot: "aux",
                    fallback: &pipes.aux_view,
                },
            ),
            &mut textures,
            &mut shares,
        );
        let views = [
            albedo_view,
            normal_view,
            occ_view,
            em_view,
            detail_view,
            detail_n_view,
            aux_view,
        ];
        let texture_set = Some(crate::texture_sets::texture_set_key(&views));
        // Texture table path (ADR-863): the views take table indices, the record names them, and
        // the material draws through the one shared record group at its slot's offset.
        if pipes.table_bgl.is_some() && self.texture_table.is_none() {
            self.texture_table = crate::texture_table::TextureTable::new(
                d,
                [
                    &pipes.white_view,
                    &pipes.flat_view,
                    &pipes.mid_view,
                    &pipes.aux_view,
                ],
                &pipes.sampler,
                self.material_anisotropy(),
            );
        }
        let table = match (self.texture_table.as_mut(), &consts) {
            (Some(texture_table), MaterialConsts::Slot(_, slot, _)) if pipes.table_bgl.is_some() => {
                let mut indices = texture_table.acquire_set(&views);
                indices.1 = mat.samplers.bits();
                if let Some(arena) = self.material_arena.as_mut() {
                    arena.write_at(
                        q,
                        *slot,
                        size_of::<GpuMaterial>() as u64,
                        bytemuck::cast_slice(&indices.words()),
                    );
                }
                Some(indices)
            }
            _ => None,
        };
        let (bg, dyn_offset) = match (table.as_ref(), &consts, self.material_arena.as_ref()) {
            (Some(_), MaterialConsts::Slot(_, slot, _), Some(arena)) => {
                let Some(offset) = arena.dynamic_offset(*slot) else {
                    return Err(RenderError::Shader(
                        "material arena slot past a dynamic offset".into(),
                    ));
                };
                (self.record_group.get(arena, d, pipes), Some(offset))
            }
            // The table path's material layout holds the record only: a standard material
            // without table indices cannot bind on it.
            _ if pipes.table_bgl.is_some() => {
                return Err(RenderError::Shader(
                    "texture table path: material without table indices".into(),
                ));
            }
            _ => {
                let Some(resource) = consts.binding(self.material_arena.as_ref()) else {
                    return Err(RenderError::Shader("material arena slot missing".into()));
                };
                (material_bind_group(d, pipes, resource, &views), None)
            }
        };
        // An arena slot keeps its views too: the arena growing rebuilds the group.
        let keep_views = stream.is_some() || matches!(consts, MaterialConsts::Slot(..));
        let views = keep_views.then(|| Box::new(views));
        self.may_stream |= stream.is_some();
        self.cache_stats.material_uploads += 1;
        self.materials.insert(
            key,
            MaterialEntry {
                consts,
                bg,
                params,
                graph_key: None,
                graph_params: None,
                graph_uploaded: None,
                stream,
                views,
                table,
                dyn_offset,
                fixed: textures,
                shares,
                pinned: crate::texture_residency::pin_material(mat),
                name,
                last_used: now,
                texture_set,
                film: None,
                counted,
            },
        );
        Ok(())
    }

    /// Records the texels a visible `mesh` needs from its streamed material this frame.
    pub(super) fn note_stream_need(
        &mut self,
        mesh: &FrameMesh<'_>,
        eye: [f32; 3],
        near: f32,
        ppr: f32,
        now: u64,
    ) {
        let Some(stream) = self
            .materials
            .get_mut(&mesh.material_key)
            .and_then(|entry| entry.stream.as_mut())
        else {
            return;
        };
        let need = match self.meshes.get(&mesh.mesh_key) {
            Some(MeshEntry {
                bounds: Some(bounds),
                uv_span,
                ..
            }) => {
                let model = regia_core::glam::Mat4::from_cols_array_2d(&mesh.model);
                let scale = (model
                    .x_axis
                    .length_squared()
                    .max(model.y_axis.length_squared())
                    .max(model.z_axis.length_squared()))
                .sqrt();
                let center = model.transform_point3(regia_core::glam::Vec3::from_array(bounds.center));
                let distance = (center.distance(regia_core::glam::Vec3::from_array(eye))
                    - bounds.radius * scale)
                    .max(near);
                crate::texture_stream::needed_px(uv_span * scale, distance, ppr)
            }
            // Skinned meshes (no static bound) keep their maps at the cap.
            _ => f32::MAX,
        };
        stream.note_need(need, now);
    }

    /// One frame of material texture streaming, after the draw loop measured the need:
    /// over the resident budget, the least-recently-seen sets shrink first; then pending sets,
    /// then the materials whose need most exceeds what is resident, all inside the frame's
    /// upload budget — a set grows only when the resident budget has room for it (after
    /// evicting older sets once a frame if not). Completed sets swap in (new bind group) and fade.
    #[tracing::instrument(skip_all)]
    pub(super) fn stream_materials(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        scene: &FrameScene<'_>,
        now: u64,
    ) {
        let Some(pipes) = self.pipes.as_ref() else {
            return;
        };
        // Each streamed material of this frame once. The slots keep the order list and the
        // in-scene map; a mesh index stands in for the material so nothing here borrows the scene
        // for the life of the slot.
        let mut order = self.scratch.stream_order.take();
        let mut in_scene = self.scratch.stream_in_scene.take();
        let mut previous = None;
        for (index, mesh) in scene.meshes.iter().enumerate() {
            let Ok(mesh_ix) = u32::try_from(index) else {
                continue;
            };
            // Draws of one material are usually adjacent: skip the map for repeats.
            if previous == Some(mesh.material_key) || in_scene.insert(mesh.material_key, mesh_ix).is_some() {
                previous = Some(mesh.material_key);
                continue;
            }
            previous = Some(mesh.material_key);
            if let Some(stream) = self
                .materials
                .get(&mesh.material_key)
                .and_then(|e| e.stream.as_ref())
            {
                order.push((mesh.material_key, mesh_ix, stream.priority(now)));
            }
        }
        order.sort_by(|a, b| b.2.total_cmp(&a.2));
        let policy = self.stream_frame.policy;
        let features = d.features();
        let started = std::time::Instant::now();
        let limit = policy.resident_budget_bytes();
        // One true sum. `total` is the budget cursor (a predicted eviction or growth moves it).
        // `actual` is what `stream_frame.resident` stores: the same sum, minus bytes an eviction
        // freed, plus the real change each `step` made. The cursor is not the resident total.
        #[cfg(test)]
        {
            self.resident_scans = self.resident_scans.wrapping_add(1);
        }
        let mut actual: u64 = self
            .materials
            .values()
            .filter_map(|e| e.stream.as_ref())
            .map(|s| s.resident_bytes() + s.pending_bytes())
            .sum();
        let mut total = actual;
        let mut encoder: Option<wgpu::CommandEncoder> = None;
        // Only the main view's pass takes it: a texture view's scene would make the main view's
        // materials look out of view.
        if self.view_pass.is_none() && self.stream_frame.take_pressure_drop() {
            // Out of memory again soon after a trim (`gpu_health`): free one level's worth of
            // every set, least recently seen first; a set in view keeps one level under its need.
            let one_level: u64 = self
                .materials
                .values()
                .filter_map(|e| e.stream.as_ref())
                .map(|s| s.one_level_gain())
                .sum();
            if one_level > 0 {
                let freed = evict_texture_levels(
                    &mut self.materials,
                    pipes,
                    self.material_arena.as_mut(),
                    self.texture_table.as_mut(),
                    d,
                    q,
                    &mut encoder,
                    one_level,
                    None,
                    &in_scene,
                    &scene.meshes,
                    policy,
                    now,
                );
                total -= freed;
                actual -= freed;
            }
        }
        if total > limit {
            let freed = evict_texture_levels(
                &mut self.materials,
                pipes,
                self.material_arena.as_mut(),
                self.texture_table.as_mut(),
                d,
                q,
                &mut encoder,
                total - limit,
                None,
                &in_scene,
                &scene.meshes,
                policy,
                now,
            );
            total -= freed;
            actual -= freed;
        }
        let mut evicted_for_growth = false;
        for &(key, mesh_ix, _) in &order {
            let Some(mat) = scene.meshes.get(mesh_ix as usize).map(|mesh| mesh.material) else {
                continue;
            };
            // A streamed texture the device could not allocate stops this frame's streaming.
            self.stream_frame.halt_after_failure();
            let growth = self
                .materials
                .get(&key)
                .and_then(|e| e.stream.as_ref())
                .and_then(|s| {
                    s.growth(policy, now)
                        .map(|after| after.saturating_sub(s.resident_bytes()))
                });
            let mut may_grow = false;
            if let Some(delta) = growth {
                if total + delta > limit && !evicted_for_growth {
                    // One eviction round a frame on behalf of growth (a visible material's
                    // need outranks older or over-resident sets).
                    evicted_for_growth = true;
                    let freed = evict_texture_levels(
                        &mut self.materials,
                        pipes,
                        self.material_arena.as_mut(),
                        self.texture_table.as_mut(),
                        d,
                        q,
                        &mut encoder,
                        total + delta - limit,
                        Some(key),
                        &in_scene,
                        &scene.meshes,
                        policy,
                        now,
                    );
                    total -= freed;
                    actual -= freed;
                }
                may_grow = total.saturating_add(delta) <= limit;
                if may_grow {
                    total += delta;
                }
            }
            let before = self
                .materials
                .get(&key)
                .and_then(|e| e.stream.as_ref())
                .map(|s| s.resident_bytes() + s.pending_bytes())
                .unwrap_or(0);
            let Some(entry) = self.materials.get_mut(&key) else {
                continue;
            };
            let Some(stream) = entry.stream.as_mut() else {
                continue;
            };
            let outcome = stream.step(
                d,
                q,
                policy,
                stream_sources(mat, features),
                &mut self.stream_frame.budget,
                may_grow,
                now,
            );
            if let (Some(new_views), Some(views)) = (outcome.views, entry.views.as_mut()) {
                for (slot, view) in new_views.into_iter().enumerate() {
                    if let Some(view) = view {
                        views[slot] = view;
                    }
                }
                if let (Some(texture_table), Some(old)) = (self.texture_table.as_mut(), entry.table) {
                    // Texture table path (ADR-863): the group binds the record and stays. A
                    // swapped texture keeps its table index (ADR-1110), so the record changes
                    // only for a slot that had to move.
                    let indices = texture_table.swap_set(old, views);
                    entry.table = Some(indices);
                    if indices != old
                        && let (MaterialConsts::Slot(_, slot, _), Some(arena)) =
                            (&entry.consts, self.material_arena.as_mut())
                    {
                        arena.write_at(
                            q,
                            *slot,
                            size_of::<GpuMaterial>() as u64,
                            bytemuck::cast_slice(&indices.words()),
                        );
                    }
                }
                // A bind group the device cannot allocate keeps the old one (its views live on).
                else if let Some(resource) = entry.consts.binding(self.material_arena.as_ref())
                    && let Some(bg) = crate::texture_stream::oom_scoped(d, "material bind group", || {
                        material_bind_group(d, pipes, resource, views)
                    })
                {
                    entry.bg = bg;
                }
                self.stream_frame.swapped += 1;
            }
            let min_lod = stream.min_lod();
            if entry.params.detail[3] != min_lod {
                entry.params.detail[3] = min_lod;
                entry
                    .consts
                    .write(q, self.material_arena.as_mut(), bytemuck::bytes_of(&entry.params));
            }
            let after = stream.resident_bytes() + stream.pending_bytes();
            actual = actual.saturating_sub(before).saturating_add(after);
        }
        if let Some(encoder) = encoder {
            q.submit([encoder.finish()]);
        }
        self.stream_frame.resident = actual;
        self.stream_frame.measure(started.elapsed());
        self.scratch.stream_order.give(order);
        self.scratch.stream_in_scene.give(in_scene);
    }

    /// Resident budget of streamed material textures under the current policy, bytes
    /// (`u64::MAX` with streaming off). Cheap: no iteration (telemetry reads it).
    #[must_use]
    pub fn texture_budget_bytes(&self) -> u64 {
        let policy = self.stream_frame.policy;
        if policy.enabled {
            policy.resident_budget_bytes()
        } else {
            u64::MAX
        }
    }

    /// Whether this frame's streaming is held a mip lower ([`crate::gpu_health::texture_mips_held`]).
    #[must_use]
    pub fn texture_streaming_held(&self) -> bool {
        self.stream_frame.held
    }

    /// Whether texture streaming is backing off after a streamed texture could not be allocated
    /// ([`crate::texture_stream::STREAM_OOM_BACKOFF`]).
    #[must_use]
    pub fn texture_streaming_backing_off(&self) -> bool {
        self.stream_frame.backing_off()
    }

    /// Whether this renderer's adapter gets the integrated-GPU streaming budget
    /// ([`crate::low_memory::integrated_limits_apply`]; `false` before the first frame).
    #[must_use]
    pub fn integrated_gpu_limits(&self) -> bool {
        self.adapter_integrated
    }

    /// Material texture streaming totals ([`crate::texture_stream`]).
    #[must_use]
    pub fn stream_report(&self) -> crate::texture_stream::StreamReport {
        let mut report = crate::texture_stream::StreamReport {
            uploaded_bytes: self.stream_frame.uploaded(),
            swapped: self.stream_frame.swapped,
            pass_us: self.stream_frame.pass_us,
            ..Default::default()
        };
        for stream in self.materials.values().filter_map(|e| e.stream.as_ref()) {
            stream.report(&mut report);
        }
        report
    }

    /// Every material texture this renderer holds — streamed (under the budget) or uploaded
    /// whole — with its authored and resident size, format, GPU and CPU bytes, largest first,
    /// plus the resident budget and the totals. The agent method `assets.texture_memory`
    /// returns it; [`crate::TextureMemoryReport::problems`] is what `render.problems` adds.
    #[must_use]
    pub fn texture_memory(&self) -> crate::TextureMemoryReport {
        let _span = tracing::info_span!("texture_memory").entered();
        let policy = self.stream_frame.policy;
        let mut report = crate::TextureMemoryReport {
            frame: self.frame_index,
            streaming: policy.enabled,
            budget_bytes: policy.resident_budget_bytes(),
            wrong_platform: crate::texture_budget::wrong_platform_textures(),
            ..Default::default()
        };
        for (key, entry) in &self.materials {
            let name = entry.name.as_deref().map(str::to_string);
            let last_seen = entry.stream.as_ref().map_or(entry.last_used, |s| s.last_seen());
            if let Some(stream) = entry.stream.as_ref() {
                for t in stream.textures() {
                    report.textures.push(crate::TextureMemoryEntry {
                        material_key: *key,
                        name: name.clone(),
                        slot: STREAM_SLOT_NAMES[t.slot],
                        width: t.full.0,
                        height: t.full.1,
                        resident_width: t.resident.0,
                        resident_height: t.resident.1,
                        format: format!("{:?}", t.format),
                        compressed: crate::texture_budget::is_compressed(t.format),
                        streamed: true,
                        gpu_bytes: t.gpu_bytes,
                        cpu_bytes: t.cpu_bytes,
                        last_seen_frame: last_seen,
                    });
                }
            }
            for (slot, texture) in &entry.fixed {
                let size = texture.size();
                report.textures.push(crate::TextureMemoryEntry {
                    material_key: *key,
                    name: name.clone(),
                    slot,
                    width: size.width,
                    height: size.height,
                    resident_width: size.width,
                    resident_height: size.height,
                    format: format!("{:?}", texture.format()),
                    compressed: crate::texture_budget::is_compressed(texture.format()),
                    streamed: false,
                    gpu_bytes: crate::texture_budget::texture_bytes(
                        texture.format(),
                        size.width,
                        size.height,
                        texture.mip_level_count(),
                    ),
                    cpu_bytes: 0,
                    last_seen_frame: last_seen,
                });
            }
        }
        self.shared_textures.append_memory(&mut report);
        report.finish();
        report
    }

    /// The totals of the texture report the renderer last built for its budget check (every
    /// 60 frames and on the first): resident bytes against the budget
    /// without building a [`Self::texture_memory`] report. Free to read every frame (the
    /// performance overlay shows it); `frame` says which frame it describes.
    #[must_use]
    pub fn texture_memory_summary(&self) -> crate::texture_budget::TextureMemorySummary {
        self.texture_summary
    }

    /// Refreshes the texture budget problems ([`Self::texture_memory`]) every
    /// [`TEXTURE_PROBLEMS_EVERY`] frames (and on the first), so the frame path does not build the
    /// report each frame.
    pub(super) fn refresh_texture_problems(&mut self) {
        if self.frame_index > 1 && !self.frame_index.is_multiple_of(TEXTURE_PROBLEMS_EVERY) {
            return;
        }
        let report = self.texture_memory();
        self.texture_summary = report.summary();
        let mut problems = report.problems();
        // Texture slots past a full texture table (bindless level 3, ADR-863).
        if let Some(problem) = self
            .texture_table_stats()
            .as_ref()
            .and_then(crate::texture_table::table_problem)
        {
            problems.push(problem);
        }
        if !problems.is_empty() && problems != self.texture_problems {
            for problem in &problems {
                tracing::warn!(target: "regia_render", code = problem.code, "{}", problem.message);
            }
        }
        self.texture_problems = problems;
    }
}

#[cfg(test)]
mod tests {
    use super::graph_block_needs_upload;

    #[test]
    fn a_live_clock_skips_a_slot0_only_change() {
        let old = [[0.0; 4]; 16];
        let mut slot0 = old;
        slot0[0][0] = 1.5;
        assert!(
            !graph_block_needs_upload(Some(&old), &slot0, true, true),
            "a live clock owns slot 0"
        );
        assert!(
            graph_block_needs_upload(Some(&old), &slot0, true, false),
            "an unset clock still uploads slot 0"
        );
        assert!(
            graph_block_needs_upload(Some(&old), &slot0, false, true),
            "a graph that does not read time still uploads its block"
        );
        let mut later = slot0;
        later[1][2] = 4.0;
        assert!(
            graph_block_needs_upload(Some(&old), &later, true, true),
            "a later slot still uploads"
        );
        assert!(!graph_block_needs_upload(Some(&old), &old, true, true));
        assert!(graph_block_needs_upload(None, &slot0, true, true));
    }
}
