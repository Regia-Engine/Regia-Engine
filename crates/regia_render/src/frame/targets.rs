// SPDX-License-Identifier: MPL-2.0
//! Frame targets: the per-size target pair, depth matching, the refraction copy, the glass front
//! layer, HDR scratch, and the small buffer and texture helpers (moved out of `frame.rs`
//! unchanged, ADR-1552).

use crate::parallel_finish::pass_cuts::SegmentedEncoder as _;

use super::*;

/// The depth and display targets of one attachment size. The linear HDR colour target is a
/// transient of the view's pool (`FrameRenderer::transients`, ADR-2018), keyed by size, format
/// and usage.
pub(super) struct TargetPair {
    /// Single-sample scene depth (`Depth32Float`). [`FrameRenderer::read_scene_depth`] copies it.
    pub(super) depth_texture: wgpu::Texture,
    pub(super) depth: wgpu::TextureView,
    pub(super) ldr_unorm: Option<OffscreenTarget>,
    pub(super) ldr_srgb: Option<OffscreenTarget>,
    /// MSAA colour targets keyed by (samples, format).
    pub(super) msaa_color: HashMap<(u32, wgpu::TextureFormat), wgpu::TextureView>,
    /// MSAA depth targets keyed by samples.
    pub(super) msaa_depth: HashMap<u32, wgpu::TextureView>,
    /// Frame this size was last drawn or handed out ([`release_idle_targets`]).
    pub(super) last_used: u64,
}

impl TargetPair {
    /// Returns the depth memory now. Only for a pair that left the map: nothing records into
    /// it again, and the bind caches drop their groups under the new epoch. (The HDR target is
    /// the pool's: it goes with the view's pool.)
    pub(super) fn destroy(self) {
        self.depth_texture.destroy();
    }
}

/// Usage of the frame's linear HDR target (COPY_SRC: translucent decals composite over a copy
/// of it, and the refraction copy is taken from it).
pub(super) const HDR_USAGE: wgpu::TextureUsages = wgpu::TextureUsages::RENDER_ATTACHMENT
    .union(wgpu::TextureUsages::TEXTURE_BINDING)
    .union(wgpu::TextureUsages::COPY_SRC);

/// Usage of the depth match's copy (`output depth`): a depth attachment the translucent passes
/// sample, and a copy source like every single-sample frame depth (`depth_target`).
pub(super) const OUTPUT_DEPTH_USAGE: wgpu::TextureUsages = wgpu::TextureUsages::RENDER_ATTACHMENT
    .union(wgpu::TextureUsages::TEXTURE_BINDING)
    .union(wgpu::TextureUsages::COPY_SRC);

/// Releases the per-size targets no frame has used for [`TARGET_IDLE_FRAMES`] (a resize, a
/// dynamic-resolution step), except `keep`. Returns `true` when it released any: the caller
/// renews the bind epoch so no cached group holds them. Before v0.1.6 every size a view was
/// ever drawn at stayed allocated.
///
/// The pairs are dropped, not `destroy()`ed: a released size is not tied to an extent change,
/// so a view still held elsewhere (the screen cache, a capture, the depth match) may be used
/// once more; wgpu frees the memory when the last reference goes.
pub(super) fn release_idle_targets(
    targets: &mut HashMap<(u32, u32), TargetPair>,
    now: u64,
    keep: &[(u32, u32)],
) -> bool {
    let before = targets.len();
    targets
        .retain(|key, pair| keep.contains(key) || pair.last_used.saturating_add(TARGET_IDLE_FRAMES) >= now);
    targets.len() != before
}

