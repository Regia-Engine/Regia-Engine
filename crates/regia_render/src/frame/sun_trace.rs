// SPDX-License-Identifier: MPL-2.0
//! The sun-visibility trace of `FrameRenderer::render_into` (ADR-2035,
//! `crate::hwrt::sun_trace`): the frame's casters, the acceleration structures or BVH they build,
//! and the compute pass that writes the screen visibility texture in place of contact shadows.
//! Everything here stays empty while `RenderProfile::sun_shadow_trace` is off.

use crate::parallel_finish::pass_cuts::SegmentedEncoder as _;

use regia_core::glam::{Mat4, Vec3};

use super::{Draw, FrameMesh, FrameRenderer, LabVertex, MeshEntry, PipelineKey, PipelineSet};
use crate::hwrt::casters::{
    MASKED_CASTER_TAG, MASKED_COVERAGE, MASKED_INSTANCE_MASK, OPAQUE_INSTANCE_MASK, SignatureBuilder,
    coverage_custom_data,
};
use crate::hwrt::sun_trace::{
    RAY_OFFSET_M, RAY_OFFSET_PER_M, SCREEN_STEPS, SUN_TRACE_FORMAT, SunTraceUniform, sun_ray_length,
    sun_trace_groups, sun_trace_pipeline,
};
use crate::hwrt::{
    AccelBuildStats, AccelError, AccelInstance, AccelMesh, FrameTraceSupport, GpuAccel, GpuAccelMesh,
    RtShadowRequest, ShadowBvh, ShadowFallbackScene, SunShadowSelection, SunShadowVariant,
    frame_sun_shadow_selection,
};
use crate::hwrt::{SceneChange, SceneSignature, classify_scene_change};
use crate::lab_frame::ALPHA_MASK;
use crate::{RenderDevice, RenderError, TrackedDevice};

mod scene;

/// What the last main-view frame did about the sun's shadows ([`FrameRenderer::sun_trace_report`],
/// `render.hwrt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SunTraceReport {
    /// The variant and why.
    pub selection: SunShadowSelection,
    /// Main-view frames that have run the trace.
    pub traces: u64,
    /// Times the traced scene was rebuilt (a caster moved, appeared or left).
    pub scene_builds: u64,
    /// Instances in the traced scene at its last build.
    pub casters: u32,
    /// Triangles of the casters' meshes at the last build: every instance's, for the compute BVH;
    /// the bottom-level structures built then, for ray queries.
    pub triangles: u64,
    /// Draws the last build left out because they are skinned, morphed or vertex-animated: their
    /// shadows are missing under the traced variants.
    pub skipped_deforming: u32,
    /// Draws left out because they are alpha-masked: their shadows are missing under the traced
    /// variants.
    pub skipped_masked: u32,
    /// The last acceleration-structure build (zero on the other tracers).
    pub accel: AccelBuildStats,
    /// Why the last build was refused (`accel_*`, `bvh_failed`), if it was.
    pub failure: Option<&'static str>,
    /// Instances of the shared draw's proxy (the terrain) in the last build (ADR-2310).
    pub shared_casters: u32,
    /// Instances of persistent instance sets (scatter) in the last build.
    pub set_casters: u32,
    /// Alpha-masked casters in the last traced-scene build, using the coverage proxy.
    pub masked_casters: u32,
    /// Times only the casters' transforms changed and the top level was refitted in place
    /// (counted apart from [`Self::scene_builds`]).
    pub tlas_refits: u64,
    /// Main-view frames that skipped the sun's cascades because nothing read them.
    pub sun_maps_skipped: u64,
}

/// What [`SunTraceState::record`] reads from the frame.
pub(super) struct SunTraceInputs<'a> {
    pub(super) scene: &'a super::FrameScene<'a>,
    pub(super) variant: SunShadowVariant,
    /// The single-sample scene depth.
    pub(super) depth_view: &'a wgpu::TextureView,
    /// The target's size: the depth's.
    pub(super) size: (u32, u32),
    /// The projection (jitter included) times the view: the one that drew the depth.
    pub(super) view_proj: Mat4,
    /// The pixel rectangle NDC maps to.
    pub(super) viewport: [f32; 4],
    pub(super) meshes: &'a crate::key_hash::KeyMap<u64, MeshEntry>,
    pub(super) draws: &'a [Draw],
}

struct TraceTarget {
    generation: u64,
    storage: wgpu::TextureView,
    sampled: wgpu::TextureView,
}

