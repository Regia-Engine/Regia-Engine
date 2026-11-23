// SPDX-License-Identifier: MPL-2.0
//! Pipeline prewarm: the forward, double-sided, glass, decal and effect sets, material graphs
//! and the warm-pipeline handoff (moved out of `frame.rs` unchanged, ADR-1552).

use super::*;

/// The pipeline keys [`FrameRenderer::warm_pipeline_key`] builds (ADR-2018). Every key a
/// built-in step of `FRAME_STEPS` declares is here, and every key here is declared by a step.
pub(super) const WARM_KEYS: &[&str] = &[
    "blue_noise",
    "tsr",
    "taau_resolve",
    "ssgi_half_res",
    "depth_match",
    "depth_match_msaa",
    "outdoor",
    "sun_shadow_trace",
    "forward_sun_rt",
    "forward_studio",
    "visbuffer_path",
    "radiance_cache",
    "subsurface",
    "oit",
    "restir_di",
];

impl FrameRenderer {
    /// Takes the device-level pipeline state `warmed` built on a warm worker (the forward/shadow
    /// pipeline sets and their layouts, the post chain's per-format pipelines, the high-end
    /// passes' pipelines), so this renderer's frames do not create them again
    /// (`pipeline_warm`). Each part moves only where this renderer has none yet (pipeline sets
    /// are bound to the layouts built with them, so they move together); frame state (targets,
    /// exposure, histories, caches) never moves, so frames are unchanged. Returns whether
    /// anything moved.
    pub fn adopt_warm_pipelines(&mut self, warmed: &mut FrameRenderer) -> bool {
        let mut moved = false;
        if self.pipes.is_none()
            && warmed.pipes.is_some()
            && warmed.material_anisotropy() == self.material_anisotropy()
        {
            self.pipes = warmed.pipes.take();
            moved = true;
        }
        if let Some(post) = warmed.post.as_mut() {
            moved |= self
                .post
                .get_or_insert_with(crate::post_pass::PostChain::default)
                .adopt_pipelines(post);
        }
        moved |= self.highend.adopt_pipelines(&mut warmed.highend);
        moved |= self.visbuffer.adopt_pipelines(&mut warmed.visbuffer);
        moved |= self.radiance_cache.adopt_pipelines(&mut warmed.radiance_cache);
        moved
    }

    /// Builds material-graph pipelines on worker threads instead of on the frame that first
    /// draws a graph (`graph_pipelines`). Off by default, so lab captures and goldens draw every
    /// graph on the first frame; [`crate::RuntimeRenderer`] turns it on. While a graph's set
    /// builds, its meshes draw with the standard surface.
    pub fn set_async_material_graphs(&mut self, enabled: bool) {
        self.graph_cache.set_async(enabled);
    }

    /// Starts building the pipelines of every graph in `graphs` for every target in `targets`
    /// (level load: pass each graph material the level can draw). Async mode queues them on
    /// workers ([`Self::settle_material_graphs`] waits); inline mode builds them now. A target
    /// asking for instanced variants on a device or frame that cannot draw GPU-driven is
    /// normalised, so the prewarmed set is the one the frame looks up. Returns the number of
    /// sets started.
    ///
    /// # Errors
    ///
    /// [`RenderError`] when the renderer's base pipelines fail to build.
    #[tracing::instrument(skip_all)]
    pub fn prewarm_material_graphs(
        &mut self,
        device: &RenderDevice,
        graphs: &[&crate::SurfaceGraph],
        targets: &[crate::GraphTarget],
    ) -> Result<usize, RenderError> {
        let d = &device.device;
        if self.pipes.is_none() {
            let q = crate::device::TrackedQueue::observe(&device.queue);
            self.pipes = Some(build_pipes(
                d,
                &q,
                device.shared_pipelines(),
                self.material_anisotropy(),
            )?);
        }
        self.ensure_graph_layouts(d)?;
        let Some(layouts) = self.graph_layouts.clone() else {
            return Ok(0);
        };
        let gpu_driven = crate::gpu_driven::IndirectStrategy::select(&device.capabilities).uses_gpu_objects();
        let mut started = 0;
        for target in targets {
            let mut target = *target;
            target.instanced &= gpu_driven && target.samples <= 1;
            for graph in graphs {
                // Inline builds compile here, one set (several pipelines) per graph.
                crate::pipeline_warm::warm_checkpoint()?;
                if self.graph_cache.prewarm(d, &layouts, target, &graph.code()) {
                    started += 1;
                }
            }
        }
        Ok(started)
    }

    /// Hands over every material graph the level can draw, also the ones no mesh of the warm
    /// frame uses yet (units, wrecks and effects spawned mid-play). The engine's frame warms
    /// ([`Self::warm_extracted`], [`Self::warm_scene`], and so [`crate::GameFrameWarm`]) then
    /// start their pipeline sets for every target the warm frame drew, on the graph workers,
    /// next to the frame's own: a graph first drawn mid-play finds its set built instead of
    /// drawing the standard surface while it compiles. Kept until replaced; set it at level
    /// load, before the warm (on a [`crate::RuntimeRenderer`] that is away on a warm, the call
    /// on the stand-in is moved over). Frames are unchanged.
    pub fn set_level_material_graphs(&mut self, graphs: Vec<std::sync::Arc<crate::SurfaceGraph>>) {
        self.level_graphs = graphs;
        self.installed_since.level_graphs = true;
    }

