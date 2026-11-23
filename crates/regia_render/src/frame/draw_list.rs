// SPDX-License-Identifier: MPL-2.0
//! The draw list of `FrameRenderer::render_into` (ADR-1552 split, line m): the frame's
//! material-graph pipeline sets, every submitted mesh's mesh, material and object entries and
//! its [`Draw`], the per-object uploads, and the opaque, blended and shadow-caster index lists.
//! Moved out of `frame.rs` unchanged. It creates entries (`ensure_mesh`, `ensure_material`,
//! `ensure_object`), so it is a `&mut self` method.

use super::*;

/// Tag of the standard twin a graph material draws with on the table path while its set builds.
const PENDING_GRAPH_TWIN_TAG: u64 = 0x7A1E_6D2B_0C94_53F1;

/// What the draw list is built from: the frame's view, target format and switches.
pub(super) struct DrawListInputs<'a> {
    /// The frame's scene (meshes, eye, quality).
    pub(super) scene: &'a FrameScene<'a>,
    /// The frame renders into a texture view (`view_pass`), not the main view.
    pub(super) view_pass: bool,
    /// The frame renders linear HDR (post or TAAU on).
    pub(super) linear_hdr: bool,
    /// The target is not sRGB: the forward pass encodes.
    pub(super) encode_target: bool,
    /// The color format the passes render into.
    pub(super) color_format: wgpu::TextureFormat,
    /// Instanced (storage-object) draws are on.
    pub(super) gpu_objects: bool,
    /// MSAA sample count before the per-frame fallbacks.
    pub(super) samples_early: u32,
    /// A shadow diagnostic frame.
    pub(super) shadow_debug: bool,
    /// Temporal AA/upscale is on.
    pub(super) taau_on: bool,
    /// Output height in pixels.
    pub(super) height: u32,
    /// Vertical field of view (radians).
    pub(super) fov_y: f32,
    /// The unjittered view frustum.
    pub(super) frustum: &'a Frustum,
    /// The view matrix.
    pub(super) view: regia_core::glam::Mat4,
    /// The unjittered view-projection (the masked occlusion cull's matrix, ADR-10072).
    pub(super) unjittered_vp: regia_core::glam::Mat4,
    /// Per non-empty mesh: joint base, model, a folded one-joint rigid skin. Returned to the
    /// scratch lists here.
    pub(super) spans: Vec<scratch::SkinSpan>,
    /// How many one-joint rigid skins were folded.
    pub(super) rigid_skin_folded: usize,
    /// The frame clock.
    pub(super) now: u64,
    /// The ray-traced sun's forward module this frame ([`PipelineKey::sun_rt`]).
    pub(super) sun_rt: bool,
}

/// The frame's draws and the lists the passes read.
pub(super) struct DrawList {
    /// Material-graph pipeline sets by `(code key, double-sided in a culled frame)`.
    pub(super) graph_sets: HashMap<(u64, bool), std::sync::Arc<crate::graph_pipelines::GraphPipelineSet>>,
    /// The visibility buffer draws this frame's rigid opaque meshes.
    pub(super) vis_frame: bool,
    /// The visibility-buffer sets cast cluster shadows this frame.
    pub(super) vis_shadow_state: bool,
    /// The snapped eye persistent local casters pick their cascades from.
    pub(super) vis_set_eye: regia_core::glam::Vec3,
    /// One draw per non-empty mesh, in submission order.
    pub(super) draws: Vec<Draw>,
    /// The material texture table path is in use (ADR-863).
    pub(super) table_path: bool,
    /// Masked instances whose material cannot take the mask.
    pub(super) mask_unsupported: usize,
    /// Visible opaque draws the CPU and GPU-driven loops draw.
    pub(super) opaque_ix: Vec<usize>,
    /// Visible blended draws, sorted back to front.
    pub(super) blend_ix: Vec<usize>,
    /// Shadow casters, culled ones included.
    pub(super) shadow_ix: Vec<usize>,
    /// The material texture table's group, when the table is in use.
    pub(super) texture_table_group: Option<wgpu::BindGroup>,
}