/// The trace's state in the renderer.
#[derive(Default)]
pub(super) struct SunTraceState {
    report: SunTraceReport,
    /// Whether the last main-view frame's variant writes the visibility texture.
    applied: bool,
    accel: Option<GpuAccel>,
    bvh: Option<ShadowFallbackScene>,
    /// The variant and casters the traced scene was built from.
    scene_hash: Option<(SunShadowVariant, SceneSignature)>,
    /// Casters from outside the mesh list (the shared draw's proxy, persistent sets).
    extra: scene::ExtraCasters,
    /// This frame's variant (main view or not) and whether it is the main view.
    frame_variant: (SunShadowVariant, bool),
    /// The last main-view frame skipped sun cascades: the next frame redraws them whole.
    maps_skipped_last: bool,
    /// The shared draw was told the sun is traced ([`crate::SharedPassDraw::set_engine_sun_traced`]).
    told_traced: bool,
    /// The instance list a refit fills (kept between frames).
    refit_instances: Vec<AccelInstance>,
    /// A build was refused: the frame keeps the shadow map until the request changes.
    accel_failed: bool,
    /// Renewed when the bound scene's buffers change (a new TLAS, or a compute-fallback BVH
    /// that does not fit the buffers already uploaded). A fitting refit writes into those
    /// buffers and leaves the epoch alone.
    scene_epoch: u64,
    target: Option<TraceTarget>,
    params: Option<wgpu::Buffer>,
    param_bytes: Vec<u8>,
    bg_cache: crate::bind_cache::BindCache<(wgpu::TextureView, wgpu::TextureView, u64, SunShadowVariant)>,
}

/// Whether `draw` is in the traced scene. Skinned, morphed and vertex-animated draws have no
/// static geometry to build from; masked ones would shadow as whole cards with this opaque build;
/// translucent ones keep their own shadow path.
fn traced_caster(draw: &Draw) -> bool {
    draw.rigid
        && draw.joint_count == 0
        && !draw.morphed
        && draw.alpha_mode < ALPHA_MASK
        && !draw.transmissive
        && !draw.hair
        && !draw.graph_vertex
        && draw.shadow.casts_sun()
}

/// Whether `draw` enters a traced scene as an alpha-masked proxy (ADR-2310, ADR-4557): a rigid static
/// caster whose only reason to stay out is its alpha mask.
fn masked_caster(draw: &Draw) -> bool {
    draw.rigid
        && draw.joint_count == 0
        && !draw.morphed
        && draw.alpha_mode == ALPHA_MASK
        && !draw.transmissive
        && !draw.hair
        && !draw.graph_vertex
        && draw.shadow.casts_sun()
}

/// What the traced scene is built from, slot by slot in build order: casters (including masked
/// coverage proxies), their meshes' uploads and transforms, then the extra casters. Allocates nothing:
/// a steady frame reads this and stops.
fn scene_signature(
    variant: SunShadowVariant,
    draws: &[Draw],
    meshes: &crate::key_hash::KeyMap<u64, MeshEntry>,
    extra: &scene::ExtraCasters,
) -> SceneSignature {
    let mut builder = SignatureBuilder::new();
    // Ray queries and the software walk both any-hit a masked caster (ADR-4557).
    let masked = variant.traces_rays();
    for draw in draws {
        let flags = if traced_caster(draw) {
            0
        } else if masked && masked_caster(draw) {
            1
        } else {
            continue;
        };
        let generation = meshes.get(&draw.mesh).map_or(0, |entry| entry.generation);
        builder.slot(draw.mesh, generation, flags, &draw.model);
    }
    extra.sign(&mut builder);
    builder.finish()
}

/// `true` when `key` is not already in `seen`. One lookup per instance: the structural rebuild
/// used to scan every mesh already kept.
fn accel_mesh_is_new(seen: &mut std::collections::HashSet<u64>, key: u64, compares: &mut u64) -> bool {
    *compares += 1;
    seen.insert(key)
}

/// The ray-query scene's instances in signature order: the draws' casters whose mesh can feed a
/// build (masked ones as the coverage proxy), then the extra casters; and the meshes drawn masked.
fn accel_instances(
    inputs: &SunTraceInputs<'_>,
    extra: &scene::ExtraCasters,
    instances: &mut Vec<AccelInstance>,
) -> std::collections::HashSet<u64> {
    // A mesh without the usage (a compute-skinning twin's buffer) cannot feed a build.
    let buildable = |draw: &Draw| {
        inputs.meshes.get(&draw.mesh).is_some_and(|entry| {
            entry.static_bound
                && entry.vb.usage().contains(wgpu::BufferUsages::BLAS_INPUT)
                && entry.ib.usage().contains(wgpu::BufferUsages::BLAS_INPUT)
        })
    };
    instances.clear();
    let mut masked = std::collections::HashSet::new();
    for (draw_index, draw) in inputs.draws.iter().enumerate() {
        let opaque = traced_caster(draw);
        if !(opaque || masked_caster(draw)) || !buildable(draw) {
            continue;
        }
        if opaque {
            instances.push(AccelInstance {
                mesh: draw.mesh,
                transform: draw.model,
                // ReSTIR GI reads this as the draw's material-table index + 1 (ADR-3349).
                custom_data: (draw_index + 1) as u32,
                mask: OPAQUE_INSTANCE_MASK,
            });
        } else {
            masked.insert(draw.mesh);
            instances.push(AccelInstance {
                mesh: draw.mesh ^ MASKED_CASTER_TAG,
                transform: draw.model,
                custom_data: coverage_custom_data(MASKED_COVERAGE),
                mask: MASKED_INSTANCE_MASK,
            });
        }
    }
    extra.accel_instances(instances);
    masked
}