/// Nearest copy of the scene depth at the swapchain size.
///
/// The translucent colour target is the output. A scaled frame's depth is
/// smaller, and a render pass rejects mismatched attachment sizes. The copy itself (`output
/// depth`) is a frame transient of the view's pool (ADR-4945): this keeps the pipeline and the
/// copy's groups.
pub(super) struct DepthMatch {
    pub(super) pipeline: wgpu::RenderPipeline,
    pub(super) layout: wgpu::BindGroupLayout,
    pub(super) size: (u32, u32),
    /// The source is an MSAA depth; the copy keeps the nearest sample.
    pub(super) multisampled: bool,
    /// The copy's group by scene-depth view: built once per view, not every frame (ADR-2303).
    pub(super) binds: crate::bind_cache::BindCache<wgpu::TextureView>,
}

impl DepthMatch {
    pub(super) fn new(
        device: &wgpu::Device,
        shared: &crate::device_pipelines::DevicePipelines,
        width: u32,
        height: u32,
        multisampled: bool,
    ) -> Result<Self, RenderError> {
        let pipe = depth_match_pipe(device, shared, multisampled)?;
        Ok(Self {
            pipeline: pipe.pipeline,
            layout: pipe.layout,
            size: (width, height),
            multisampled,
            binds: crate::bind_cache::BindCache::default(),
        })
    }
}

/// The depth match's pipeline and its group layout (no size: a new output size makes a new
/// target, not a new pipeline).
#[derive(Clone)]
pub(super) struct DepthMatchPipe {
    pub(super) pipeline: wgpu::RenderPipeline,
    pub(super) layout: wgpu::BindGroupLayout,
}

/// The depth-match pipeline for a single-sample or an MSAA source, once per device (ADR-220).
/// Before v0.1.14 every [`DepthMatch`] compiled its own: each output size change (a resize, a
/// fullscreen toggle) and each renderer's first scaled or MSAA frame with translucent layers
/// compiled it on the frame thread.
pub(super) fn depth_match_pipe(
    device: &wgpu::Device,
    shared: &crate::device_pipelines::DevicePipelines,
    multisampled: bool,
) -> Result<DepthMatchPipe, RenderError> {
    let key = shared_pipes_key(if multisampled {
        "depth match msaa"
    } else {
        "depth match"
    });
    shared
        .get_or_create(key, || {
            let (source, name) = if multisampled {
                (
                    include_str!("../../shaders/depth_resolve.wgsl"),
                    "depth_resolve.wgsl",
                )
            } else {
                (include_str!("../../shaders/depth_match.wgsl"), "depth_match.wgsl")
            };
            let module = shader_module(device, "depth match", source, name, HashMap::new())
                .map_err(|e| e.to_string())?;
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("depth match"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled,
                    },
                    count: None,
                }],
            });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("depth match"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
            crate::frame_stats::note_pipeline_create(Some("depth match"));
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("depth match"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    targets: &[],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth32Float,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::Always),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: crate::pipeline_cache::cache_for(&device).as_deref(),
            });
            Ok(DepthMatchPipe { pipeline, layout })
        })
        .map_err(RenderError::Shader)
}

impl TargetPair {
    /// The LDR target of the requested flavor.
    pub(super) fn ldr_for(&self, srgb: bool) -> Option<&OffscreenTarget> {
        if srgb {
            self.ldr_srgb.as_ref()
        } else {
            self.ldr_unorm.as_ref()
        }
    }
}

pub(super) fn binding(index: u32, resource: wgpu::BindingResource<'_>) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding: index,
        resource,
    }
}

pub(super) fn texture_binding(index: u32, view: &wgpu::TextureView) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding: index,
        resource: wgpu::BindingResource::TextureView(view),
    }
}

pub(super) fn make_ldr(
    d: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> OffscreenTarget {
    let texture = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("frame ldr"),
        size: extent3d(width, height),
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    OffscreenTarget {
        texture,
        view,
        width,
        height,
    }
}

pub(super) fn extent3d(width: u32, height: u32) -> wgpu::Extent3d {
    wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    }
}