impl FrameRenderer {
    /// Builds the frame's draw list: graph pipeline sets, entries, draws, object uploads and
    /// the index lists. Also records the frame's draw stats and texture problems.
    pub(super) fn build_draw_list(
        &mut self,
        device: &RenderDevice,
        d: &crate::TrackedDevice,
        q: &crate::device::TrackedQueue,
        inputs: DrawListInputs<'_>,
    ) -> Result<DrawList, RenderError> {
        let DrawListInputs {
            scene,
            view_pass,
            linear_hdr,
            encode_target,
            color_format,
            gpu_objects,
            samples_early,
            shadow_debug,
            taau_on,
            height,
            fov_y,
            frustum,
            view,
            unjittered_vp,
            spans,
            rigid_skin_folded,
            now,
            sun_rt,
        } = inputs;
        // ---- Material-graph pipeline sets for this target (`graph_pipelines`) ----
        self.graph_cache.poll();
        // The load budget's upload plan (ADR-7884): which missing meshes this frame uploads.
        self.plan_mesh_uploads(scene);
        self.ensure_graph_layouts(d)?;
        let graph_target = crate::graph_pipelines::GraphTarget {
            format: color_format,
            encode: if linear_hdr { false } else { encode_target },
            double_sided: scene.double_sided,
            samples: if scene.quality.msaa_samples >= 4 { 4 } else { 1 },
            instanced: gpu_objects,
            sun_rt,
        };
        // `(code key, material is double-sided in a culled frame)`. One code used both ways
        // needs both sets: the cull-none set is a different [`crate::GraphTarget`].
        let mut graph_sets: HashMap<(u64, bool), std::sync::Arc<crate::graph_pipelines::GraphPipelineSet>> =
            HashMap::new();

        // ---- Mesh/material/object entries ----
        // A Mobile lite tier this frame (ADR-1874): its opaque draws have no film.
        let lite_frame =
            crate::forward_lite::frame_tier(scene.quality.forward_lite, self.forward_lite_override)
                != crate::forward_lite::ForwardLite::Off;
        // Output pixels per radian of the vertical field: texture streaming's need scale.
        let stream_ppr = height as f32 / (2.0 * (fov_y * 0.5).tan()).max(1e-6);
        // The visibility buffer replaces the prepass + forward draws of rigid opaque meshes. It
        // needs the single-sample HDR frame with a depth prepass (its depth joins the prepass).
        let vis_frame = !view_pass
            && scene.quality.visibility_buffer
            && linear_hdr
            && samples_early == 1
            && scene.depth_prepass
            && !shadow_debug
            && self.visbuffer.has_sources()
            && crate::visbuffer::VisBufferPass::supported(d)
            && self
                .visbuffer
                .select_frame(scene.quality.visibility_path.select, || {
                    visbuffer_api::dense_scene_count(&scene.meshes)
                });
        if !view_pass {
            if vis_frame {
                // The arena's caps follow the device before this frame's meshes take ranges, and
                // `begin_frame` rebuilds masked sets knowing the device has the table (ADR-3915).
                self.visbuffer.clamp_to_device(d);
            }
            self.visbuffer
                .note_path(scene.quality.visibility_buffer, scene.quality.visibility_path);
            // Cluster residency follows the profile unless a runtime override is set (ADR-3118);
            // before `begin_frame`, which takes the frame's upload allowance from it.
            self.visbuffer
                .set_profile_residency(scene.quality.cluster_residency);
            self.visbuffer.begin_frame();
            // The clock and the wind the forward passes write into graph slot 0 (ADR-3916).
            self.visbuffer
                .set_sway_clock(self.material_time, self.foliage_wind);
            self.visbuffer.set_active(vis_frame);
        }
        if vis_frame {
            // Persistent sets' materials stay resident while the sets draw.
            for (key, material) in self.visbuffer.set_materials() {
                self.ensure_material(d, q, key, &material, now)?;
            }
        }
        // Cached shadow maps hold the sets' shadows (or not): a set change, or the cluster
        // shadows switching on or off, redraws them once.
        let vis_shadow_state = vis_frame && self.visbuffer.settings.cluster_shadows;
        if (!view_pass && self.visbuffer.take_sets_changed()) || vis_shadow_state != self.vis_shadow_last {
            self.csm_valid = false;
            self.atlas_valid = false;
        }
        self.vis_shadow_last = vis_shadow_state;
        // Persistent `ShadowCast::Local` instances pick their cascades on the GPU from an eye
        // snapped to a grid; crossing a cell changes the choice, so cached cascades are redrawn.
        let vis_set_eye = VIS_SET_EYE_SNAP_M
            * regia_core::glam::Vec3::from_array(scene.eye)
                .div_euclid(regia_core::glam::Vec3::splat(VIS_SET_EYE_SNAP_M))
            + regia_core::glam::Vec3::splat(VIS_SET_EYE_SNAP_M * 0.5);
        if vis_shadow_state && self.visbuffer.gpu_masked() && self.vis_set_eye != Some(vis_set_eye.to_array())
        {
            self.csm_valid = false;
        }
        self.vis_set_eye = Some(vis_set_eye.to_array());
        // The previous frame's buffer, emptied. `reserve` grows it once; a steady scene fits.
        let mut draws = self.scratch.draws.take();
        draws.reserve(scene.meshes.len());
        // The texture table path (ADR-863): the mask-decal and glass variants are standard
        // pipelines over the record-only material layout, which a graph material's own group does
        // not fit, so graph draws go without them there. (Glass instances apply to standard
        // materials only anyway.) Graph translucent casters take their own caster variant.
        let table_path = crate::texture_table::device_support(d).capacity().is_some();
        let canopy_cheap_setting = self.canopy_glass().shading == crate::glass::CanopyShading::Cheap;
        let mut canopy_draws = CanopyDraws::default();
        let mut glass_index = self.scratch.glass_index.take();
        glass_index.extend(
            self.glass
                .iter()
                .enumerate()
                .map(|(index, instance)| (instance.object_key, index)),
        );
        self.scalar_pass.begin_frame();
        let mut culled = 0usize;
        // Masked instances (`mask_decals_gpu`); materials that cannot take the feature are counted.
        self.mask_decals.begin_frame();
        let any_masked = !self.mask_decals.is_empty();
        // Lightmap receivers (`lightmap_forward`, ADR-1623); draws the lightmap cannot shade are
        // counted.
        let lightmap_lite = self.frame_forward_tier(scene.quality.forward_lite);
        self.lightmap_forward
            .begin_frame(scene.quality.lightmap_tier, lightmap_lite);
        let any_lightmapped = self.lightmap_forward.active();
        // Thin film materials (`thin_film_gpu`, ADR-1977); draws the variant cannot shade are
        // counted.
        self.thin_film.begin_frame();
        self.subsurface.begin_frame();
        let mut mask_unsupported = 0usize;
        let mut zero_key_moving = 0usize;
        // One skin span per drawn mesh (CPU vertices, a counted mesh adopted at the start of the
        // frame, ADR-7284, or a resident draw whose host dropped its CPU copy, `host_geometry`,
        // ADR-7891), in scene order: the same rule as `pack_skin_palette`, so the spans line up.
        let mut frame_spans = spans.iter();
        for (index, submitted) in scene.meshes.iter().enumerate() {
            if !self.frame_draws_mesh(submitted) {
                continue;
            }
            let Some(&(joint_base, model, folded)) = frame_spans.next() else {
                break;
            };
            // Key 0 is ephemeral: a slot of its own for this mesh, uploaded afresh (several
            // zero-key meshes in one frame must not share one cache entry).
            let ephemeral = |key: u64| {
                if key == 0 {
                    EPHEMERAL_KEY | index as u64
                } else {
                    key
                }
            };
            // A dithered draw (LOD cross-fade) uses its own mesh key: same vertices, its own
            // batch, the alpha-test pipeline.
            let mesh_key = ephemeral(submitted.mesh_key);
            let mesh_key = if submitted.lod_dither == LOD_DITHER_NONE {
                mesh_key
            } else {
                mesh_key ^ LOD_DITHER_MESH_TAG
            };
            let keyed = FrameMesh {
                mesh_key,
                material_key: ephemeral(submitted.material_key),
                object_key: ephemeral(submitted.object_key),
                ..*submitted
            };
            // Past the load budget's upload cap (ADR-7884): drawn once a later frame uploads it.
            if submitted.mesh_key != 0 && self.defer_mesh_draw(&keyed) {
                continue;
            }
            // Morph targets (Tincal B1, `morph_gpu`): a morphed instance draws its own deformed
            // copy of the mesh, under a key of its own, in every pass.
            let morph_key = self.morph_draw_key(d, q, &keyed, is_shadow_lod_key(submitted.mesh_key));
            let keyed = match morph_key {
                Some(mesh_key) => FrameMesh { mesh_key, ..keyed },
                None => keyed,
            };
            // The joint cap (ADR-5418): a skin that weights a joint past its palette, or past
            // `MAX_JOINTS_PER_SKIN`, would skin from another draw's joints. No pass draws it; it is
            // counted (`DrawIntegrity::skinned_refused`).
            if !folded && self.refuse_skin_past_its_palette(submitted.mesh_key, submitted) {
                continue;
            }
            // A folded one-joint rigid skin draws rigid with `model × joint`; its decal layer
            // stays the skinned default (the character layer).
            let keyed = if folded {
                FrameMesh {
                    model,
                    joints: &[],
                    receiver_layer: crate::decal_receiver::stamp_layer(false, submitted.receiver_layer),
                    ..keyed
                }
            } else {
                keyed
            };
            // Zero keys pair with last frame's model by list index; a changed matrix there
            // is a moving (or shifted) mesh whose velocity may be wrong.
            if submitted.object_key == 0
                && self.have_prev_vp
                && self.prev_models.get(&keyed.object_key) != Some(&model)
            {
                zero_key_moving += 1;
            }
            if submitted.mesh_key == 0 {
                self.meshes.remove(&keyed.mesh_key);
            }
            if submitted.material_key == 0 {
                self.forget_material(keyed.material_key);
            }
            let mesh = &keyed;
            // Render override (Tincal F5): per-instance lanes; a ghost or x-ray instance draws
            // through the blend pass whatever its material's alpha mode.
            let over = self
                .render_overrides
                .get(&mesh.object_key)
                .copied()
                .unwrap_or([0, 0]);
            let over_blend = over[1] & crate::render_override::OVERRIDE_BLEND != 0;
            let material_alpha_mode = crate::hair::effective_alpha_mode(mesh.material);
            let alpha_mode = if over_blend {
                material_alpha_mode.max(ALPHA_BLEND)
            } else {
                material_alpha_mode
            };
            if !self.scalar_overlays.is_empty()
                && !is_shadow_lod_key(submitted.mesh_key)
                && let Some(overlay) = self.scalar_overlays.get(&mesh.object_key)
            {
                self.scalar_pass
                    .prepare(&crate::device::TrackedDevice::observe(d), q, mesh, overlay);
            }
            let _index_count = self.ensure_mesh(d, q, mesh, now, true)?;
            self.ensure_material(d, q, mesh.material_key, mesh.material, now)?;
            // A graph draws with its pipeline set once it is built. Async mode draws it with
            // the standard surface meanwhile; a set that cannot be built skips the mesh.
            let mut graph = None;
            let mut graph_vertex = false;
            let mut animated = false;
            let mut inflate = 0.0f32;
            let mut vertex_offset_bounded = false;
            // Per-material `doubleSided` in a frame that still culls. The scene flag already
            // builds every graph against `graph_target.double_sided`.
            let material_ds = mesh.material.double_sided && !scene.double_sided;
            if let Some(surface) = mesh.material.surface_graph.as_deref() {
                let target = if material_ds {
                    crate::graph_pipelines::GraphTarget {
                        double_sided: true,
                        ..graph_target
                    }
                } else {
                    graph_target
                };
                let ready = match graph_sets.entry((surface.key, material_ds)) {
                    std::collections::hash_map::Entry::Occupied(_) => true,
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        // An inline set build is several compiles: a cancelled warm stops here.
                        crate::pipeline_warm::warm_checkpoint()?;
                        let Some(layouts) = self.graph_layouts.as_ref() else {
                            return Err(RenderError::Shader("graph layouts missing".into()));
                        };
                        // The load budget's pipeline cap (ADR-7884): a set that would pass it
                        // builds on the graph workers and its draws wait.
                        let created = crate::frame_stats::thread_pipeline_creates()
                            .saturating_sub(self.load_budget.pipelines_at_start);
                        let defer = !self.load_budget.graph_set_fits(created);
                        // Past the cap with workers that cannot take the set: built here only as
                        // the frame's first creation (ADR-8122).
                        let inline_allowed = created == 0;
                        let (before, pending_before) = (
                            crate::frame_stats::thread_pipeline_creates(),
                            self.graph_cache.stats().pending,
                        );
                        let resolved = self.graph_cache.resolve(
                            d,
                            layouts,
                            target,
                            surface.key,
                            (defer, inline_allowed),
                        );
                        let built_here = crate::frame_stats::thread_pipeline_creates().saturating_sub(before);
                        if built_here > 0 {
                            let built = u32::try_from(built_here).unwrap_or(u32::MAX);
                            self.load_budget.graph_set_pipelines =
                                self.load_budget.graph_set_pipelines.max(built);
                        }
                        let queued = self.graph_cache.stats().pending.saturating_sub(pending_before);
                        self.load_budget.report.async_graph_sets += u32::try_from(queued).unwrap_or(u32::MAX);
                        match resolved {
                            crate::graph_pipelines::Resolved::Ready(set) => {
                                slot.insert(set);
                                true
                            }
                            // Under the budget's Defer a building set's draws wait (counted).
                            crate::graph_pipelines::Resolved::Pending if self.load_budget.gate_pipelines => {
                                self.load_budget.report.deferred_draws += 1;
                                continue;
                            }
                            // Not built yet, or refused by the permutation guard: the standard
                            // surface of the same pass draws it (never a stall).
                            crate::graph_pipelines::Resolved::Pending
                            | crate::graph_pipelines::Resolved::Refused => {
                                self.graph_cache.note_fallback();
                                false
                            }
                            crate::graph_pipelines::Resolved::Failed => continue,
                        }
                    }
                };
                if ready {
                    graph = Some(surface.key);
                    if surface.vertex_wgsl.is_some() {
                        graph_vertex = true;
                        animated = surface.uses_time;
                        inflate = surface.max_vertex_offset.max(0.0);
                        vertex_offset_bounded =
                            surface.max_vertex_offset.is_finite() && surface.max_vertex_offset > 0.0;
                    }
                    if surface.uses_uv1 {
                        self.ensure_mesh_uv1(d, q, mesh);
                    }
                }
            }
            // A swaying plant takes the visibility path under a standard twin of its material, with
            // its sway on the instance (ADR-3916). The graph keeps the forward passes it still draws
            // in (a masked instance's shadows).
            let sway_gate = if vis_frame && !is_shadow_lod_key(submitted.mesh_key) {
                crate::visbuffer::sway::sway_gate(mesh.material)
            } else {
                crate::visbuffer::sway::SwayGate::NotSwaying
            };
            let sway_plan = if sway_gate == crate::visbuffer::sway::SwayGate::Clusters {
                self.visbuffer.sway_plan(mesh.material_key, mesh.material)
            } else {
                None
            };
            if let Some(plan) = &sway_plan {
                if plan.fresh {
                    self.ensure_material(d, q, plan.twin_key, &plan.twin, now)?;
                }
                animated = true;
                inflate = inflate.max(plan.sway.reach);
            }
            // On the table path a graph's own group fits no standard pipeline, so while its set
            // builds the mesh draws a standard twin of the material (ADR-2071): the standard
            // surface meanwhile, with a record every pass can bind.
            let pending_twin;
            let mesh = if table_path && graph.is_none() && mesh.material.surface_graph.is_some() {
                let key = mesh.material_key ^ PENDING_GRAPH_TWIN_TAG;
                if let Some(entry) = self.materials.get_mut(&key) {
                    entry.last_used = now;
                } else {
                    let mut standard = mesh.material.clone();
                    standard.surface_graph = None;
                    self.ensure_material(d, q, key, &standard, now)?;
                }
                pending_twin = FrameMesh {
                    material_key: key,
                    ..*mesh
                };
                &pending_twin
            } else {
                mesh
            };
            // Material mask recolor and decal slots: standard opaque, masked and alpha-blended
            // surfaces only (`mask_decals`). An x-ray render override (F5) keeps its depth-off
            // blend pipeline, so the instance draws without its mask while x-rayed (counted as
            // unsupported); tint, ghost alpha, unlit and emissive overrides apply to masked draws.
            let mask = any_masked && self.mask_decals.has(mesh.object_key) && {
                let supported = mesh.material.surface_graph.is_none()
                    && mesh.material.transmission <= 0.0
                    && material_alpha_mode <= ALPHA_BLEND
                    && over[1] & crate::render_override::OVERRIDE_XRAY == 0;
                if !supported {
                    mask_unsupported += 1;
                }
                supported
                    && !is_shadow_lod_key(submitted.mesh_key)
                    && self.prepare_mask_decal(d, q, mesh.object_key, now)
            };
            // Lightmap receivers: standard opaque and masked surfaces drawn whole, rigid, with the
            // entry's UV2 as the mesh's second UV stream (`lightmap::receiver_mesh`). Anything
            // else keeps the probe light (counted as unsupported).
            let lightmap = if any_lightmapped && !is_shadow_lod_key(submitted.mesh_key) {
                self.lightmap_forward.receiver(mesh.object_key)
            } else {
                None
            };
            let lightmap = lightmap.filter(|&entry| {
                let supported = self.lightmap_forward.has_entry(entry)
                    && mesh.material.surface_graph.is_none()
                    && mesh.material.transmission <= 0.0
                    && alpha_mode < ALPHA_BLEND
                    && !mask
                    && mesh.joints.is_empty()
                    && morph_key.is_none()
                    && mesh.lod_dither == LOD_DITHER_NONE
                    && if Self::is_asset_draw(mesh) {
                        // A counted mesh: its UV2 for every uploaded vertex (ADR-7330).
                        self.asset_uv1_fits(mesh)
                    } else {
                        !mesh.vertices.is_empty() && mesh.uv1.len() == mesh.vertices.len()
                    };
                if !supported {
                    self.lightmap_forward.note_unsupported();
                }
                supported
            });
            let lightmap = if lightmap.is_some() && Self::is_asset_draw(mesh) {
                // Another draw of this counted mesh wrote a different UV2 this frame: probe light.
                let built = self.ensure_asset_uv1(d, q, mesh);
                if !built {
                    self.lightmap_forward.note_unsupported();
                }
                lightmap.filter(|_| built)
            } else {
                if lightmap.is_some() {
                    self.ensure_mesh_uv1(d, q, mesh);
                }
                lightmap
            };
            // A thin film: a standard surface that binds nothing else in group 3 and is not
            // x-rayed, and not an opaque draw on a Mobile lite tier (the lite shader has no film;
            // the blend pass has no lite path). Anything else draws the base Fresnel (counted as
            // unsupported).
            let film = mesh.material.thin_film.is_on() && !is_shadow_lod_key(submitted.mesh_key) && {
                let supported = mesh.material.surface_graph.is_none()
                    && mesh.material.transmission <= 0.0
                    && !mask
                    && lightmap.is_none()
                    && !(lite_frame && alpha_mode < ALPHA_BLEND)
                    && over[1] & crate::render_override::OVERRIDE_XRAY == 0;
                if !supported {
                    self.thin_film.note_unsupported();
                }
                supported && self.prepare_thin_film(d, q, mesh.material_key, mesh.material, now)
            };
            // Subsurface scattering: an opaque or masked standard surface of a profile that asks,
            // drawn from its own pass. Anything else draws without it (counted as unsupported). The
            // eye keeps its model's block and a skin keeps its falloff.
            let subsurface = scene.quality.subsurface_scattering
                && mesh.material.subsurface.is_on()
                && !is_shadow_lod_key(submitted.mesh_key)
                && {
                    let eye = self
                        .material_shading
                        .get(&mesh.material_key)
                        .is_some_and(|shading| shading[0][0] == crate::shading_model::MODEL_EYE);
                    // The reason is counted by code (ADR-3170); none is the old `supported`.
                    let reason = crate::subsurface::SubsurfaceUnsupported::of_draw(
                        mesh.material,
                        crate::subsurface::SubsurfaceDrawFacts {
                            eye,
                            blended: alpha_mode >= ALPHA_BLEND,
                            masked: mask,
                            thin_film: film,
                            lightmap: lightmap.is_some(),
                            forward_lite: lite_frame,
                            render_override: over != [0, 0],
                        },
                    );
                    if let Some(reason) = reason {
                        self.subsurface.note_unsupported(reason);
                    }
                    reason.is_none()
                };
            // A morphed copy's vertices move up to this far from the base bound.
            if morph_key.is_some() {
                inflate += self.morph.inflate(mesh.object_key);
            }
            // Storage-object draws (colour indirect, instanced shadow and depth)
            // do not need a uniform. Anything the CPU loop might still draw does.
            // The rest pose is a cull bound only while the draw is static. A skinned mesh keeps
            // the same sphere on the cache entry and must not join the rigid batches with it.
            let rest_bounds = if mesh.joints.is_empty() {
                self.meshes.get(&mesh.mesh_key).and_then(|entry| entry.bounds)
            } else {
                None
            };
            let has_bounds = rest_bounds.is_some();
            let cull_off = material_ds;
            // The joint-palette sphere of a skinned draw (counted in `skin_bounds`); the GPU cull
            // tests the same sphere when the draw joins the instanced batches.
            let mut skin_sphere = None;
            // The palette's object-space box this frame: the colour cull's sphere and the
            // shadow caches' caster bound (ADR-5418) both come from it.
            let skin_object = if mesh.joints.is_empty() {
                None
            } else {
                self.skinned_object_box(submitted.mesh_key, mesh, inflate)
            };
            // Per-view frustum culling (specs/13 § 5 step 3): a skinned mesh is tested with its
            // joint-palette bound (`skin_bounds`, ADR-248); one without a bound always draws.
            // The shadow caches page a bounded skinned caster by the same bound (ADR-5418); an
            // unbounded one redraws whole cascades. A shadow-LOD draw
            // (top byte 0xFF, set before the dither xor) is not in the colour pass; the
            // invisible-caster branch below still submits it to the shadow passes.
            let shadow_only = is_shadow_lod_key(submitted.mesh_key);
            let visible = if shadow_only {
                false
            } else if mesh.joints.is_empty() {
                let mesh_visible = rest_bounds.is_none_or(|bounds| {
                    let model = regia_core::glam::Mat4::from_cols_array_2d(&mesh.model);
                    let center = model.transform_point3(regia_core::glam::Vec3::from_array(bounds.center));
                    // Conservative world radius: the largest axis scale.
                    let scale = (model
                        .x_axis
                        .length_squared()
                        .max(model.y_axis.length_squared())
                        .max(model.z_axis.length_squared()))
                    .sqrt();
                    frustum.intersects_sphere(center.to_array(), bounds.radius * scale + inflate)
                });
                if !mesh_visible {
                    culled += 1;
                }
                mesh_visible
            } else {
                skin_sphere = self.count_skinned_bound(
                    skin_object
                        .as_ref()
                        .map(|object| super::skin_bounds::world_sphere(&mesh.model, object, inflate)),
                );
                let skin_visible =
                    skin_sphere.is_none_or(|(center, radius)| frustum.intersects_sphere(center, radius));
                if !skin_visible {
                    culled += 1;
                    self.skin_bounds.culled += 1;
                }
                skin_visible
            };
            if visible && self.stream_frame.policy.enabled {
                self.note_stream_need(mesh, scene.eye, scene.near, stream_ppr, now);
            }
            // Centroid depth (CPU vertex walk) is only needed to sort blended
            // meshes back-to-front; opaque draws keep insertion order.
            let sort_depth = if alpha_mode >= ALPHA_BLEND {
                let bounds = self.meshes.get(&mesh.mesh_key).and_then(|entry| entry.bounds);
                blended_sort_depth(
                    &mut self.caster_list_cache,
                    mesh.mesh_key,
                    mesh.vertices,
                    bounds,
                    mesh.model,
                    view,
                )
            } else {
                0.0
            };
            // Clusters become resident for every eligible mesh, as the forward vertex buffers do
            // above, so a mesh entering the view later does not cook or upload mid-run.
            // Graph materials keep their own forward pipeline (the resolve shades the standard surface).
            // Off-screen instances are queued too: the GPU culls them for the view, and they may
            // still cast into a shadow map. A LOD cross-fade draw (dithered) needs the forward
            // alpha-test path.
            // A masked material takes the path on a texture-table device with its alpha-tested
            // raster (ADR-3915), unless the frame may hash its alpha test (a temporal frame with
            // TAAU hashed alpha, or under the TSR upscaler with its hashed alpha, as
            // `frame_uniforms` decides): the cluster raster tests the hard cutoff only.
            let hashed_alpha = taau_on
                && (self.taa_hashed_alpha
                    || (self.temporal_upscaler == crate::TemporalUpscaler::Tsr
                        && self.tsr_pass.settings().hashed_alpha));
            let masked_vis = material_alpha_mode == crate::lab_frame::ALPHA_MASK
                && crate::visbuffer::gpu::alpha_tests(mesh.material)
                && !hashed_alpha
                && crate::visbuffer::VisBufferPass::masked_supported(d);
            let vis_instance = if vis_frame
                && !shadow_only
                && mesh.joints.is_empty()
                && (material_alpha_mode == crate::ALPHA_OPAQUE || masked_vis)
                && mesh.material.transmission <= 0.0
                && mesh.lod_dither == LOD_DITHER_NONE
                && over == [0, 0]
                && (graph.is_none() || sway_plan.is_some())
                && !mask
                && !film
                && !subsurface
                && (lightmap.is_none() || crate::visbuffer::VisBufferPass::lightmap_supported(d))
                && morph_key.is_none()
                && has_bounds
                // A counted mesh drawn from its upload clusters its adopted vertex buffer (ADR-7462).
                && self.ensure_vis_clusters(mesh)
                && (lightmap.is_none() || self.visbuffer.ensure_uv2(mesh.mesh_key, mesh.uv1))
            {
                // A double-sided material's clusters draw with the cull-none rasters.
                let prev_model = self
                    .prev_models
                    .get(&mesh.object_key)
                    .copied()
                    .unwrap_or(mesh.model);
                let vis_material_key = sway_plan.as_ref().map_or(mesh.material_key, |plan| plan.twin_key);
                if masked_vis {
                    self.visbuffer.push_masked_instance(
                        mesh.mesh_key,
                        vis_material_key,
                        mesh.model,
                        prev_model,
                        mesh.material.double_sided,
                    )
                } else {
                    self.visbuffer.push_instance(
                        mesh.mesh_key,
                        vis_material_key,
                        mesh.model,
                        prev_model,
                        mesh.shadow,
                        mesh.material.double_sided,
                    )
                }
            } else {
                None
            };
            // Stable identity for the cluster LOD history (ADR-4631): a replaced or reordered
            // object at the same instance count starts from the hard cut.
            if let Some(index) = vis_instance {
                self.visbuffer.set_lod_identity(index, mesh.object_key);
            }
            let vis = vis_instance.is_some();
            match (vis_instance, &sway_plan, sway_gate) {
                (Some(index), Some(plan), _) => self.visbuffer.set_instance_sway(index, plan.sway),
                (None, _, crate::visbuffer::sway::SwayGate::Forward(skip)) => {
                    self.visbuffer.note_sway_forward(skip);
                }
                (None, plan, crate::visbuffer::sway::SwayGate::Clusters) => {
                    use crate::visbuffer::sway::SwaySkip;
                    let masked = material_alpha_mode == crate::lab_frame::ALPHA_MASK;
                    self.visbuffer.note_sway_forward(if plan.is_none() {
                        SwaySkip::TableFull
                    } else if masked && hashed_alpha {
                        SwaySkip::HashedAlpha
                    } else if masked && !crate::visbuffer::VisBufferPass::masked_supported(d) {
                        SwaySkip::NoTextureTable
                    } else {
                        SwaySkip::Other
                    });
                }
                _ => {}
            }
            let glass_draw = glass_index
                .get(&mesh.object_key)
                .copied()
                .filter(|_| graph.is_none() && mesh.material.transmission > 0.0);
            // Viewer-inside glass checked against the camera (`canopy_view`, ADR-262).
            let glass_inside = glass_draw.is_some_and(|index| self.glass[index].inside);
            let canopy_outside =
                glass_inside && self.canopy_eye_outside(scene.eye, submitted.mesh_key, mesh, inflate);
            if glass_inside && visible {
                canopy_draws.note(canopy_cheap_setting, canopy_outside);
            }
            // A skinned instance on the compute path draws its skinned twin (Tincal B2). A morphed
            // draw stays on the vertex-shader path: the twin skins the base vertices, not the
            // morphed copy (morph targets are not fed into the kernel yet).
            let draw_mesh = if morph_key.is_some() {
                mesh.mesh_key
            } else {
                self.skin_twin(device, q, mesh, now).unwrap_or(mesh.mesh_key)
            };
            // The skinned caster's bound in the shadow caches (ADR-5418): the palette box's sphere
            // in model space, the shape of a rigid mesh's bound. Vertex-shader skinning only: a
            // compute twin may blend dual quaternions or eight influences, which the box (four
            // linear-blend lanes) does not bound. Its pose can move inside the bound, so on a
            // frame whose palette differs from last frame's it is a dynamic caster (`specs/88`); a
            // held pose caches like a static caster.
            let skin_bound = skin_object
                .as_ref()
                .filter(|_| draw_mesh == mesh.mesh_key)
                .map(super::skin_bounds::object_sphere);
            if skin_bound.is_some() && self.palette_changed(mesh.object_key, mesh.joints) {
                animated = true;
            }
            // A vertex-shader-skinned draw with a palette bound joins the GPU-driven batches
            // (ADR-1368); a compute-skinned twin keeps its own vertex buffer and draws alone.
            let skin_sphere = skin_sphere.filter(|_| {
                gpu_objects
                    && self.skin_bounds.instancing
                    && draw_mesh == mesh.mesh_key
                    && !mask
                    && !film
                    && !subsurface
            });
            // Transmissive surfaces keep a uniform: the refraction pass and the translucent shadow
            // casters draw them per object even when the opaque passes instance them (double-sided
            // glass used to get one through `cull_off`, before double-sided draws joined the
            // instanced lists).
            let cpu_uniform = !(gpu_objects
                && ((mesh.joints.is_empty() && has_bounds) || skin_sphere.is_some())
                && alpha_mode < ALPHA_BLEND
                && mesh.material.transmission <= 0.0);
            // A masked instance or a thin film binds its own group 3, and a scattering draw has its
            // own pass, so they draw from the CPU colour loop; so does a lightmap receiver on a Mobile
            // lite tier (ADR-2107: elsewhere it joins the batches, or the visibility buffer).
            let lite_receiver = lightmap.is_some() && lightmap_lite != crate::forward_lite::ForwardLite::Off;
            let cpu_uniform = cpu_uniform || mask || lite_receiver || film || subsurface;
            if cpu_uniform {
                self.ensure_object(
                    d,
                    super::scratch::object_uniform_key(mesh.object_key, mesh.lod_dither.to_bits()),
                    now,
                );
            }
            draws.push(Draw {
                morphed: morph_key.is_some(),
                mesh: draw_mesh,
                material: mesh.material_key,
                object: mesh.object_key,
                sort_depth,
                // A transmissive surface blends over what is behind it whatever its alpha mode.
                alpha_mode: if mesh.material.transmission > 0.0 {
                    alpha_mode.max(ALPHA_PREMULTIPLIED)
                } else {
                    alpha_mode
                },
                transmissive: mesh.material.transmission > 0.0,
                // Own-canopy glass never refracts; damaged glass refracts at its cracks even
                // as a thin wall.
                refracts: mesh.material.transmission > 0.0
                    && match glass_draw.map(|index| &self.glass[index]) {
                        Some(instance) if instance.inside => false,
                        Some(instance) if instance.damage.is_some() => true,
                        _ => {
                            mesh.material.transmission_thickness > 0.0
                                || crate::rough_transmission::is_on(mesh.material)
                        }
                    },
                model,
                joint_base,
                joint_count: mesh.joints.len().min(MAX_JOINTS_PER_SKIN) as u32,
                visible,
                rigid: mesh.joints.is_empty(),
                skin_sphere,
                skin_bound,
                receiver_layer: mesh.receiver_layer,
                shadow: mesh.shadow,
                indirect: None,
                cpu_uniform,
                vis,
                vis_masked: vis && masked_vis,
                vis_instance,
                graph,
                graph_vertex,
                animated,
                is_static: crate::static_batch_eligible(mesh),
                inflate,
                vertex_offset_bounded,
                lod_dither: mesh.lod_dither,
                glass: glass_draw.filter(|_| !(table_path && graph.is_some())),
                cull_off,
                over,
                xray: over[1] & crate::render_override::OVERRIDE_XRAY != 0,
                mask: mask && !(table_path && graph.is_some()),
                hair: mesh.material.hair.enabled,
                lightmap,
                film,
                subsurface,
                canopy_outside,
                src: index,
            });
        }
        if !view_pass {
            self.canopy_draws = canopy_draws;
        }
        // CPU masked occlusion (ADR-10072): hidden opaque rigid draws leave the colour and depth
        // lists below; the shadow list keeps them, as it keeps frustum-culled draws. Counted in
        // `masked_occlusion_report`, not in `DrawStats::culled` (the frustum's count).
        self.masked_cull_draws(scene, &mut draws, view_pass, unjittered_vp, fov_y);
        self.scratch.spans.give(spans);
        self.scratch.glass_index.give(glass_index);
        self.last_draw_stats = DrawStats {
            drawn: draws.iter().filter(|draw| draw.visible).count(),
            culled,
            rigid_skin_folded,
            zero_key_moving_meshes: zero_key_moving,
            ..DrawStats::default()
        };
        if taau_on && zero_key_moving >= ZERO_KEY_WARN_MIN {
            static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!(
                    target: "regia_render",
                    zero_key_moving_meshes = zero_key_moving,
                    meshes = scene.meshes.len(),
                    "temporal AA with moving meshes submitted with object_key 0: their motion vectors \
                     pair by list index, so a spawn, despawn or reorder smears; give moving objects a \
                     stable non-zero object_key (see the host checklist, \"Object keys\")"
                );
            }
        }
        // Residency feedback (ADR-1110, off by default): last frames' GPU texel needs, before
        // the stream pass reads this frame's needs.
        if !view_pass {
            self.residency_feedback_frame(d, q, now);
        }
        if self.stream_frame.policy.enabled || self.may_stream {
            self.stream_materials(d, q, scene, now);
        }
        if !view_pass {
            self.refresh_texture_problems();
        }
        // Every per-object draw finds its object entry for the rest of the frame (rebuilt on
        // demand; `lifecycle`, W2.2).
        self.repair_objects(
            d,
            draws
                .iter()
                .filter(|draw| draw.cpu_uniform)
                .map(|draw| draw.uniform_key()),
            now,
        );
        let mut object_uploads = self.scratch.object_uploads.take();
        for draw in &draws {
            if !draw.cpu_uniform {
                continue;
            }
            if let Some(entry) = self.objects.get_mut(&draw.uniform_key()) {
                let object = draw.object_data();
                if entry.uploaded != Some(object) {
                    object_uploads.push((entry.buf.clone(), object));
                    entry.uploaded = Some(object);
                }
                entry.last_used = now;
            }
        }
        for (buf, object) in object_uploads.drain(..) {
            self.belt.defer(d, &buf, 0, bytemuck::bytes_of(&object));
        }
        self.scratch.object_uploads.give(object_uploads);
        let mut opaque_ix = self.scratch.opaque_ix.take();
        let mut blend_ix = self.scratch.blend_ix.take();
        // Shadow passes draw every opaque mesh, culled ones included: an
        // off-screen mesh can still cast a visible shadow.
        let mut shadow_ix = self.scratch.shadow_ix.take();
        for (index, draw) in draws.iter().enumerate() {
            if draw.visible {
                if draw.alpha_mode >= ALPHA_BLEND {
                    blend_ix.push(index);
                } else if !draw.vis {
                    opaque_ix.push(index);
                }
                if draw.alpha_mode < ALPHA_BLEND && draw.shadow != ShadowCast::Off && !draw.hair {
                    shadow_ix.push(index);
                }
            } else if draw.alpha_mode < ALPHA_BLEND && draw.shadow != ShadowCast::Off && !draw.hair {
                shadow_ix.push(index);
            }
        }
        self.last_shadow_casters = shadow_ix.len();
        // Bindless step-2 census (ADR-356, off by default): counts only, nothing reordered.
        if self.texture_set_census.enabled() && !view_pass {
            self.note_opaque_texture_sets(opaque_ix.iter().map(|&index| draws[index].material));
        }
        // The material texture table (ADR-863): rebuilt here at most once per frame, after this
        // frame's material uploads and residency swaps and before the first pass binds it. Each
        // pass's tally binds it with the pass's first material group.
        let texture_table_group = match self.texture_table.as_mut() {
            Some(table) => {
                table.refresh(d);
                table.group().cloned()
            }
            None => None,
        };
        blend_ix.sort_by(|a, b| draws[*b].sort_depth.total_cmp(&draws[*a].sort_depth));
        Ok(DrawList {
            graph_sets,
            vis_frame,
            vis_shadow_state,
            vis_set_eye,
            draws,
            table_path,
            mask_unsupported,
            opaque_ix,
            blend_ix,
            shadow_ix,
            texture_table_group,
        })
    }
}