/// The key the draw list gives the `index`th submitted mesh.
fn draw_key(index: usize, mesh: &FrameMesh<'_>) -> u64 {
    let key = if mesh.mesh_key == 0 {
        super::EPHEMERAL_KEY | index as u64
    } else {
        mesh.mesh_key
    };
    if mesh.lod_dither == super::LOD_DITHER_NONE {
        key
    } else {
        key ^ super::LOD_DITHER_MESH_TAG
    }
}

impl SunTraceState {
    /// The variant this frame draws, recorded for the report when the frame is the main view. An
    /// off request reads nothing of the frame (`support` is not called) and releases what the
    /// trace held.
    pub(super) fn select_for_frame(
        &mut self,
        request: RtShadowRequest,
        main_view: bool,
        support: impl FnOnce() -> FrameTraceSupport,
    ) -> SunShadowVariant {
        if request == RtShadowRequest::Off {
            if main_view && self.report.selection.request != RtShadowRequest::Off {
                *self = Self::default();
            }
            return SunShadowVariant::ShadowMap;
        }
        if main_view && self.report.selection.request != request {
            self.accel_failed = false;
        }
        let support = FrameTraceSupport {
            accel_failed: self.accel_failed,
            ..support()
        };
        let selection = frame_sun_shadow_selection(request, support, None);
        if main_view {
            self.report.selection = selection;
            self.applied = selection.variant.writes_visibility();
        }
        selection.variant
    }

    /// The report ([`FrameRenderer::sun_trace_report`]).
    pub(super) fn report(&self) -> SunTraceReport {
        self.report
    }

    /// The pooled target of this frame ([`crate::TransientPool`], ADR-2018), the view the forward
    /// pass samples. [`Self::record`] writes it.
    pub(super) fn prepare(
        &mut self,
        d: &TrackedDevice,
        pool: &mut crate::TransientPool,
        width: u32,
        height: u32,
    ) -> wgpu::TextureView {
        let desc = crate::TextureDesc::new(width.max(1), height.max(1), SUN_TRACE_FORMAT).with_usage(
            wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | crate::resource_inspector::debug_usage(),
        );
        let pooled = pool.acquire(d, &desc, "sun trace");
        match self.target.as_ref() {
            Some(target) if target.generation == pooled.generation => target.sampled.clone(),
            _ => {
                let view = |usage: wgpu::TextureUsages| {
                    pooled.texture.create_view(&wgpu::TextureViewDescriptor {
                        label: Some("sun trace"),
                        format: Some(SUN_TRACE_FORMAT),
                        usage: Some(usage),
                        ..Default::default()
                    })
                };
                let target = TraceTarget {
                    generation: pooled.generation,
                    storage: view(wgpu::TextureUsages::STORAGE_BINDING),
                    sampled: view(wgpu::TextureUsages::TEXTURE_BINDING),
                };
                let sampled = target.sampled.clone();
                self.target = Some(target);
                self.bg_cache.clear();
                sampled
            }
        }
    }