pub(super) fn make_buffer(
    d: &wgpu::Device,
    label: &str,
    size: u64,
    usage: wgpu::BufferUsages,
) -> wgpu::Buffer {
    crate::frame_stats::note_buffer(Some(label), size);
    d.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: usage | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

pub(super) fn depth_target(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    label: &str,
) -> (wgpu::Texture, wgpu::TextureView) {
    depth_target_samples(device, width, height, 1, label)
}

/// A depth attachment, optionally multisampled.
/// A colour pass that loads and stores `view` over read-only `depth` (translucent layers).
pub(super) fn begin_load_pass<'e>(
    encoder: &'e mut wgpu::CommandEncoder,
    label: &'static str,
    view: &wgpu::TextureView,
    depth: &wgpu::TextureView,
) -> wgpu::RenderPass<'e> {
    encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Load,
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: depth,
            depth_ops: None,
            stencil_ops: None,
        }),
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    })
}

pub(super) fn depth_target_samples(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    samples: u32,
    label: &str,
) -> (wgpu::Texture, wgpu::TextureView) {
    let mut usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
    // [`read_scene_depth`] copies the single-sample scene depth. A multisampled
    // attachment cannot be a copy source.
    if samples <= 1 {
        usage |= wgpu::TextureUsages::COPY_SRC;
    }
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: extent3d(width.max(1), height.max(1)),
        mip_level_count: 1,
        sample_count: samples.max(1),
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth32Float,
        usage,
        view_formats: &[],
    });
    if samples <= 1 {
        crate::resource_inspector::track(label, &texture);
    }
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

impl FrameRenderer {
    /// Depth attachment of the LDR translucent layers, at the full output. Mixed HDR/LDR layers
    /// need two attachment extents in split screen: the HDR pass used the player's image, this
    /// pass uses the whole target (moved out of `render_into`, ADR-1562's ceiling).
    pub(super) fn ldr_layer_depth(
        &mut self,
        render_device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        layer_depth: &wgpu::TextureView,
        target: &FrameTarget<'_>,
    ) -> Result<wgpu::TextureView, RenderError> {
        let scene = (layer_depth.texture().width(), layer_depth.texture().height());
        let output = (target.width.max(1), target.height.max(1));
        self.depth_for_output(render_device, encoder, layer_depth, false, scene, output)
    }