    /// Frame warms ([`Self::warm_extracted`], [`Self::warm_scene`], and so
    /// [`crate::GameFrameWarm`]) build the decal receiver ([`Self::prewarm_decals`]) even while
    /// no translucent layer is installed: an effect runtime whose layer is still being built on
    /// a warm worker sets it, so a warm that runs first does not leave the first decal to
    /// compile on the frame path. Kept until changed (moved over from a stand-in like
    /// [`Self::set_level_material_graphs`]). Frames are unchanged.
    pub fn set_warm_decals(&mut self, enabled: bool) {
        self.warm_decals = enabled;
        self.installed_since.warm_decals = true;
    }

    /// Frame warms ([`Self::warm_extracted`], and so [`crate::GameFrameWarm`]) also render one
    /// reflection-probe face of the warm frame (the capture's quality, no post stack, its
    /// readback target, the terrain's plain and hole-cut sets for that target), so a later
    /// [`Self::capture_reflection_probe`] compiles nothing on the thread that calls it. On by
    /// itself once reflection probes are installed or a capture ran on this renderer; a host
    /// that captures its probes only after the game-frame warm sets it at level load. Kept until
    /// changed (moved over from a stand-in like [`Self::set_level_material_graphs`]). Frames are
    /// unchanged.
    pub fn set_warm_probe_capture(&mut self, enabled: bool) {
        self.warm_probe_capture = enabled;
        self.installed_since.warm_probe_capture = true;
    }

    /// Frame warms: renders one face of `scene` the way [`Self::capture_reflection_probe`] renders
    /// its six (capture quality, no post stack, the readback target), when this renderer captures
    /// probes ([`Self::set_warm_probe_capture`], probes installed, or a capture ran). A capture
    /// draws the terrain and the forward sets in a target no game frame uses: before v0.1.11 the
    /// first capture of a map compiled them (and the hole-cut set) where the host called it.
    /// Returns whether it rendered. The shared draw's sets for the face's format are warmed by
    /// [`crate::SharedPassDraw::warm_pipelines`] afterwards; the next frame starts with a cut.
    ///
    /// # Errors
    ///
    /// The face failed to render, or the warm queue running this shut down.
    pub(crate) fn warm_probe_face(
        &mut self,
        device: &RenderDevice,
        scene: &FrameScene<'_>,
    ) -> Result<bool, RenderError> {
        if !(self.warm_probe_capture || self.probe_captured || !self.reflection_probes.is_empty()) {
            return Ok(false);
        }
        let _span = tracing::info_span!("warm_probe_face").entered();
        crate::pipeline_warm::warm_checkpoint()?;
        let center = scene.eye;
        // The cube face that looks most along the warm frame's view: it sees the frame's meshes.
        let look = [
            scene.target[0] - center[0],
            scene.target[1] - center[1],
            scene.target[2] - center[2],
        ];
        let face_index = (0..6u32)
            .max_by(|a, b| {
                let dot = |face: u32| {
                    let (dir, _) = crate::reflections::cube_face_axes(face);
                    dir[0] * look[0] + dir[1] * look[1] + dir[2] * look[2]
                };
                dot(*a).total_cmp(&dot(*b))
            })
            .unwrap_or(0);
        self.with_probe_state(|this| {
            let (dir, up) = crate::reflections::cube_face_axes(face_index);
            this.camera_up = up;
            let target = [center[0] + dir[0], center[1] + dir[1], center[2] + dir[2]];
            let face = probe_face_scene(scene, center, target, crate::reflections::PROBE_FACE_SIZE);
            this.render_offscreen(device, &face).map(drop)
        })?;
        // The GPU-driven colour variants a capture's opaque batches bind (plain and alpha-masked)
        // whatever this face saw: what a capture sees depends on where the host captures.
        if crate::gpu_driven::IndirectStrategy::select(&device.capabilities).uses_gpu_objects()
            && let Some(pipes) = self.pipes.as_ref()
        {
            crate::pipeline_warm::warm_checkpoint()?;
            let (view_bgl, mat_bgl) = (pipes.view_bgl.clone(), pipes.mat_bgl.clone());
            self.culler.color_pipelines(
                &device.device,
                PROBE_CAPTURE_FORMAT,
                !PROBE_CAPTURE_FORMAT.is_srgb(),
                1,
                true,
                &view_bgl,
                &mat_bgl,
                &[(scene.double_sided, false), (scene.double_sided, true)],
            )?;
        }
        Ok(true)
    }

    /// The graphs given to [`Self::set_level_material_graphs`].
    #[must_use]
    pub fn level_material_graphs(&self) -> &[std::sync::Arc<crate::SurfaceGraph>] {
        &self.level_graphs
    }