    /// Rebuilds the traced scene when its casters changed; `false` when it was refused.
    fn update_scene(
        &mut self,
        d: &TrackedDevice,
        queue: &crate::device::TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        inputs: &SunTraceInputs<'_>,
    ) -> bool {
        let hash = scene_signature(inputs.variant, inputs.draws, inputs.meshes, &self.extra);
        let previous = self
            .scene_hash
            .filter(|(variant, _)| *variant == inputs.variant)
            .map(|(_, signature)| signature);
        let change = classify_scene_change(previous, hash);
        if change == SceneChange::Unchanged {
            return true;
        }
        let _span = tracing::info_span!(
            "sun_trace_scene",
            variant = inputs.variant.as_str(),
            change = change.as_str()
        )
        .entered();
        // Moved casters only: the top level takes the new transforms in place (ADR-2310).
        if change == SceneChange::Refit
            && inputs.variant == SunShadowVariant::RayQuery
            && let Some(accel) = self.accel.as_mut()
        {
            // The refit path reuses the kept list, so moving casters allocate nothing per frame.
            accel_instances(inputs, &self.extra, &mut self.refit_instances);
            match accel.refit(encoder, &self.refit_instances) {
                Ok(stats) => {
                    self.report.accel = stats;
                    self.report.tlas_refits += 1;
                    self.scene_hash = Some((inputs.variant, hash));
                    return true;
                }
                Err(crate::hwrt::AccelError::RefitShape) => {}
                Err(e) => {
                    return self.refuse(e.code());
                }
            }
        }
        self.report.skipped_deforming = inputs
            .draws
            .iter()
            .filter(|draw| !draw.rigid || draw.joint_count > 0 || draw.morphed || draw.graph_vertex)
            .count() as u32;
        self.report.skipped_masked = inputs
            .draws
            .iter()
            .filter(|draw| draw.rigid && !draw.morphed && draw.alpha_mode == ALPHA_MASK)
            .count() as u32;
        let masked_traced = inputs.variant.traces_rays();
        if masked_traced {
            self.report.skipped_masked = 0;
        }
        let built = match inputs.variant {
            SunShadowVariant::RayQuery => self.build_accel(d, encoder, inputs),
            SunShadowVariant::ComputeFallback => self.build_bvh(d, queue, inputs),
            SunShadowVariant::ShadowMap | SunShadowVariant::ScreenTrace => Ok(()),
        };
        match built {
            Ok(()) => {
                self.report.failure = None;
                self.report.scene_builds += 1;
                self.scene_hash = Some((inputs.variant, hash));
                true
            }
            Err(code) => self.refuse(code),
        }
    }

    /// A refused scene: the shadow map takes over from the next frame.
    fn refuse(&mut self, code: &'static str) -> bool {
        tracing::warn!(
            target: "regia_render",
            code,
            "sun trace: the scene was refused; the shadow map takes over from the next frame"
        );
        self.report.failure = Some(code);
        self.accel_failed = true;
        self.scene_hash = None;
        false
    }