    /// Depth attachment for the translucent pass.
    ///
    /// Same view when the scene already matches the output. Otherwise a nearest
    /// copy at `output`, so the colour and depth attachments share an extent.
    pub(super) fn depth_for_output(
        &mut self,
        render_device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        scene_depth: &wgpu::TextureView,
        multisampled: bool,
        scene: (u32, u32),
        output: (u32, u32),
    ) -> Result<wgpu::TextureView, RenderError> {
        if scene == output && !multisampled {
            return Ok(scene_depth.clone());
        }
        let device = &render_device.tracked();
        let rebuild = self
            .depth_match
            .as_ref()
            .is_none_or(|matched| matched.size != output || matched.multisampled != multisampled);
        if rebuild {
            self.depth_match = Some(DepthMatch::new(
                device,
                render_device.shared_pipelines(),
                output.0,
                output.1,
                multisampled,
            )?);
        }
        // The copy: a frame transient of the view's pool (`depth_target`'s usage), the same texture
        // every frame at one output size (ADR-4945).
        let target = self.transients.acquire(
            device,
            &crate::TextureDesc::new(
                output.0.max(1),
                output.1.max(1),
                wgpu::TextureFormat::Depth32Float,
            )
            .with_usage(OUTPUT_DEPTH_USAGE),
            "output depth",
        );
        // invariant: the branch above stores a match for `output`
        let matched = self.depth_match.as_mut().expect("depth match built above");
        let layout = &matched.layout;
        let bind = matched.binds.get_or_insert_with(scene_depth.clone(), || {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("depth match"),
                layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(scene_depth),
                }],
            })
        });
        {
            let mut pass = encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("depth match"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &target.view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&matched.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.draw(0..3, 0..1);
        }
        Ok(target.view)
    }

    /// Clears the decal receiver to terrain, stamps `stamps` (opaque meshes) and draws
    /// the shared heightfield into it, on `encoder`, before the pass that draws decals.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record_decal_receiver(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        layer_depth: &wgpu::TextureView,
        stamps: &[crate::decal_receiver::Stamp],
        view_proj: [[f32; 4]; 4],
        viewport: [f32; 4],
        double_sided: bool,
        samples: u32,
    ) -> Result<(), RenderError> {
        if let Some(skin) = self.skin_buf.clone() {
            self.decal_receiver.record(
                d,
                q,
                encoder,
                Some(layer_depth),
                view_proj,
                viewport,
                double_sided,
                &skin,
                stamps,
            )?;
        }
        // Heightfield overwrites the +Y clear where it is the closest surface.
        // A closer mesh already stamped its layer; the depth test keeps that.
        // MSAA frames skip shared draws (the colour pass does too).
        if samples == 1
            && let Some(draw) = self.shared_draw.as_ref()
            && let Some(receiver) = self.decal_receiver.view_cloned()
        {
            let mut pass = encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("terrain decal receiver"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &receiver,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: layer_depth,
                    depth_ops: None,
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let vp_w = viewport[2].max(1.0);
            let vp_h = viewport[3].max(1.0);
            pass.set_viewport(viewport[0], viewport[1], vp_w, vp_h, 0.0, 1.0);
            draw.draw(&mut pass, crate::SharedPass::Receiver);
        }
        Ok(())
    }

    /// Takes a refraction copy and the glass front layer for every HDR target the view's pool
    /// holds, so the first frame that refracts creates none (the game-frame warm with glass in
    /// the level, ADR-2018). Returns how many textures it made.
    pub(crate) fn prewarm_refraction_copies(&mut self, d: &crate::TrackedDevice) -> usize {
        let created = self.transients.stats().created;
        let hdr_targets: Vec<crate::TextureDesc> = self
            .transients
            .entries()
            .into_iter()
            .map(|(desc, _)| desc)
            .filter(|desc| desc.usage == HDR_USAGE && desc.layers == 1 && desc.sample_count == 1)
            .collect();
        for desc in hdr_targets {
            let _ = self.refract_copy(d, desc.width, desc.height, desc.format);
            let _ = self.front_layer(d, desc.width, desc.height);
        }
        (self.transients.stats().created - created) as usize
    }

    /// The opaque-frame copy refracting glass samples: a transient of the view's pool
    /// (ADR-2018), so one size and format gets the same texture every frame.
    pub(super) fn refract_copy(
        &mut self,
        d: &crate::TrackedDevice,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> crate::TransientTexture {
        let desc = crate::TextureDesc::new(width, height, format)
            .with_usage(wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
        self.transients.acquire(d, &desc, "refraction copy")
    }

    /// The glass front layer's depth and transmittance targets at `width` × `height`:
    /// transients of the view's pool (ADR-2018), cleared by the front-layer pass every frame
    /// that draws it.
    pub(super) fn front_layer(
        &mut self,
        d: &crate::TrackedDevice,
        width: u32,
        height: u32,
    ) -> (wgpu::TextureView, wgpu::TextureView) {
        let usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT;
        let depth = self.transients.acquire(
            d,
            &crate::TextureDesc::new(width, height, wgpu::TextureFormat::Depth32Float).with_usage(usage),
            "glass front layer depth",
        );
        let transmittance = self.transients.acquire(
            d,
            &crate::TextureDesc::new(width, height, FRONT_LAYER_FORMAT).with_usage(usage),
            "glass front layer transmittance",
        );
        (depth.view, transmittance.view)
    }

    pub(super) fn target_pair(
        &mut self,
        device: &wgpu::Device,
        width: u32,
        height: u32,
        srgb: bool,
    ) -> &mut TargetPair {
        let d = device;
        let now = self.frame_index;
        let entry = self.targets.entry((width, height)).or_insert_with(|| {
            let (depth_texture, depth) = depth_target(d, width, height, "frame depth");
            TargetPair {
                depth_texture,
                depth,
                ldr_unorm: None,
                ldr_srgb: None,
                msaa_color: HashMap::new(),
                msaa_depth: HashMap::new(),
                last_used: 0,
            }
        });
        entry.last_used = entry.last_used.max(now);
        let want_srgb = srgb;
        if want_srgb && entry.ldr_srgb.is_none() {
            entry.ldr_srgb = Some(make_ldr(d, width, height, wgpu::TextureFormat::Rgba8UnormSrgb));
        }
        if !want_srgb && entry.ldr_unorm.is_none() {
            entry.ldr_unorm = Some(make_ldr(d, width, height, wgpu::TextureFormat::Rgba8Unorm));
        }
        entry
    }

    pub(super) fn ensure_frame_buffers(
        &mut self,
        d: &wgpu::Device,
        max_per_cluster: u32,
    ) -> Result<(), RenderError> {
        // Per camera: what the view uploads from its eye and frustum. A texture view swaps in its
        // own set ([`view_state::ViewState`]), so it never rewrites the main view's.
        if self.frame_buf.is_none() {
            self.frame_buf = Some(make_buffer(
                d,
                "frame uniform",
                size_of::<LabFrame>() as u64,
                wgpu::BufferUsages::UNIFORM,
            ));
            self.lights_buf = Some(make_buffer(
                d,
                "lights storage",
                (size_of::<GpuLight>() * crate::MAX_UPLOADED_LIGHTS as usize) as u64,
                wgpu::BufferUsages::STORAGE,
            ));
            self.csm_buf = Some(make_buffer(
                d,
                "csm uniforms",
                size_of::<crate::ShadowUniforms>() as u64,
                wgpu::BufferUsages::UNIFORM,
            ));
            self.tiles_buf = Some(make_buffer(
                d,
                "shadow tiles",
                (size_of::<ShadowTileGpu>() * MAX_SHADOW_TILES) as u64,
                wgpu::BufferUsages::STORAGE,
            ));
            let cells =
                (crate::shared_draw::GROUND_BOUNCE_MAX * crate::shared_draw::GROUND_BOUNCE_MAX) as u64;
            self.ground_bounce_buf = Some(make_buffer(
                d,
                "ground bounce",
                // Per level: a header, the radiance cells and the horizon cells; one horizon
                // header.
                16 * (crate::shared_draw::GROUND_BOUNCE_LEVELS as u64 * (2 * cells + 1) + 1),
                wgpu::BufferUsages::STORAGE,
            ));
            self.mesh_occluder_buf = Some(make_buffer(
                d,
                "mesh occluders",
                size_of::<crate::MeshOccluderBlock>() as u64,
                wgpu::BufferUsages::UNIFORM,
            ));
        }
        // Shared by every view: the same contents (sun, GI field), or written by the frame's own
        // cluster pass before it is read.
        if self.dir_buf.is_none() {
            self.dir_buf = Some(make_buffer(
                d,
                "directional uniform",
                size_of::<DirectionalLightUniform>() as u64,
                wgpu::BufferUsages::UNIFORM,
            ));
            self.counts_buf = Some(make_buffer(
                d,
                "cluster counts",
                u64::from(CLUSTER_COUNT) * 4,
                wgpu::BufferUsages::STORAGE,
            ));
            self.gi_buf = Some(make_buffer(
                d,
                "gi probes",
                (size_of::<crate::gi::GpuProbe>() * crate::gi::MAX_PROBES) as u64,
                wgpu::BufferUsages::STORAGE,
            ));
            self.gi_grid_buf = Some(make_buffer(
                d,
                "gi grid",
                size_of::<crate::gi::GpuGiGrid>().max(16) as u64,
                wgpu::BufferUsages::UNIFORM,
            ));
        }
        // The cluster light lists, sized for this frame's lights-per-cluster cap instead of the
        // 256 maximum (ADR-3351: 3.4 MiB at 256, 0.2 MiB at Mobile's 16). A frame with a higher
        // cap (a profile change) replaces the buffer with one at least twice as large; the cached
        // view groups that bind the old one are dropped (`cluster_indices_gen`). The cluster
        // build, the forward pass and ReSTIR all index it by the frame's cap, not the maximum.
        let capacity = crate::cluster_index_capacity(self.cluster_indices_cap, max_per_cluster);
        if self.indices_buf.is_none() || capacity != self.cluster_indices_cap {
            let _span = tracing::debug_span!("frame_cluster_buffers_resize").entered();
            self.indices_buf = Some(make_buffer(
                d,
                "cluster indices",
                crate::cluster_index_bytes(capacity),
                wgpu::BufferUsages::STORAGE,
            ));
            self.cluster_indices_cap = capacity;
            self.cluster_indices_gen += 1;
            // Retire renderer-owned cached references to the old allocation, including the sampler's
            // inactive parity and dormant texture views (ADR-8065).
            self.cluster_bg.clear();
            self.view_bgs.clear();
            self.many_light.invalidate_cluster_bindings();
            for view in self.texture_views.values_mut() {
                view.invalidate_cluster_bindings(self.cluster_indices_gen);
            }
        }
        Ok(())
    }

    /// Output size, internal (render) size and upscaler of the last view [`Self::render_into`]
    /// drew; `None` before the first. The performance overlay's resolution line reads this, so
    /// it shows what was rendered, offscreen captures included.
    #[must_use]
    pub fn last_extent(&self) -> Option<FrameExtent> {
        self.last_extent
    }
}

/// The renderer fields the frame's color and depth targets read and write.
pub(super) struct FrameTargetFields<'a> {
    pub(super) targets: &'a mut HashMap<(u32, u32), TargetPair>,
    pub(super) bind_epoch: &'a mut u64,
    /// The view's transient pool: the HDR target comes from it.
    pub(super) transients: &'a mut crate::TransientPool,
}

/// What the frame's color and depth targets are sized and formatted from.
pub(super) struct FrameTargetInputs<'a> {
    /// The frame's output target.
    pub(super) target: &'a FrameTarget<'a>,
    /// The frame renders linear HDR into the internal target.
    pub(super) linear_hdr: bool,
    /// The color format the passes render into.
    pub(super) color_format: wgpu::TextureFormat,
    /// Internal (pre-upscale) render width.
    pub(super) internal_w: u32,
    /// Internal (pre-upscale) render height.
    pub(super) internal_h: u32,
    /// Output width.
    pub(super) width: u32,
    /// Output height.
    pub(super) height: u32,
    /// MSAA sample count.
    pub(super) samples: u32,
    /// The frame clock.
    pub(super) now: u64,
}