    /// Frame warms: the double-sided (cull-off) twin of every forward pipeline set the warm
    /// frame built ([`Self::prewarm_double_sided`]: the per-object set and the GPU-driven
    /// colour, depth-prepass and shadow variants). A
    /// frame without a double-sided material builds only the culled set; the first
    /// double-sided mesh mid-play (a wreck, foliage, a spawned unit's decal card) then compiled
    /// a whole set on the frame path (DO27 INTEGRATE_005 § 3). Returns the sets built.
    pub(crate) fn prewarm_double_sided_sets(&mut self, device: &RenderDevice) -> Result<usize, RenderError> {
        let Some(pipes) = self.pipes.as_ref() else {
            return Ok(0);
        };
        let targets: Vec<GlassPipelineTarget> = pipes
            .sets
            .keys()
            .filter(|k| !k.double_sided)
            .filter(|k| {
                !pipes.sets.contains_key(&PipelineKey {
                    double_sided: true,
                    sun_rt: self.culler.forward_sun_rt(),
                    studio_off: self.forward_studio_off(),
                    ..**k
                })
            })
            .map(|k| GlassPipelineTarget {
                format: k.format,
                encode: k.encode,
                samples: k.samples,
            })
            .collect();
        for target in &targets {
            crate::pipeline_warm::warm_checkpoint()?;
            self.prewarm_double_sided(device, *target)?;
        }
        Ok(targets.len())
    }

    /// Frame warms: starts the sets of [`Self::set_level_material_graphs`] for every target
    /// this renderer's frames drew graphs into (`graph_pipelines`). Returns the sets started.
    pub(crate) fn prewarm_level_graphs(&mut self, device: &RenderDevice) -> Result<usize, RenderError> {
        if self.level_graphs.is_empty() {
            return Ok(0);
        }
        let targets = self.graph_cache.targets_seen().to_vec();
        if targets.is_empty() {
            return Ok(0);
        }
        let graphs = self.level_graphs.clone();
        let refs: Vec<&crate::SurfaceGraph> = graphs.iter().map(std::sync::Arc::as_ref).collect();
        self.prewarm_material_graphs(device, &refs, &targets)
    }

    /// Waits until every queued material-graph build has finished (level load; never call on
    /// the frame path).
    pub fn settle_material_graphs(&mut self) {
        let _span = tracing::info_span!("material_graph_settle").entered();
        self.graph_cache.settle();
    }

    /// [`Self::settle_material_graphs`] for at most `budget`: installs the builds that land
    /// in time and returns whether every queued build landed. The rest keep building on the
    /// workers and are installed by the frames that follow (their draws use the standard
    /// surface until then, as on the frame path).
    pub fn settle_material_graphs_for(&mut self, budget: std::time::Duration) -> bool {
        let _span = tracing::info_span!("material_graph_settle").entered();
        self.graph_cache.settle_until(std::time::Instant::now() + budget)
    }

    /// Material-graph pipeline counters (ready, pending, failed, built on/off the frame path,
    /// fallback draws).
    #[must_use]
    pub fn material_graph_stats(&self) -> crate::GraphPipelineStats {
        self.graph_cache.stats()
    }

    /// Installs the shader-permutation guard (master plan § 5.5a, ADR-2026): a material graph
    /// whose pipeline set is not on the cooked list of the guard's profile is built with a
    /// warning ([`crate::PermutationEnforcement::Warn`]) or never built, its draws keeping the
    /// standard surface of the same pass ([`crate::PermutationEnforcement::Fallback`]). Sets
    /// already built stay. [`crate::PermutationGuard::off`] (the default) checks nothing, so
    /// frames are unchanged.
    pub fn set_permutation_guard(&mut self, guard: crate::PermutationGuard) {
        self.graph_cache.set_guard(guard);
        self.installed_since.permutation_guard = true;
    }

    /// Whether a guard was installed on this renderer ([`Self::set_permutation_guard`]) since it
    /// last came back from a warm: the startup install leaves that one in place.
    pub(crate) fn permutation_guard_installed(&self) -> bool {
        self.installed_since.permutation_guard
    }

    /// The permutation guard's counters (`render.permutations`).
    #[must_use]
    pub fn permutation_stats(&self) -> crate::PermutationGuardStats {
        self.graph_cache.guard_stats()
    }

    /// Targets the frame path has drawn material graphs on (prewarm these for a new level).
    #[must_use]
    pub fn material_graph_targets(&self) -> Vec<crate::GraphTarget> {
        self.graph_cache.targets_seen().to_vec()
    }