    fn build_accel(
        &mut self,
        d: &TrackedDevice,
        encoder: &mut wgpu::CommandEncoder,
        inputs: &SunTraceInputs<'_>,
    ) -> Result<(), &'static str> {
        let accel = match self.accel.as_mut() {
            Some(accel) => accel,
            None => {
                let accel = GpuAccel::new(d).map_err(|e: AccelError| e.code())?;
                self.accel.insert(accel)
            }
        };
        self.extra.upload(d);
        let mut instances = Vec::new();
        let masked = accel_instances(inputs, &self.extra, &mut instances);
        let mut meshes: Vec<AccelMesh> = Vec::new();
        let mut buffers: Vec<GpuAccelMesh<'_>> = Vec::new();
        let mut seen_meshes = std::collections::HashSet::new();
        let mut mesh_compares = 0u64;
        for (key, entry) in inputs.meshes.iter().filter(|(_, entry)| entry.static_bound) {
            let mesh = AccelMesh {
                key: *key,
                generation: entry.generation,
                vertex_count: (entry.vertex_bytes / std::mem::size_of::<LabVertex>()) as u32,
                index_count: entry.index_count,
            };
            meshes.push(mesh);
            // The masked twin: the same buffers, built non-opaque (only when an instance uses it).
            if masked.contains(key) {
                meshes.push(AccelMesh {
                    key: key ^ MASKED_CASTER_TAG,
                    ..mesh
                });
            }
        }
        for instance in &instances {
            let opaque = instance.mask == OPAQUE_INSTANCE_MASK;
            let draw_key = if opaque {
                instance.mesh
            } else {
                instance.mesh ^ MASKED_CASTER_TAG
            };
            if !accel_mesh_is_new(&mut seen_meshes, instance.mesh, &mut mesh_compares) {
                continue;
            }
            if let Some(entry) = inputs.meshes.get(&draw_key) {
                buffers.push(GpuAccelMesh {
                    key: instance.mesh,
                    vertex_buffer: &entry.vb,
                    first_vertex: entry.base_vertex as u32,
                    vertex_stride: std::mem::size_of::<LabVertex>() as u64,
                    index_buffer: Some(&entry.ib),
                    first_index: entry.first_index,
                    opaque,
                });
            }
        }
        self.extra.accel_meshes(&mut meshes, &mut buffers);
        let stats = accel
            .build(d, encoder, &meshes, &buffers, &instances)
            .map_err(|e| e.code())?;
        if stats.tlas_recreated {
            self.scene_epoch += 1;
        }
        self.report.casters = stats.instances as u32;
        self.report.triangles = stats.triangles_built;
        self.report.accel = stats;
        self.report.masked_casters = instances
            .iter()
            .filter(|i| i.mask == MASKED_INSTANCE_MASK)
            .count() as u32;
        self.report.shared_casters = self.extra.shared;
        self.report.set_casters = self.extra.sets;
        Ok(())
    }

    fn build_bvh(
        &mut self,
        d: &TrackedDevice,
        queue: &crate::device::TrackedQueue,
        inputs: &SunTraceInputs<'_>,
    ) -> Result<(), &'static str> {
        let submitted: crate::key_hash::KeyMap<u64, &FrameMesh<'_>> = inputs
            .scene
            .meshes
            .iter()
            .enumerate()
            .map(|(index, mesh)| (draw_key(index, mesh), mesh))
            .collect();
        let mut triangles: Vec<[[f32; 3]; 3]> = Vec::new();
        let mut coverage: Vec<f32> = Vec::new();
        let mut casters = 0u32;
        let mut masked_casters = 0u32;
        for draw in inputs.draws {
            let opaque = traced_caster(draw);
            let masked = masked_caster(draw);
            if !opaque && !masked {
                continue;
            }
            let Some(mesh) = submitted.get(&draw.mesh) else {
                continue;
            };
            casters += 1;
            if masked {
                masked_casters += 1;
            }
            let fraction = if masked { MASKED_COVERAGE } else { 0.0 };
            let model = Mat4::from_cols_array_2d(&draw.model);
            let world = |index: u32| -> Option<[f32; 3]> {
                let vertex = mesh.vertices.get(index as usize)?;
                Some(
                    model
                        .transform_point3(Vec3::from_array(vertex.position))
                        .to_array(),
                )
            };
            let (triangle_indices, _) = mesh.indices.as_chunks::<3>();
            for t in triangle_indices {
                if let (Some(a), Some(b), Some(c)) = (world(t[0]), world(t[1]), world(t[2])) {
                    triangles.push([a, b, c]);
                    coverage.push(fraction);
                }
            }
        }
        casters += self.extra.bvh_triangles(&mut triangles);
        coverage.resize(triangles.len(), 0.0);
        let bvh = ShadowBvh::build_with_coverage(&triangles, &coverage).map_err(|_| "bvh_failed")?;
        self.report.shared_casters = self.extra.shared;
        self.report.set_casters = self.extra.sets;
        self.report.masked_casters = masked_casters;
        self.report.casters = casters;
        self.report.triangles = triangles.len() as u64;
        ShadowFallbackScene::install(&mut self.bvh, &mut self.scene_epoch, d, queue, &bvh);
        Ok(())
    }

    /// Records the trace over the frame's depth: the scene when it changed, then the pass. A
    /// refused scene traces through the depth buffer this frame (a frame must fill the texture
    /// it binds) and hands over to the shadow map from the next.
    pub(super) fn record(
        &mut self,
        d: &TrackedDevice,
        queue: &crate::device::TrackedQueue,
        shared: &std::sync::Arc<crate::device_pipelines::DevicePipelines>,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        inputs: &SunTraceInputs<'_>,
    ) -> Result<(), RenderError> {
        let _span = tracing::info_span!("sun_shadow_trace", variant = inputs.variant.as_str()).entered();
        let _crumb = crate::breadcrumbs::frame_pass("sun_shadow_trace");
        let variant = if self.update_scene(d, queue, encoder, inputs) {
            inputs.variant
        } else {
            SunShadowVariant::ScreenTrace
        };
        let pipe = sun_trace_pipeline(d, shared, variant)?;
        let Some(target) = self.target.as_ref() else {
            return Err(RenderError::Shader("sun trace target missing".into()));
        };
        let scene = inputs.scene;
        let to_sun = -Vec3::from_array(scene.directional.direction).normalize_or_zero();
        let params = SunTraceUniform {
            inv_view_proj: inputs.view_proj.inverse().to_cols_array_2d(),
            view_proj: inputs.view_proj.to_cols_array_2d(),
            to_sun: [to_sun.x, to_sun.y, to_sun.z, sun_ray_length(scene.far)],
            eye: [scene.eye[0], scene.eye[1], scene.eye[2], scene.near],
            viewport: inputs.viewport,
            tune: [RAY_OFFSET_M, RAY_OFFSET_PER_M, scene.far, 0.0],
            counts: [
                self.bvh.as_ref().map_or(0, ShadowFallbackScene::node_count),
                SCREEN_STEPS,
                0,
                0,
            ],
        };
        let params_buf = self
            .params
            .get_or_insert_with(|| {
                d.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("sun trace params"),
                    size: std::mem::size_of::<SunTraceUniform>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                })
            })
            .clone();
        belt.write_if_changed(
            d,
            encoder,
            &params_buf,
            bytemuck::bytes_of(&params),
            &mut self.param_bytes,
        );
        let tlas = self.accel.as_ref().and_then(GpuAccel::tlas);
        let bvh = self.bvh.as_ref();
        let bg = self.bg_cache.get_or_insert_with(
            (
                inputs.depth_view.clone(),
                target.storage.clone(),
                self.scene_epoch,
                variant,
            ),
            || {
                let mut entries = vec![
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: params_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(inputs.depth_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&target.storage),
                    },
                ];
                match variant {
                    SunShadowVariant::RayQuery => {
                        if let Some(tlas) = tlas {
                            entries.push(wgpu::BindGroupEntry {
                                binding: 3,
                                resource: tlas.as_binding(),
                            });
                        }
                    }
                    SunShadowVariant::ComputeFallback => {
                        if let Some((nodes, triangles)) = bvh.map(ShadowFallbackScene::buffers) {
                            entries.push(wgpu::BindGroupEntry {
                                binding: 3,
                                resource: nodes.as_entire_binding(),
                            });
                            entries.push(wgpu::BindGroupEntry {
                                binding: 4,
                                resource: triangles.as_entire_binding(),
                            });
                        }
                    }
                    SunShadowVariant::ShadowMap | SunShadowVariant::ScreenTrace => {}
                }
                d.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("sun trace"),
                    layout: &pipe.layout,
                    entries: &entries,
                })
            },
        );
        let (groups_x, groups_y) = sun_trace_groups(inputs.size.0.max(1), inputs.size.1.max(1));
        let mut pass = encoder.segmented_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("sun trace"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipe.pipeline);
        pass.set_bind_group(0, &bg, &[]);
        pass.dispatch_workgroups(groups_x, groups_y, 1);
        drop(pass);
        self.report.traces += 1;
        Ok(())
    }

    /// Builds or refits the same traced scene for ReSTIR GI without writing the sun's visibility
    /// target or changing its variant selection. `None` means GI must keep the probe fallback.
    pub(super) fn restir_scene(
        &mut self,
        d: &TrackedDevice,
        queue: &crate::device::TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        inputs: &SunTraceInputs<'_>,
    ) -> Option<(&wgpu::Tlas, u64)> {
        if !self.update_scene(d, queue, encoder, inputs) {
            return None;
        }
        self.accel
            .as_ref()
            .and_then(GpuAccel::tlas)
            .map(|tlas| (tlas, self.scene_epoch))
    }
}