/// The frame's color and depth attachments.
pub(super) struct FrameTargets {
    /// Attachment width.
    pub(super) attach_w: u32,
    /// Attachment height.
    pub(super) attach_h: u32,
    /// The single-sample scene depth.
    pub(super) depth_view: wgpu::TextureView,
    /// The linear HDR color texture, `None` on a display-referred frame.
    pub(super) hdr_texture: Option<wgpu::Texture>,
    /// The linear HDR color view, `None` on a display-referred frame.
    pub(super) hdr_view: Option<wgpu::TextureView>,
    /// The multisampled color attachment, on an MSAA frame.
    pub(super) msaa_color: Option<wgpu::TextureView>,
    /// The multisampled depth attachment, on an MSAA frame.
    pub(super) msaa_depth: Option<wgpu::TextureView>,
}

/// Finds (or creates) the frame's color and depth attachments for its size, format and sample
/// count, releasing idle sizes first. Moved out of `render_into` unchanged (ADR-1552). The frame
/// still borrows its pipeline sets here, so this takes the renderer fields it touches.
/// The scene color format of a frame: the profile's HDR target on a linear HDR frame (packed
/// `Rg11b10Ufloat` where the device renders it, else `Rgba16Float`), the display `target`
/// format otherwise. Moved out of `render_into` unchanged (ADR-7860).
pub(super) fn scene_color_format(
    device: &RenderDevice,
    scene: &FrameScene<'_>,
    linear_hdr: bool,
    target: wgpu::TextureFormat,
) -> wgpu::TextureFormat {
    if linear_hdr {
        match scene.quality.hdr_target {
            crate::profile::HdrTarget::Rg11B10 if device.capabilities.rg11b10_renderable => {
                wgpu::TextureFormat::Rg11b10Ufloat
            }
            _ => wgpu::TextureFormat::Rgba16Float,
        }
    } else {
        target
    }
}