    pub(super) fn ensure_graph_layouts(&mut self, d: &wgpu::Device) -> Result<(), RenderError> {
        if self.graph_layouts.is_some() {
            return Ok(());
        }
        let Some(pipes) = self.pipes.as_ref() else {
            return Err(RenderError::Shader(
                "pipelines missing before graph layouts".into(),
            ));
        };
        let (color, depth, objects) = self
            .culler
            .graph_layouts(d, &pipes.view_bgl, &pipes.graph_mat_bgl)?;
        let (velocity_frame, velocity_objects) = self.taau_pass.velocity_layouts(d);
        let layout = |label, groups: &[Option<&wgpu::BindGroupLayout>]| {
            d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: groups,
                immediate_size: 0,
            })
        };
        self.graph_layouts = Some(crate::graph_pipelines::GraphLayouts {
            forward: pipes.graph_forward_layout.clone(),
            shadow: layout(
                "graph shadow layout",
                &[
                    Some(&pipes.shadow_bgl),
                    Some(&pipes.object_bgl),
                    Some(&pipes.graph_mat_bgl),
                ],
            ),
            instanced: crate::graph_pipelines::InstancedLayouts {
                color,
                depth,
                shadow: layout(
                    "graph instanced shadow layout",
                    &[
                        Some(&pipes.shadow_bgl),
                        Some(&objects),
                        Some(&pipes.graph_mat_bgl),
                    ],
                ),
            },
            velocity: layout(
                "graph velocity layout",
                &[
                    Some(&velocity_frame),
                    Some(&velocity_objects),
                    Some(&pipes.graph_mat_bgl),
                ],
            ),
        });
        Ok(())
    }

    /// Makes a level's draw variants resident before gameplay (`specs/27` § 2.4: no frame
    /// creates a buffer or bind group after warm-up).
    ///
    /// Uploads the geometry and material (textures, uniform, bind group) of every mesh in
    /// `meshes` now — pass one [`FrameMesh`] per mesh × material variant the level can draw
    /// (set pieces, every scatter LOD and rock variant, effects' meshes) — and keeps them
    /// resident: the idle-frame eviction skips them. Sizes the per-instance buffers (GPU cull,
    /// instanced shadow/depth objects, motion vectors, decal receivers) for `max_instances`
    /// meshes per frame, and the staging belt for their uploads (640 B each, at most 32 MiB,
    /// plus the rest of its ring and its spill).
    /// Material graphs among `meshes` start their pipeline builds
    /// ([`Self::prewarm_material_graphs`]) for every target drawn so far plus
    /// [`crate::GraphTarget::HDR`]. A double-sided graph is also built for that target with
    /// culling off. A transmissive standard material also prewarms the `GLASS`
    /// pipelines, including the cull-none set a double-sided pane draws with
    /// ([`Self::prewarm_glass`]), and a double-sided material the cull-none forward, depth and
    /// shadow pipelines, instanced variants included ([`Self::prewarm_double_sided`]), so the
    /// frame path does not compile them. A filmed material builds its `THIN_FILM` variant. The
    /// velocity pass (back-face and cull-none) is built
    /// here too. Other pipelines are built by the first frame or [`crate::SharedPassDraw`]
    /// prewarms.
    ///
    /// # Errors
    ///
    /// The forward pipelines failed to build.
    #[tracing::instrument(skip_all)]
    pub fn prewarm(
        &mut self,
        device: &RenderDevice,
        meshes: &[FrameMesh<'_>],
        max_instances: u32,
    ) -> Result<(), RenderError> {
        let d = &device.device;
        let q = crate::device::TrackedQueue::observe(&device.queue);
        crate::pipeline_warm::warm_checkpoint()?;
        if self.pipes.is_none() {
            self.pipes = Some(build_pipes(
                d,
                &q,
                device.shared_pipelines(),
                self.material_anisotropy(),
            )?);
        }
        crate::pipeline_warm::warm_checkpoint()?;
        let now = self.frame_index;
        for mesh in meshes {
            if mesh.vertices.is_empty() || mesh.indices.is_empty() {
                continue;
            }
            self.ensure_mesh(d, &q, mesh, now, false)?;
            self.ensure_material(d, &q, mesh.material_key, mesh.material, now)?;
            self.pinned.insert(mesh.mesh_key);
            self.pinned.insert(mesh.material_key);
        }
        self.reserve_instances(max_instances);
        // The frame's uploads (instance lists, motion vectors, objects) for that many meshes:
        // the staging belt grows on the frame path otherwise (a dense scatter view).
        self.belt.warm_ring(d, belt_reserve_bytes(max_instances));
        // The skin palette every object group binds (first frame uploads its contents).
        if self.skin_buf.is_none() {
            let capacity = self.skin_capacity.max(1024);
            self.skin_buf = Some(make_buffer(
                d,
                "skin palette",
                (size_of::<JointMatrix>() * capacity) as u64,
                wgpu::BufferUsages::STORAGE,
            ));
            self.skin_capacity = capacity;
            self.skin_generation += 1;
            self.skin_bytes.clear();
            self.objects.clear();
        }
        if let Some(skin) = self.skin_buf.clone() {
            self.decal_receiver.prewarm(d, &skin)?;
        }
        crate::pipeline_warm::warm_checkpoint()?;
        let mut graphs: Vec<&crate::SurfaceGraph> = Vec::new();
        for graph in meshes
            .iter()
            .filter_map(|mesh| mesh.material.surface_graph.as_deref())
        {
            if !graphs.iter().any(|known| known.key == graph.key) {
                graphs.push(graph);
            }
        }
        if !graphs.is_empty() {
            let mut targets = self.material_graph_targets();
            if !targets.contains(&crate::GraphTarget::HDR) {
                targets.push(crate::GraphTarget::HDR);
            }
            self.prewarm_material_graphs(device, &graphs, &targets)?;
        }
        // Cull-none sets for graphs whose material is double-sided. The frame looks these
        // up separately from the back-face set of the same code.
        let mut graphs_ds: Vec<&crate::SurfaceGraph> = Vec::new();
        for graph in meshes
            .iter()
            .filter(|mesh| mesh.material.double_sided)
            .filter_map(|mesh| mesh.material.surface_graph.as_deref())
        {
            if !graphs_ds.iter().any(|known| known.key == graph.key) {
                graphs_ds.push(graph);
            }
        }
        if !graphs_ds.is_empty() {
            let mut seen = self.material_graph_targets();
            if !seen.contains(&crate::GraphTarget::HDR) {
                seen.push(crate::GraphTarget::HDR);
            }
            let mut ds_targets = Vec::new();
            for target in seen {
                let ds = crate::GraphTarget {
                    double_sided: true,
                    ..target
                };
                if !ds_targets.contains(&ds) {
                    ds_targets.push(ds);
                }
            }
            self.prewarm_material_graphs(device, &graphs_ds, &ds_targets)?;
        }
        // Both velocity pipelines, so a later double-sided draw compiles none.
        crate::pipeline_warm::warm_checkpoint()?;
        self.taau_pass.share_pipelines(device.shared_pipelines());
        self.taau_pass.prewarm(d)?;
        // The compute skinning kernel (one per device), so enabling it mid-play compiles none.
        self.skin_compute.prewarm(device);
        crate::pipeline_warm::warm_checkpoint()?;
        if meshes
            .iter()
            .any(|mesh| mesh.material.transmission > 0.0 && mesh.material.surface_graph.is_none())
        {
            self.prewarm_glass(device, GlassPipelineTarget::HDR)?;
            // Mobile HDR (Rg11b10) when the device can render it. Same shader as
            // [`GlassPipelineTarget::HDR`] (`encode` is false for every linear target); only the
            // attachment format differs.
            if device.capabilities.rg11b10_renderable {
                self.prewarm_glass(
                    device,
                    GlassPipelineTarget {
                        format: wgpu::TextureFormat::Rg11b10Ufloat,
                        encode: false,
                        samples: 1,
                    },
                )?;
            }
        }
        crate::pipeline_warm::warm_checkpoint()?;
        // The thin film's variants, so a level's filmed material shows the film on its first frame.
        let filmed: Vec<&LabMaterial> = meshes
            .iter()
            .map(|mesh| mesh.material)
            .filter(|material| material.thin_film.is_on())
            .collect();
        if !filmed.is_empty() {
            self.prewarm_thin_film(device, GlassPipelineTarget::HDR, &filmed)?;
        }
        // The scattering variants, once a frame has shown this renderer's profile scatters. The
        // first frames of a new renderer build them where they may wait, or draw without the blur.
        if self.subsurface.wanted() {
            let scattering: Vec<&LabMaterial> = meshes
                .iter()
                .map(|mesh| mesh.material)
                .filter(|material| material.subsurface.is_on())
                .collect();
            self.prewarm_subsurface_materials(device, GlassPipelineTarget::HDR, &scattering)?;
        }
        // The translucent variants, likewise once a frame has asked for them (ADR-2065).
        if self.oit.wanted() {
            let translucent: Vec<&LabMaterial> = meshes.iter().map(|mesh| mesh.material).collect();
            self.prewarm_oit_materials(device, GlassPipelineTarget::HDR, &translucent)?;
        }
        crate::pipeline_warm::warm_checkpoint()?;
        if meshes
            .iter()
            .any(|mesh| mesh.material.double_sided && mesh.material.surface_graph.is_none())
        {
            self.prewarm_double_sided(device, GlassPipelineTarget::HDR)?;
            if device.capabilities.rg11b10_renderable {
                self.prewarm_double_sided(
                    device,
                    GlassPipelineTarget {
                        format: wgpu::TextureFormat::Rg11b10Ufloat,
                        encode: false,
                        samples: 1,
                    },
                )?;
            }
        }
        Ok(())
    }

    /// Builds the cull-none pipelines a double-sided standard material draws with in a frame
    /// that culls back faces, for the forward target `target`: the per-object set (blended,
    /// skinned and non-GPU-driven draws) and, on a GPU-driven device, the instanced colour,
    /// depth-prepass and shadow variants its batches use, plus the visibility buffer's cull-none
    /// camera and shadow rasters when a meshlet source is set.
    ///
    /// [`Self::prewarm`] calls this for [`GlassPipelineTarget::HDR`] (and `Rg11b10` when the
    /// device can render it) whenever a level mesh has a double-sided standard material. A frame
    /// that needs a pipeline this did not build still compiles it once, on that frame.
    ///
    /// # Errors
    ///
    /// The forward or shadow shaders failed to build.
    #[tracing::instrument(skip_all)]
    pub fn prewarm_double_sided(
        &mut self,
        device: &RenderDevice,
        target: GlassPipelineTarget,
    ) -> Result<(), RenderError> {
        let d = &device.device;
        if self.pipes.is_none() {
            let q = crate::device::TrackedQueue::observe(&device.queue);
            self.pipes = Some(build_pipes(
                d,
                &q,
                device.shared_pipelines(),
                self.material_anisotropy(),
            )?);
        }
        let pipes = self
            .pipes
            .as_mut()
            .ok_or_else(|| RenderError::Shader("double-sided prewarm: pipelines missing".into()))?;
        let samples = if target.samples >= 4 { 4 } else { 1 };
        let sun_rt = self.culler.forward_sun_rt();
        // The studio axis the next frame composes (ADR-2308); the culler's colour pipelines
        // below follow it.
        let studio_off = forward_studio::studio_off_of(&self.studio, self.ibl.as_ref());
        self.culler.set_forward_studio_off(studio_off);
        let key = PipelineKey {
            format: target.format,
            encode: target.encode,
            double_sided: true,
            samples,
            sun_rt,
            studio_off,
            restir_di: false,
        };
        if !pipes.sets.contains_key(&key) {
            let built = build_pipeline_set(d, pipes, key);
            pipes.sets.insert(key, built);
        }
        if !crate::gpu_driven::IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
            return Ok(());
        }
        // A frame warm stops between these compiles when its queue shuts down.
        crate::pipeline_warm::warm_checkpoint()?;
        let (view_bgl, mat_bgl, shadow_bgl) = (
            pipes.view_bgl.clone(),
            pipes.mat_bgl.clone(),
            pipes.shadow_bgl.clone(),
        );
        for mask in [false, true] {
            self.culler.color_pipeline(
                d,
                target.format,
                target.encode,
                true,
                samples,
                mask,
                true,
                &view_bgl,
                &mat_bgl,
            )?;
            crate::pipeline_warm::warm_checkpoint()?;
            self.culler
                .instanced_depth_pipeline(d, &view_bgl, &mat_bgl, true, samples, mask)?;
            crate::pipeline_warm::warm_checkpoint()?;
        }
        self.culler.instanced_shadow_pipeline(d, &shadow_bgl, true)?;
        // Alpha-tested casters (`set_alpha_tested_shadows`): both cull modes, so the first frame
        // with a masked caster compiles nothing.
        if self.alpha_tested_shadows {
            for double_sided in [false, true] {
                crate::pipeline_warm::warm_checkpoint()?;
                self.culler
                    .instanced_mask_shadow_pipeline(d, &shadow_bgl, &mat_bgl, double_sided)?;
            }
        }
        // The visibility buffer's cull-none rasters, when meshlets can reach it.
        if self.visbuffer.has_sources() && crate::visbuffer::VisBufferPass::supported(d) {
            self.visbuffer.prewarm_double_sided(d)?;
        }
        Ok(())
    }

    /// Builds the `GLASS` transmission pipelines for `target`: the back-face-culled set and the
    /// cull-none set a double-sided glass material draws with.
    ///
    /// [`Self::prewarm`] calls this for [`GlassPipelineTarget::HDR`] (and `Rg11b10` when the
    /// device can render it) whenever a level mesh is a transmissive standard material. A frame
    /// that needs a set this did not build still compiles it once, on that frame.
    ///
    /// # Errors
    ///
    /// The forward shader or a glass pipeline failed to build.
    #[tracing::instrument(skip_all)]
    pub fn prewarm_glass(
        &mut self,
        device: &RenderDevice,
        target: GlassPipelineTarget,
    ) -> Result<(), RenderError> {
        let d = &device.device;
        if self.pipes.is_none() {
            let q = crate::device::TrackedQueue::observe(&device.queue);
            self.pipes = Some(build_pipes(
                d,
                &q,
                device.shared_pipelines(),
                self.material_anisotropy(),
            )?);
        }
        let pipes = self
            .pipes
            .as_mut()
            .ok_or_else(|| RenderError::Shader("glass prewarm: pipelines missing".into()))?;
        let samples = if target.samples >= 4 { 4 } else { 1 };
        pipes.glass.prewarm(
            d,
            [&pipes.view_bgl, &pipes.object_bgl, &pipes.mat_bgl],
            PipelineKey {
                format: target.format,
                encode: target.encode,
                double_sided: false,
                samples,
                sun_rt: false,
                studio_off: false,
                restir_di: false,
            },
        )
    }

    /// Builds the pipelines the first projected decal would otherwise compile on the frame
    /// path: the decal receiver's stamp variants (scene-depth tested or not, back-face culled
    /// and double-sided) and, once the skin palette exists, its bind group. [`Self::prewarm`]
    /// includes this; a host that warms on a [`crate::PipelineWarmQueue`] worker
    /// ([`crate::PipelineWarmQueue::warm_renderer`]) calls it there. The effects' own decal
    /// pipelines belong to the effect layer (`regia_vfx::VfxGpu::warm`). The engine's game-frame
    /// warm ([`crate::GameFrameWarm`]) calls it when translucent layers are installed.
    ///
    /// # Errors
    ///
    /// A receiver pipeline failed to build, or the warm queue running this shut down
    /// ([`crate::warm_cancelled`]).
    #[tracing::instrument(skip_all)]
    pub fn prewarm_decals(&mut self, device: &RenderDevice) -> Result<(), RenderError> {
        crate::pipeline_warm::warm_checkpoint()?;
        let d = &device.device;
        match self.skin_buf.clone() {
            Some(skin) => self.decal_receiver.prewarm(d, &skin),
            None => self.decal_receiver.prewarm_pipelines(d),
        }
    }

    /// Builds the `GLASS` pipelines a [`crate::glass::GlassInstance`] (own-canopy view, crack
    /// damage) draws with, both cull modes, for every forward target this renderer has
    /// pipelines for (the targets its frames and prewarms drew; [`GlassPipelineTarget::HDR`]
    /// before any). Run it after a warm frame of the map: the first glass instance (a canopy
    /// entered, a pane cracked) then compiles nothing on the frame path. Returns how many
    /// targets were warmed. The engine's game-frame warm calls it when the frame has a
    /// transmissive standard material.
    ///
    /// # Errors
    ///
    /// A glass pipeline failed to build, or the warm queue running this shut down
    /// ([`crate::warm_cancelled`]).
    #[tracing::instrument(skip_all)]
    pub fn prewarm_glass_instances(&mut self, device: &RenderDevice) -> Result<usize, RenderError> {
        let mut targets: Vec<GlassPipelineTarget> = Vec::new();
        if let Some(pipes) = self.pipes.as_ref() {
            for key in pipes.sets.keys() {
                let target = GlassPipelineTarget {
                    format: key.format,
                    encode: key.encode,
                    samples: key.samples,
                };
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
        if targets.is_empty() {
            targets.push(GlassPipelineTarget::HDR);
        }
        for target in &targets {
            crate::pipeline_warm::warm_checkpoint()?;
            self.prewarm_glass(device, *target)?;
        }
        Ok(targets.len())
    }

    /// The engine's frame warm ([`Self::warm_extracted`]): the decal receiver when effect
    /// layers are installed (or [`Self::set_warm_decals`]), the glass-instance sets when `glass` (the frame can draw one).
    pub(crate) fn prewarm_frame_effects(
        &mut self,
        device: &RenderDevice,
        glass: bool,
    ) -> Result<(), RenderError> {
        if !self.translucent.is_empty() || self.warm_decals {
            self.prewarm_decals(device)?;
        }
        if glass {
            self.prewarm_glass_instances(device)?;
            // The refraction copy is a pooled target (ADR-2018): taken now for the HDR targets
            // the warm drew, so the first refracting frame of play creates no texture.
            self.prewarm_refraction_copies(&device.tracked());
        }
        crate::pipeline_warm::warm_checkpoint()?;
        self.warm_shared_draw();
        self.prewarm_double_sided_sets(device)?;
        Ok(())
    }

    /// Game-frame warms: the pipelines play can switch on that the warm frame itself does not
    /// draw (ADR-220). The TSR upscaler (a settings or quality-tier change to TSR), the depth
    /// match for single-sample and MSAA sources (a scaled frame with effects after a native
    /// warm frame, or a switch to an MSAA tier) and every outdoor pass for the HDR targets the warm drew
    /// (the next map has clouds or froxel fog the warm frame's map has not) are device
    /// pipelines, so a stand-in or a second renderer finds them as well. TAAU's resolve and
    /// sharpen, built when the warm frame used TSR (a switch back), and the half-resolution
    /// SSGI upsample of a frame at exactly 0.67 (the 4K lever) are this renderer's own.
    /// Before v0.1.14 DO27's rehearsal compiled TSR and the depth match on the frame thread
    /// 5–15 s into play, and the outdoor passes on a second map's first frame. Returns the
    /// pipelines asked for (built now, or already built).
    ///
    /// The frame graph names what to build (ADR-2018): each live built-in pass declares its
    /// pipeline keys ([`FrameStep::Builtin`]'s `pipelines`), and this builds every key once
    /// ([`Self::warm_pipeline_key`]), then the registered passes' own warms.
    pub(crate) fn prewarm_mid_play(&mut self, device: &RenderDevice) -> Result<usize, RenderError> {
        let _span = tracing::info_span!("prewarm_mid_play").entered();
        let keys = self.extension_passes.graph().warm_keys().to_vec();
        let mut asked = 0;
        for key in keys {
            crate::pipeline_warm::warm_checkpoint()?;
            asked += self.warm_pipeline_key(device, key)?;
        }
        // Registered host passes (the lens effects pass, ADR-1863) build their pipelines now.
        crate::pipeline_warm::warm_checkpoint()?;
        asked += self.extension_passes.prewarm(&device.tracked());
        // The `LIGHTMAP` variants, when this renderer has a lightmap (ADR-2107).
        asked += self.prewarm_lightmap(device)?;
        // The GPU-driven cull pipelines on a device that draws GPU-driven, also when the warm
        // frame had no batch for them (ADR-7885).
        if crate::gpu_driven::IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
            crate::pipeline_warm::warm_checkpoint()?;
            asked += usize::from(self.culler.prewarm_pipes(&device.device)?);
        }
        Ok(asked)
    }

    /// Builds the pipelines a frame-graph pipeline key names (`FRAME_STEPS`; ADR-2018) and
    /// returns how many it asked for. An unknown key builds nothing (a registered pass's keys
    /// are listed for `render.graph`; its own [`FramePass::prewarm`] builds them).
    pub(crate) fn warm_pipeline_key(
        &mut self,
        device: &RenderDevice,
        key: &str,
    ) -> Result<usize, RenderError> {
        if !WARM_KEYS.contains(&key) {
            return Ok(0);
        }
        let d = &device.device;
        let shared = device.shared_pipelines();
        Ok(match key {
            // The TSR upscaler: a settings or quality-tier change to TSR.
            "tsr" => {
                crate::tsr::shared_pipelines(d, shared)?;
                4
            }
            // TAAU's resolve and sharpen when the warm frame used TSR (a switch back to TAAU).
            "taau_resolve" => {
                self.taau_pass.prewarm_resolve(d)?;
                1
            }
            // The half-resolution SSGI upsample of a frame at 0.67 (the 4K lever).
            "ssgi_half_res" => {
                self.highend.prewarm_half_res(d)?;
                1
            }
            // Both sources, whatever the warm frame's sample count: a switch to an MSAA tier
            // mid-play (v0.1.14 review) must not compile the multisampled variant on the frame
            // thread.
            "depth_match" | "depth_match_msaa" => {
                depth_match_pipe(d, shared, key == "depth_match_msaa")?;
                1
            }
            // The radiance cache's update and gather (ADR-2085): nothing unless the profile asks.
            "radiance_cache" => self.prewarm_radiance_cache(d)?,
            // Spatiotemporal blue noise (ADR-2075): nothing unless the profile asks for it.
            "blue_noise" => match self.blue_noise.active().cloned() {
                Some(noise) => {
                    self.ssao_pass.set_blue_noise(d, Some(&noise))?;
                    self.highend.set_blue_noise(Some(noise));
                    2 + self.highend.prewarm_blue_noise(d)?
                }
                None => 0,
            },
            // The sun trace (ADR-2035): nothing unless the profile asks for it.
            "sun_shadow_trace" => self.prewarm_sun_trace(device)?,
            // Many-light sampling (ADR-2652): nothing unless the profile asks for it.
            "restir_di" => self.prewarm_many_light(device)?,
            // The visibility path's pipelines (ADR-2071): nothing unless the profile selects
            // it by density, where the warm frame itself may not draw through it.
            "visbuffer_path" => self.prewarm_visbuffer_path(device)?,
            // The other forward module for every forward target the warm drew: the trace can
            // give way to the shadow map mid-play, and back. Nothing unless the profile asks.
            "forward_sun_rt" => self.prewarm_forward_sun_twins(device)?,
            // The other studio module (ADR-2308) for every forward target the warm drew: a
            // studio feature can turn on or off mid-play. Nothing without a studio environment.
            "forward_studio" => self.prewarm_forward_studio_twins(device)?,
            // The scattering pipelines. Nothing unless a material asks under a profile that does.
            "subsurface" => self.prewarm_subsurface(device)?,
            // The translucent pipelines. Nothing unless a material asks under a profile that does.
            "oit" => self.prewarm_oit(device)?,
            // Every outdoor pass for the HDR targets the warm drew.
            "outdoor" => {
                let mut asked = 0;
                for (format, fog, clouds) in self.outdoor_targets.clone() {
                    asked += self.outdoor.prewarm(d, shared, format, (fog, clouds))?;
                }
                asked
            }
            _ => 0,
        })
    }

    /// The radiance cache's pipelines for the HDR format the warm's SSGI gather drew. Nothing when
    /// the cache is off or the warm frame ran no gather.
    fn prewarm_radiance_cache(&mut self, d: &wgpu::Device) -> Result<usize, RenderError> {
        if !self.radiance_cache.enabled() {
            return Ok(0);
        }
        let Some(format) = self.highend.ssgi_format() else {
            return Ok(0);
        };
        self.radiance_cache.ensure_pipes(d)?;
        if let Some(read_layout) = self.radiance_cache.read_layout() {
            self.highend.ensure_ssgi_cached(d, format, read_layout)?;
        }
        Ok(2)
    }

    /// Game-frame warms: builds the deferred blended (`lab blend`, `lab premul`, `lab additive`)
    /// and transmission (`lab transmit multiply`, `lab transmit add`, `lab transmit add (depth)`,
    /// `lab transmit front layer`, `lab transmit refract`) variants of every forward set this
    /// renderer has, side by side ([`crate::pipeline_warm::join_all`]), on the warm worker
    /// (ADR-7885). A warm frame builds only the variants it draws; the rest used to build on a
    /// warm worker after warm-up (DO27 tester A2: seven of them on the first gameplay frame)
    /// while the first draw that needed one was skipped. The x-ray variant stays on demand.
    /// Returns the variants built now.
    pub(crate) fn prewarm_deferred_variants(&mut self) -> Result<usize, RenderError> {
        crate::pipeline_warm::warm_checkpoint()?;
        let Some(pipes) = self.pipes.as_ref() else {
            return Ok(0);
        };
        let _span = tracing::info_span!("prewarm_deferred_variants").entered();
        let mut wanted: Vec<crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>> = Vec::new();
        for set in pipes.sets.values() {
            for variant in [
                &set.blend,
                &set.premul,
                &set.add,
                &set.transmit_mul,
                &set.transmit_add,
                &set.transmit_add_depth,
                &set.transmit_front,
                &set.transmit_refract,
            ] {
                // A ReSTIR DI set shares its twin's variants: build each object once.
                if !variant.is_built() && !wanted.iter().any(|known| known.same(variant)) {
                    wanted.push(variant.clone());
                }
            }
        }
        // At most this many compiles at once, a quit check between the batches.
        const BATCH: usize = 8;
        let built = wanted.len();
        for batch in wanted.chunks(BATCH) {
            let jobs: Vec<_> = batch
                .iter()
                .cloned()
                .map(|variant| {
                    move || {
                        variant.get();
                    }
                })
                .collect();
            crate::pipeline_warm::join_all(jobs);
            crate::pipeline_warm::warm_checkpoint()?;
        }
        Ok(built)
    }

    /// [`crate::SharedPassDraw::warm_pipelines`] of the installed shared draw (frame warms).
    pub(crate) fn warm_shared_draw(&mut self) {
        if let Some(draw) = self.shared_draw.as_mut() {
            draw.warm_pipelines();
        }
    }
}