impl FrameRenderer {
    /// The sun's variant this frame (a step of `render_into`, ADR-1562). Off in every preset: a
    /// default frame takes the shadow map, and no key the frame computes differs.
    pub(super) fn select_sun_variant(
        &mut self,
        d: &TrackedDevice,
        scene: &super::FrameScene<'_>,
        view_pass: bool,
        samples_early: u32,
    ) -> SunShadowVariant {
        let full_forward =
            self.frame_forward_tier(scene.quality.forward_lite) == crate::forward_lite::ForwardLite::Off;
        let request = scene.quality.sun_shadow_trace;
        // The shared draw builds its trace casters while the sun or ReSTIR GI traces scene geometry.
        let gi_wants = scene.quality.restir_gi
            && d.features().contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY)
            && d.features()
                .contains(wgpu::Features::EXPERIMENTAL_RAY_HIT_VERTEX_RETURN);
        let wants_casters = matches!(request, RtShadowRequest::On | RtShadowRequest::Compute) || gi_wants;
        if !view_pass
            && let Some(draw) = self.shared_draw.as_mut()
            && (wants_casters || self.sun_trace.extra.requested)
        {
            draw.set_trace_casters(wants_casters);
        }
        if !view_pass {
            self.sun_trace.extra.requested = wants_casters;
        }
        // Casters the traced scene cannot hold: a shared draw without a trace proxy would lose its
        // shadows, so such a frame keeps the screen trace on the shadow map. Persistent sets keep
        // a copy of their mesh, so they are always traceable.
        let shared_casters = request != RtShadowRequest::Off
            && self
                .shared_draw
                .as_ref()
                .is_some_and(|draw| draw.trace_casters().is_none());
        // Kept across `select_for_frame`, which releases the trace's state when the knob goes off.
        let skipped_last = !view_pass && std::mem::take(&mut self.sun_trace.maps_skipped_last);
        let told_traced = self.sun_trace.told_traced;
        let variant = self
            .sun_trace
            .select_for_frame(request, !view_pass, || FrameTraceSupport {
                device_ray_query: d.features().contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY),
                single_sample_depth: samples_early == 1 && scene.depth_prepass,
                sun_shadows: scene.directional_shadows,
                full_forward,
                main_view: !view_pass,
                shared_casters,
                accel_failed: false,
            });
        self.sun_trace.frame_variant = (variant, !view_pass);
        self.sun_trace.told_traced = told_traced;
        // Cascades skipped last frame hold stale depth: this frame's cache redraws every cascade.
        if skipped_last {
            self.csm_valid = false;
        }
        if !view_pass {
            if variant.traces_rays() || gi_wants {
                let _span = tracing::info_span!("sun_trace_extra_casters").entered();
                let sets_on = scene.quality.visibility_buffer && self.visbuffer.has_persistent_sets();
                self.sun_trace.extra.collect(
                    self.shared_draw.as_ref().and_then(|draw| draw.trace_casters()),
                    self.origin_offset,
                    self.visbuffer.trace_sets().filter(|_| sets_on),
                );
            } else {
                self.sun_trace.extra.clear_snapshot();
            }
        }
        variant
    }

    /// The sun cascades this frame draws (bit `i` = cascade `i`, ADR-2310): all of them unless the
    /// main view traces scene geometry and nothing but the outdoor composite reads them (then its
    /// cascade alone). Tells a shared draw that can take the trace alone whether to.
    pub(super) fn sun_map_keep_mask(
        &mut self,
        scene: &super::FrameScene<'_>,
        csm: &crate::ShadowUniforms,
        draws: &[Draw],
    ) -> u8 {
        let (variant, main_view) = self.sun_trace.frame_variant;
        if !(variant.traces_rays() && main_view) {
            if main_view
                && std::mem::take(&mut self.sun_trace.told_traced)
                && let Some(draw) = self.shared_draw.as_mut()
            {
                draw.set_engine_sun_traced(false);
            }
            return u8::MAX;
        }
        let _span = tracing::info_span!("sun_map_readers").entered();
        let outdoor = scene.quality.physical_atmosphere
            || scene.quality.volumetric_clouds
            || scene.quality.volumetric_fog
            || !self.outdoor.params().is_earth();
        let views = self.csm.as_ref().map_or(1, |(_, _, views)| views.len());
        let readers = crate::hwrt::SunMapReaders {
            blended_receivers: draws.iter().any(|draw| {
                draw.visible
                    && (draw.alpha_mode > ALPHA_MASK
                        || draw.transmissive
                        || draw.xray
                        || draw.hair
                        || draw.over != [0, 0])
            }),
            shared_draw: self
                .shared_draw
                .as_ref()
                .is_some_and(|draw| !draw.takes_traced_sun()),
            missing_casters: draws.iter().any(|draw| {
                draw.shadow.casts_sun()
                    && (!draw.rigid || draw.joint_count > 0 || draw.morphed || draw.graph_vertex)
            }),
            fog_cascade: outdoor.then(|| super::fog_cascade_index(csm, views) as u8),
        };
        let keep = crate::hwrt::sun_map_keep_mask(variant, self.sun_trace.applied, readers);
        let traced = keep != u8::MAX;
        if (traced || self.sun_trace.told_traced)
            && let Some(draw) = self.shared_draw.as_mut()
        {
            draw.set_engine_sun_traced(traced);
        }
        self.sun_trace.told_traced = traced && self.shared_draw.is_some();
        if traced {
            self.sun_trace.maps_skipped_last = true;
            self.sun_trace.report.sun_maps_skipped += 1;
        }
        keep
    }

    /// What the last main-view frame did about the sun's shadows: the variant and why, and the
    /// traced scene's counters (`render.hwrt`).
    #[must_use]
    pub fn sun_trace_report(&self) -> SunTraceReport {
        self.sun_trace.report()
    }

    /// Whether the last main-view frame's variant wrote the screen visibility texture.
    #[must_use]
    pub fn sun_trace_applied(&self) -> bool {
        self.sun_trace.applied
    }

    /// `Self::warm_pipeline_key` for the hitch test: how many pipelines the warm asks for under
    /// `key`, 0 on an error.
    #[cfg(feature = "test-hooks")]
    #[must_use]
    pub fn warm_pipeline_key_for_tests(&mut self, device: &RenderDevice, key: &str) -> usize {
        self.warm_pipeline_key(device, key).unwrap_or(0)
    }

    /// Game-frame warms: the trace pipelines this profile can reach. Nothing unless it asks.
    pub(crate) fn prewarm_sun_trace(&mut self, device: &RenderDevice) -> Result<usize, RenderError> {
        let request = self.sun_trace.report.selection.request;
        if request == RtShadowRequest::Off {
            return Ok(0);
        }
        let d = device.tracked();
        let has_ray_query = d.features().contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY);
        let mut asked = 0;
        // The screen trace is every request's way out: a refused scene takes it for a frame.
        let reachable = [
            (true, SunShadowVariant::ScreenTrace),
            (
                request == RtShadowRequest::On && has_ray_query,
                SunShadowVariant::RayQuery,
            ),
            (
                request == RtShadowRequest::Compute,
                SunShadowVariant::ComputeFallback,
            ),
        ];
        for (_, variant) in reachable.into_iter().filter(|(wanted, _)| *wanted) {
            sun_trace_pipeline(&d, device.shared_pipelines(), variant)?;
            asked += 1;
        }
        Ok(asked)
    }

    /// Game-frame warms: the other forward module of every forward family the warm drew (the
    /// per-object sets with the deferred variants built so far, the GPU-driven colour pipelines,
    /// mask, lightmap and thin-film sets, the visibility-buffer resolve, material graphs), so the
    /// trace giving way to the shadow map (or the profile switching back) compiles nothing on the
    /// frame path. Nothing unless this profile asks for the trace.
    pub(crate) fn prewarm_forward_sun_twins(&mut self, device: &RenderDevice) -> Result<usize, RenderError> {
        if self.sun_trace.report.selection.request == RtShadowRequest::Off {
            return Ok(0);
        }
        let d = &device.device;
        self.ensure_graph_layouts(d)?;
        let Some(pipes) = self.pipes.as_mut() else {
            return Ok(0);
        };
        let keys: Vec<PipelineKey> = pipes.sets.keys().copied().collect();
        let mut asked = 0;
        for key in keys {
            crate::pipeline_warm::warm_checkpoint()?;
            let twin = PipelineKey {
                sun_rt: !key.sun_rt,
                ..key
            };
            if !pipes.sets.contains_key(&twin) {
                let set = super::build_pipeline_set(d, pipes, twin);
                pipes.sets.insert(twin, set);
            }
            if let (Some(drawn), Some(set)) = (pipes.sets.get(&key), pipes.sets.get(&twin)) {
                for (built, deferred) in deferred_variants(drawn).into_iter().zip(deferred_variants(set)) {
                    if built.is_built() {
                        deferred.get();
                    }
                }
            }
            asked += 1;
        }
        let (view_bgl, mat_bgl, object_bgl) = (
            pipes.view_bgl.clone(),
            pipes.mat_bgl.clone(),
            pipes.object_bgl.clone(),
        );
        let groups = [&view_bgl, &object_bgl, &mat_bgl];
        asked += pipes.mask.warm_sun_twins(d, groups);
        asked += pipes.lightmap.warm_sun_twins(d, groups);
        asked += pipes.film.warm_sun_twins(d, groups);
        if crate::gpu_driven::IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
            let keys: Vec<PipelineKey> = pipes.sets.keys().copied().collect();
            let frame_module = self.culler.forward_sun_rt();
            let frame_studio = self.culler.forward_studio_off();
            for key in keys {
                crate::pipeline_warm::warm_checkpoint()?;
                self.culler.set_forward_sun_rt(key.sun_rt);
                // The key's studio module too (ADR-2308).
                self.culler.set_forward_studio_off(key.studio_off);
                let built = self.culler.color_pipelines(
                    d,
                    key.format,
                    key.encode,
                    key.samples,
                    true,
                    &view_bgl,
                    &mat_bgl,
                    &[(key.double_sided, false), (key.double_sided, true)],
                );
                // The next frame sets the module too, but a failed warm leaves no surprise.
                self.culler.set_forward_sun_rt(frame_module);
                self.culler.set_forward_studio_off(frame_studio);
                built?;
                asked += 1;
            }
        }
        crate::pipeline_warm::warm_checkpoint()?;
        asked += self.visbuffer.warm_resolve_twins(d, &view_bgl, &mat_bgl)?;
        if let Some(layouts) = self.graph_layouts.clone() {
            asked += self.graph_cache.prewarm_sun_twins(d, &layouts);
        }
        Ok(asked)
    }
}

/// The deferred variants of a forward pipeline set.
fn deferred_variants(
    set: &PipelineSet,
) -> [&crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>; 9] {
    [
        &set.blend,
        &set.premul,
        &set.add,
        &set.xray,
        &set.transmit_mul,
        &set.transmit_add,
        &set.transmit_add_depth,
        &set.transmit_front,
        &set.transmit_refract,
    ]
}

#[cfg(test)]
mod tests {
    use super::accel_mesh_is_new;

    /// A structural sun-trace rebuild dedups instance meshes in one pass. Many instances of a
    /// few meshes must not compare every instance against every mesh already kept.
    #[test]
    fn accel_mesh_dedup_compares_once_per_instance() {
        const INSTANCES: u64 = 400;
        const MESHES: u64 = 40;
        let mut seen = std::collections::HashSet::new();
        let mut compares = 0u64;
        for i in 0..INSTANCES {
            let _ = accel_mesh_is_new(&mut seen, i % MESHES, &mut compares);
        }
        assert_eq!(seen.len() as u64, MESHES, "one entry per mesh");
        assert!(
            compares <= INSTANCES + MESHES,
            "dedup took {compares} comparisons for {INSTANCES} instances of {MESHES} meshes"
        );
    }
}