pub(super) fn frame_targets(
    fields: FrameTargetFields<'_>,
    d: &crate::TrackedDevice,
    inputs: &FrameTargetInputs<'_>,
) -> FrameTargets {
    let FrameTargetFields {
        targets,
        bind_epoch,
        transients,
    } = fields;
    let FrameTargetInputs {
        target,
        linear_hdr,
        color_format,
        internal_w,
        internal_h,
        width,
        height,
        samples,
        now,
    } = *inputs;
    // Attachment sizes must match: with the post stack the colour target is
    // the internal HDR (view size); without it the frame draws straight
    // into the output view (target size).
    let (attach_w, attach_h) = if linear_hdr {
        (internal_w, internal_h)
    } else {
        (target.width.max(1), target.height.max(1))
    };
    if release_idle_targets(targets, now, &[(attach_w, attach_h), (width, height)]) {
        *bind_epoch = crate::bind_cache::new_epoch();
    }
    let pair = targets.entry((attach_w, attach_h)).or_insert_with(|| {
        let (depth_texture, depth) = depth_target(d, attach_w, attach_h, "frame depth");
        TargetPair {
            depth_texture,
            depth,
            ldr_unorm: None,
            ldr_srgb: None,
            msaa_color: HashMap::new(),
            msaa_depth: HashMap::new(),
            last_used: 0,
        }
    });
    pair.last_used = now;
    let depth_view = pair.depth.clone();
    // The linear HDR colour target: the pool hands the same texture back every frame at one
    // size and format (ADR-2018), so what it held at the end of the last frame is there, as
    // when the pair owned it.
    let hdr = linear_hdr.then(|| {
        let desc = crate::TextureDesc::new(attach_w, attach_h, color_format).with_usage(HDR_USAGE);
        transients.acquire(d, &desc, "frame hdr")
    });
    let hdr_texture = hdr.as_ref().map(|hdr| hdr.texture.clone());
    let hdr_view = hdr.map(|hdr| hdr.view);
    // MSAA attachments (profile-selected): the multisampled colour resolves
    // into the single-sample target (HDR, or the output view without post).
    let (msaa_color, msaa_depth) = if samples > 1 {
        let resolve_format = color_format;
        let color = pair
            .msaa_color
            .entry((samples, resolve_format))
            .or_insert_with(|| {
                let texture = d.create_texture(&wgpu::TextureDescriptor {
                    label: Some("frame msaa color"),
                    size: extent3d(attach_w, attach_h),
                    mip_level_count: 1,
                    sample_count: samples,
                    dimension: wgpu::TextureDimension::D2,
                    format: resolve_format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                });
                texture.create_view(&wgpu::TextureViewDescriptor::default())
            })
            .clone();
        let depth = pair
            .msaa_depth
            .entry(samples)
            .or_insert_with(|| depth_target_samples(d, attach_w, attach_h, samples, "frame msaa depth").1)
            .clone();
        (Some(color), Some(depth))
    } else {
        (None, None)
    };
    FrameTargets {
        attach_w,
        attach_h,
        depth_view,
        hdr_texture,
        hdr_view,
        msaa_color,
        msaa_depth,
    }
}