#[cfg(test)]
mod load_budget_tests {
    use std::sync::Arc;

    use crate::{
        DirectionalLightUniform, FrameLoadBudget, FrameMesh, FrameRenderer, FrameScene, GraphCode,
        LabMaterial, LabMesh, LabScene, LabVertex, LateDraw, SurfaceGraph,
    };

    fn lab() -> LabScene {
        let quad = LabMesh {
            vertices: vec![
                LabVertex::from_pos_normal_uv([-0.1, -0.1, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0]),
                LabVertex::from_pos_normal_uv([0.1, -0.1, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0]),
                LabVertex::from_pos_normal_uv([0.1, 0.1, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
                LabVertex::from_pos_normal_uv([-0.1, 0.1, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
            ],
            indices: vec![0, 1, 2, 0, 2, 3],
            ..LabMesh::unskinned()
        };
        LabScene {
            width: 32,
            height: 32,
            eye: [0.0, 0.0, 3.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 0.9,
            near: 0.1,
            far: 100.0,
            depth_prepass: false,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 12.0,
            ambient_lux: 5_000.0,
            double_sided: false,
            post: None,
            directional: DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0; 3], 20_000.0),
            local_lights: Vec::new(),
            materials: vec![LabMaterial::default()],
            meshes: vec![quad],
        }
    }

    /// Graph `i`'s code: a flat surface whose albedo differs per `i`, so each is its own key.
    fn graph_code(i: usize) -> String {
        format!(
            "fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {{\n    \
             return regia::pbr::surface::Surface(vec3<f32>({:.4}, 0.8, 0.8), 1.0, 0.0, 1.0, in.world_normal, \
             1.0, vec3<f32>(0.0), 0.0);\n}}\n",
            0.2 + i as f32 / 100.0
        )
    }

    /// Review rev-d row 4 (ADR-8122): under `LateDraw::Defer`, a material-graph set the workers
    /// cannot take (here they were shut down) is not compiled inline past the pipeline cap. The
    /// frame builds at most one set (its first creation, like the mesh that crosses the upload
    /// cap) and the other draws wait; one more set lands per frame until all draw. v0.5.1 before
    /// the fix built every set of the frame inline on its first frame.
    #[test]
    fn defer_builds_at_most_one_graph_set_per_frame_when_the_workers_cannot_take_it() {
        const GRAPHS: usize = 6;
        let device = crate::device::test_device();
        let lab = lab();
        let base = lab.as_frame_scene();
        let materials: Vec<LabMaterial> = (0..GRAPHS)
            .map(|i| LabMaterial {
                surface_graph: Some(Arc::new(SurfaceGraph::from_code(&GraphCode {
                    surface: graph_code(i).into(),
                    vertex: None,
                    uv1: false,
                }))),
                ..LabMaterial::default()
            })
            .collect();
        let template = base.meshes[0];
        let mut meshes = vec![template];
        meshes.extend(materials.iter().enumerate().map(|(i, material)| FrameMesh {
            material,
            material_key: 0x8122_0000 + i as u64 + 1,
            object_key: 0x8122_1000 + i as u64 + 1,
            model: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [i as f32 * 0.25 - 0.6, 0.3, 0.0, 1.0],
            ],
            ..template
        }));
        let scene = FrameScene {
            meshes,
            ..base.clone()
        };
        let mut renderer = FrameRenderer::new();
        renderer.set_load_budget(FrameLoadBudget {
            upload_bytes_per_frame: None,
            new_pipelines_per_frame: Some(8),
            late: LateDraw::Defer,
        });
        for _ in 0..3 {
            renderer.render_offscreen(&device, &base).expect("warm-up frame");
        }
        // The material-graph workers are gone (a shut-down cache, or threads that cannot start).
        renderer.graph_cache.shutdown_workers();
        // Drawn like the window path (no capture overrides); the readback is not counted.
        let frame = |renderer: &mut FrameRenderer| {
            let before = crate::frame_stats::counters();
            let target = renderer
                .begin_offscreen_live(&device, &scene, wgpu::TextureFormat::Rgba8Unorm)
                .expect("frame");
            let pipelines = crate::frame_stats::counters().since(before).pipelines;
            renderer.read_offscreen(&device, target).expect("readback");
            pipelines
        };
        let pipelines = frame(&mut renderer);
        let report = renderer.load_budget_report();
        let set = u64::from(renderer.load_budget.graph_set_pipelines);
        // The cap, or one set when a set alone is larger than the cap.
        let bound = set.max(8);
        eprintln!("frame 1: {pipelines} pipelines (one set: {set}), {report:?}");
        assert!(set > 0, "one set was built and measured: {report:?}");
        assert!(
            pipelines <= bound,
            "frame 1 compiled {pipelines} pipelines inline under an 8-pipeline Defer cap (one set is {set})"
        );
        assert!(report.deferred_draws >= 1, "graph draws wait: {report:?}");
        // Every graph draws within GRAPHS frames in all, no frame over the bound.
        let mut frames = 1;
        while renderer.load_budget_report().deferred_draws > 0 {
            frames += 1;
            assert!(frames <= GRAPHS, "graph draws still wait after {GRAPHS} frames");
            let pipelines = frame(&mut renderer);
            assert!(
                pipelines <= bound,
                "frame {frames}: {pipelines} pipelines (bound {bound})"
            );
        }
        let pipelines = frame(&mut renderer);
        assert_eq!(pipelines, 0, "every set is built");
        assert_eq!(renderer.draw_stats().drawn, GRAPHS + 1, "every draw is back");
    }
}
