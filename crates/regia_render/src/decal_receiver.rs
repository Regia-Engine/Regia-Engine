// SPDX-License-Identifier: MPL-2.0
//! Receiver target for projected decals.
//!
//! The forward renderer has no G-buffer. This pass writes one `Rgba8Unorm` texture the
//! decal shader samples: R is the receiver layer (`layer_index / 255`), GBA is the world
//! normal encoded as `n * 0.5 + 0.5`. The attachment is cleared to terrain (layer 0, world
//! +Y). A mesh with no explicit layer stamps static when it is rigid (layer 1) and a
//! character when it is skinned (layer 2). An explicit layer overrides that, so a rigid
//! mesh can be masked too. Native terrain then draws into the same target, so a slope
//! replaces the +Y clear with the heightfield normal.
//!
//! Skinning matches `shaders/mesh.wgsl` (`Mat3x4` rows, translation in `w`). The pass does
//! not edit that shader.
//!
//! Stamps draw as one instanced draw per mesh. Where stamps overlap, the one later in the
//! frame's draw order keeps the pixel, exactly as when they were drawn one by one: each
//! instance writes its draw-order key as fragment depth into a `Depth32Float` order target
//! (`GreaterEqual`), the same key as the velocity pass (`taau::order_depth`). The scene-depth
//! test (`LessEqual`, no write) moves into the fragment shader, which compares the fragment
//! depth with the scene depth texel it covers.

use crate::RenderError;
use crate::frame_work::counted::*;
use crate::lab_frame::LabVertex;

/// One opaque mesh to stamp into the receiver.
pub(crate) struct Stamp {
    /// Mesh key: stamps of one mesh draw as one instanced draw.
    pub mesh: u64,
    pub vb: wgpu::Buffer,
    pub ib: wgpu::Buffer,
    pub index_count: u32,
    /// First index of the mesh in `ib` (a mesh-arena range).
    pub first_index: u32,
    /// First vertex of the mesh in `vb`.
    pub base_vertex: i32,
    pub model: [[f32; 4]; 4],
    pub joint_base: u32,
    /// `1` static mesh, `2` character, or any other explicit layer index.
    pub layer: u32,
}

/// Layer index written into the receiver.
///
/// `explicit == 0` keeps the default: a rigid mesh is static (1) and a skinned mesh is a
/// character (2). Any other value is that layer, clamped to 31, so a rigid mesh can be
/// masked without a joint palette.
pub(crate) fn stamp_layer(rigid: bool, explicit: u32) -> u32 {
    if explicit == 0 {
        if rigid { 1 } else { 2 }
    } else {
        explicit.min(31)
    }
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct ReceiverItem {
    model: [[f32; 4]; 4],
    /// Joint base, layer, draw-order key (`f32` bits), unused.
    skin_layer: [u32; 4],
}

/// One instanced draw: a mesh and its run of items.
struct StampBatch {
    stamp: usize,
    first: u32,
    instances: u32,
}

const ITEM_SIZE: u64 = 80;

/// Receiver colour plus an optional scene-colour copy.
#[derive(Default)]
pub(crate) struct DecalReceiver {
    texture: Option<wgpu::Texture>,
    view: Option<wgpu::TextureView>,
    /// Draw-order keys of the stamp pass (`Depth32Float`, same size as the receiver).
    order: Option<wgpu::TextureView>,
    size: (u32, u32),
    scene: Option<wgpu::Texture>,
    scene_view: Option<wgpu::TextureView>,
    scene_size: (u32, u32),
    scene_format: Option<wgpu::TextureFormat>,
    bgl: Option<wgpu::BindGroupLayout>,
    /// Group 1 of the depth variants: the scene depth the fragment shader tests against.
    bgl_depth: Option<wgpu::BindGroupLayout>,
    pipe_depth: Option<wgpu::RenderPipeline>,
    pipe_depth_double: Option<wgpu::RenderPipeline>,
    pipe_nodepth: Option<wgpu::RenderPipeline>,
    pipe_nodepth_double: Option<wgpu::RenderPipeline>,
    uniform_buf: Option<wgpu::Buffer>,
    items_buf: Option<wgpu::Buffer>,
    items_capacity: usize,
    /// Stamps the items buffer is sized for at least ([`Self::reserve`]).
    reserved: usize,
    /// The receiver group by (uniform, items, skin) buffer: rebuilt only when one is replaced.
    binds: crate::bind_cache::BindCache<(wgpu::Buffer, wgpu::Buffer, wgpu::Buffer)>,
    /// The scene-depth group by depth view.
    depth_binds: crate::bind_cache::BindCache<wgpu::TextureView>,
    /// Per-frame scratch, kept across frames: `(mesh, draw order)` sorted, items, batches.
    keyed: Vec<(u64, u32)>,
    items: Vec<ReceiverItem>,
    batches: Vec<StampBatch>,
    copy_warned: bool,
    /// View-projection last written into `uniform_buf`.
    uploaded_vp: [[f32; 4]; 4],
    /// `uploaded_vp` matches the buffer. False until the first write.
    uploaded_vp_set: bool,
    /// Item rows last written into `items_buf`. Cleared when that buffer is replaced.
    uploaded_items: Vec<ReceiverItem>,
    /// View-projection writes this receiver has queued (tests).
    #[cfg(test)]
    vp_uploads: u64,
    /// Item-buffer writes this receiver has queued (tests).
    #[cfg(test)]
    item_uploads: u64,
}

impl DecalReceiver {
    /// Sizes the stamp buffer for at least `stamps` receivers (no growth mid-game).
    pub(crate) fn reserve(&mut self, stamps: usize) {
        self.reserved = self.reserved.max(stamps);
    }

    /// Builds every pass variant, the stamp buffers and the group for `skin` now, so the
    /// first decal of a level costs no pipeline or bind-group creation on the frame path.
    pub(crate) fn prewarm(&mut self, device: &wgpu::Device, skin: &wgpu::Buffer) -> Result<(), RenderError> {
        self.prewarm_pipelines(device)?;
        self.ensure_items(device, self.reserved);
        let (Some(uniform), Some(items), Some(layout)) =
            (self.uniform_buf.clone(), self.items_buf.clone(), self.bgl.clone())
        else {
            return Ok(());
        };
        let key = (uniform.clone(), items.clone(), skin.clone());
        let _ = self.binds.get_or_insert_with(key, || {
            crate::device::TrackedDevice::observe(device).create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("decal receiver bg"),
                layout: &layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: uniform.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: items.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: skin.as_entire_binding(),
                    },
                ],
            })
        });
        Ok(())
    }

    /// Builds every pass variant (a warm stops between them once its queue shuts down,
    /// [`crate::warm_cancelled`]).
    pub(crate) fn prewarm_pipelines(&mut self, device: &wgpu::Device) -> Result<(), RenderError> {
        for (depth, double_sided) in [(true, false), (true, true), (false, false), (false, true)] {
            crate::pipeline_warm::warm_checkpoint()?;
            self.ensure_pipeline(device, depth, double_sided)?;
        }
        Ok(())
    }

    /// Creates or resizes the receiver to `width`×`height`.
    pub(crate) fn ensure(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        if self.size == (width, height) && self.view.is_some() {
            return;
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("decal receiver"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            // COPY_SRC: read back by the stamp-order test (and any debug capture).
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        self.view = Some(texture.create_view(&wgpu::TextureViewDescriptor::default()));
        self.texture = Some(texture);
        let order = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("decal receiver order"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        self.order = Some(order.create_view(&wgpu::TextureViewDescriptor::default()));
        self.size = (width, height);
    }

    /// Creates or resizes the scene-colour copy. Format must match the source.
    pub(crate) fn ensure_scene(
        &mut self,
        device: &wgpu::Device,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) {
        let width = width.max(1);
        let height = height.max(1);
        if self.scene_size == (width, height)
            && self.scene_format == Some(format)
            && self.scene_view.is_some()
        {
            return;
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("decal scene copy"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        self.scene_view = Some(texture.create_view(&wgpu::TextureViewDescriptor::default()));
        self.scene = Some(texture);
        self.scene_size = (width, height);
        self.scene_format = Some(format);
    }

    pub(crate) fn view_cloned(&self) -> Option<wgpu::TextureView> {
        self.view.clone()
    }

    pub(crate) fn scene_view_cloned(&self) -> Option<wgpu::TextureView> {
        self.scene_view.clone()
    }

    pub(crate) fn scene_texture_cloned(&self) -> Option<wgpu::Texture> {
        self.scene.clone()
    }

    pub(crate) fn take_copy_warning(&mut self) -> bool {
        if self.copy_warned {
            return false;
        }
        self.copy_warned = true;
        true
    }

    /// Clears the receiver to terrain and stamps `stamps`. `depth` is the output-sized
    /// scene depth (`depth_for_output`); pass `None` only when no depth is available.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record(
        &mut self,
        device: &wgpu::Device,
        queue: &crate::device::TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        depth: Option<&wgpu::TextureView>,
        view_proj: [[f32; 4]; 4],
        viewport: [f32; 4],
        double_sided: bool,
        skin: &wgpu::Buffer,
        stamps: &[Stamp],
    ) -> Result<(), RenderError> {
        let _span = tracing::info_span!("decal_receiver").entered();
        let _crumb = crate::breadcrumbs::frame_pass("decal_receiver");
        let Some(color) = self.view.clone() else {
            return Ok(());
        };
        let Some(order) = self.order.clone() else {
            return Ok(());
        };
        let with_depth = depth.is_some();
        self.ensure_pipeline(device, with_depth, double_sided)?;
        let pipeline = self.pipeline(with_depth, double_sided).cloned();
        // Group by mesh, keeping the draw order inside each mesh; every item carries its
        // draw-order key, so the batch order does not change which stamp keeps a pixel.
        self.keyed.clear();
        self.keyed.extend(
            stamps
                .iter()
                .enumerate()
                .filter(|(_, stamp)| stamp.index_count > 0)
                .map(|(index, stamp)| (stamp.mesh, u32::try_from(index).unwrap_or(u32::MAX))),
        );
        self.keyed.sort_unstable();
        self.items.clear();
        self.batches.clear();
        let mut last_mesh = None;
        for &(mesh, order_index) in &self.keyed {
            let stamp_index = order_index as usize;
            let Some(stamp) = stamps.get(stamp_index) else {
                continue;
            };
            let slot = u32::try_from(self.items.len()).unwrap_or(u32::MAX);
            self.items.push(ReceiverItem {
                model: stamp.model,
                skin_layer: [
                    stamp.joint_base,
                    stamp.layer,
                    crate::taau::order_depth(order_index).to_bits(),
                    0,
                ],
            });
            if last_mesh == Some(mesh)
                && let Some(batch) = self.batches.last_mut()
            {
                batch.instances += 1;
                continue;
            }
            last_mesh = Some(mesh);
            self.batches.push(StampBatch {
                stamp: stamp_index,
                first: slot,
                instances: 1,
            });
        }
        if !self.items.is_empty() {
            let items_replaced = self.ensure_items(device, self.items.len());
            let write_vp = !self.uploaded_vp_set || self.uploaded_vp != view_proj;
            let write_items = items_replaced || self.uploaded_items != self.items;
            // Clone the handles so the write does not borrow `self` across the mirror update.
            if write_vp && let Some(buf) = self.uniform_buf.clone() {
                #[cfg(test)]
                {
                    self.vp_uploads += 1;
                }
                queue.write_buffer(&buf, 0, bytemuck::cast_slice(&view_proj));
                self.uploaded_vp = view_proj;
                self.uploaded_vp_set = true;
            }
            if write_items && let Some(buf) = self.items_buf.clone() {
                #[cfg(test)]
                {
                    self.item_uploads += 1;
                }
                queue.write_buffer(&buf, 0, bytemuck::cast_slice(&self.items));
                self.uploaded_items.clone_from(&self.items);
            }
        }
        let bg = if self.items.is_empty() {
            None
        } else {
            let uniform = self
                .uniform_buf
                .as_ref()
                .ok_or_else(|| RenderError::Shader("decal receiver uniform missing".into()))?;
            let items = self
                .items_buf
                .as_ref()
                .ok_or_else(|| RenderError::Shader("decal receiver items missing".into()))?;
            let layout = self
                .bgl
                .as_ref()
                .ok_or_else(|| RenderError::Shader("decal receiver layout missing".into()))?;
            let key = (uniform.clone(), items.clone(), skin.clone());
            Some(self.binds.get_or_insert_with(key, || {
                crate::device::TrackedDevice::observe(device).create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("decal receiver bg"),
                    layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: uniform.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: items.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: skin.as_entire_binding(),
                        },
                    ],
                })
            }))
        };
        let depth_bg = match (depth, bg.is_some()) {
            (Some(view), true) => Some(
                self.scene_depth_group(device, view)
                    .ok_or_else(|| RenderError::Shader("decal receiver depth layout missing".into()))?,
            ),
            _ => None,
        };
        {
            let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("decal receiver"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Terrain, world +Y: R = 0, GBA = (0, 1, 0) * 0.5 + 0.5.
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.0,
                            g: 0.5,
                            b: 1.0,
                            a: 0.5,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                // Draw-order keys: the latest stamp at a pixel wins, whatever the batch order.
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &order,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(0.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let vp_w = viewport[2].max(1.0);
            let vp_h = viewport[3].max(1.0);
            pass.set_viewport(viewport[0], viewport[1], vp_w, vp_h, 0.0, 1.0);
            if let (Some(pipeline), Some(bg)) = (pipeline.as_ref(), bg.as_ref()) {
                pass.counted_set_pipeline(pipeline);
                pass.counted_set_bind_group(0, bg, &[]);
                if let Some(depth_bg) = depth_bg.as_ref() {
                    pass.counted_set_bind_group(1, depth_bg, &[]);
                }
                for batch in &self.batches {
                    let Some(stamp) = stamps.get(batch.stamp) else {
                        continue;
                    };
                    pass.set_vertex_buffer(0, stamp.vb.slice(..));
                    pass.set_index_buffer(stamp.ib.slice(..), wgpu::IndexFormat::Uint32);
                    pass.counted_draw_indexed(
                        stamp.first_index..stamp.first_index + stamp.index_count,
                        stamp.base_vertex,
                        batch.first..batch.first + batch.instances,
                    );
                }
            }
        }
        Ok(())
    }

    /// The scene-depth group for `depth` (cached by view). Called every frame the decal
    /// layers run, decals or not, so it exists before the first decal of a level is spawned;
    /// `None` before [`Self::prewarm`] or the first stamp built the layout.
    pub(crate) fn scene_depth_group(
        &mut self,
        device: &wgpu::Device,
        depth: &wgpu::TextureView,
    ) -> Option<wgpu::BindGroup> {
        let layout = self.bgl_depth.as_ref()?;
        Some(self.depth_binds.get_or_insert_with(depth.clone(), || {
            crate::device::TrackedDevice::observe(device).create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("decal receiver scene depth"),
                layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(depth),
                }],
            })
        }))
    }

    /// View-projection writes and item-buffer writes this receiver has queued.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn upload_counts(&self) -> (u64, u64) {
        (self.item_uploads, self.vp_uploads)
    }

    fn pipeline(&self, depth: bool, double_sided: bool) -> Option<&wgpu::RenderPipeline> {
        match (depth, double_sided) {
            (true, false) => self.pipe_depth.as_ref(),
            (true, true) => self.pipe_depth_double.as_ref(),
            (false, false) => self.pipe_nodepth.as_ref(),
            (false, true) => self.pipe_nodepth_double.as_ref(),
        }
    }

    /// Grows the stamp buffers. Returns whether `items_buf` was replaced (its previous bytes
    /// are gone, so the caller must write the items even when the CPU rows did not change).
    fn ensure_items(&mut self, device: &wgpu::Device, count: usize) -> bool {
        let device = &crate::device::TrackedDevice::observe(device);
        let count = count.max(1).max(self.reserved);
        if self.uniform_buf.is_none() {
            self.uniform_buf = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("decal receiver vp"),
                size: 64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.uploaded_vp_set = false;
        }
        if self.items_capacity >= count && self.items_buf.is_some() {
            return false;
        }
        let mut capacity = self.items_capacity.max(8);
        while capacity < count {
            capacity *= 2;
        }
        self.items_buf = Some(device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("decal receiver items"),
            size: ITEM_SIZE * capacity as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        self.items_capacity = capacity;
        self.uploaded_items.clear();
        true
    }

    fn ensure_pipeline(
        &mut self,
        device: &wgpu::Device,
        depth: bool,
        double_sided: bool,
    ) -> Result<(), RenderError> {
        if self.pipeline(depth, double_sided).is_some() {
            return Ok(());
        }
        let device = &crate::device::TrackedDevice::observe(device);
        if self.bgl.is_none() {
            self.bgl = Some(device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("decal receiver bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    storage_entry(1),
                    storage_entry(2),
                ],
            }));
        }
        if self.bgl_depth.is_none() {
            self.bgl_depth = Some(device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("decal receiver scene depth bgl"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                }],
            }));
        }
        let layout = if depth {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("decal receiver depth layout"),
                bind_group_layouts: &[self.bgl.as_ref(), self.bgl_depth.as_ref()],
                immediate_size: 0,
            })
        } else {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("decal receiver layout"),
                bind_group_layouts: &[self.bgl.as_ref()],
                immediate_size: 0,
            })
        };
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("decal receiver"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let error_scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(if depth {
                "decal receiver depth"
            } else {
                "decal receiver no depth"
            }),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[Some(LabVertex::layout())],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some(if depth { "fs_depth" } else { "fs_main" }),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: if double_sided {
                    None
                } else {
                    Some(wgpu::Face::Back)
                },
                ..Default::default()
            },
            // `GreaterEqual` on the draw-order key written as fragment depth: equal keys (one
            // stamp's own overlapping triangles) keep the later triangle, as before.
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&device).as_deref(),
        });
        if let Some(err) = regia_core::tasks::block_on(error_scope.pop()) {
            return Err(RenderError::Shader(err.to_string()));
        }
        match (depth, double_sided) {
            (true, false) => self.pipe_depth = Some(pipeline),
            (true, true) => self.pipe_depth_double = Some(pipeline),
            (false, false) => self.pipe_nodepth = Some(pipeline),
            (false, true) => self.pipe_nodepth_double = Some(pipeline),
        }
        Ok(())
    }
}

const _: () = assert!(std::mem::size_of::<ReceiverItem>() == ITEM_SIZE as usize);

fn storage_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

pub(crate) const SHADER: &str = r#"
struct Mat3x4 {
    row_x: vec4<f32>,
    row_y: vec4<f32>,
    row_z: vec4<f32>,
};

struct Item {
    model: mat4x4<f32>,
    skin_layer: vec4<u32>,
};

struct Frame {
    view_proj: mat4x4<f32>,
};

@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var<storage, read> items: array<Item>;
@group(0) @binding(2) var<storage, read> joints: array<Mat3x4>;
@group(1) @binding(0) var scene_depth: texture_depth_2d;

struct Vin {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) tangent: vec4<f32>,
    @location(4) color: vec4<f32>,
    @location(5) joints: vec4<u32>,
    @location(6) weights: vec4<f32>,
};

struct Vout {
    @builtin(position) clip: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @interpolate(flat) @location(1) layer: u32,
    // The stamp's draw-order key (`skin_layer.z`, f32 bits), written as fragment depth.
    @interpolate(flat) @location(2) order: f32,
};

struct Fout {
    @location(0) receiver: vec4<f32>,
    @builtin(frag_depth) order: f32,
};

fn mul_point(m: Mat3x4, p: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        dot(m.row_x.xyz, p) + m.row_x.w,
        dot(m.row_y.xyz, p) + m.row_y.w,
        dot(m.row_z.xyz, p) + m.row_z.w,
    );
}

fn mul_dir(m: Mat3x4, d: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(dot(m.row_x.xyz, d), dot(m.row_y.xyz, d), dot(m.row_z.xyz, d));
}

@vertex
fn vs_main(in: Vin, @builtin(instance_index) instance: u32) -> Vout {
    let item = items[instance];
    let base = item.skin_layer.x;
    let wsum = dot(in.weights, vec4<f32>(1.0));
    var pos = in.position;
    var nrm = in.normal;
    if (wsum >= 1e-5) {
        let j0 = joints[base + in.joints.x];
        let j1 = joints[base + in.joints.y];
        let j2 = joints[base + in.joints.z];
        let j3 = joints[base + in.joints.w];
        pos = in.weights.x * mul_point(j0, in.position)
            + in.weights.y * mul_point(j1, in.position)
            + in.weights.z * mul_point(j2, in.position)
            + in.weights.w * mul_point(j3, in.position);
        nrm = in.weights.x * mul_dir(j0, in.normal)
            + in.weights.y * mul_dir(j1, in.normal)
            + in.weights.z * mul_dir(j2, in.normal)
            + in.weights.w * mul_dir(j3, in.normal);
    }
    let world = (item.model * vec4<f32>(pos, 1.0)).xyz;
    let world_n = (item.model * vec4<f32>(nrm, 0.0)).xyz;
    // Constructor, not `var out: Vout` (ADR-026 / ADR-036).
    return Vout(
        frame.view_proj * vec4<f32>(world, 1.0),
        world_n,
        item.skin_layer.y,
        bitcast<f32>(item.skin_layer.z),
    );
}

fn encode(in: Vout) -> Fout {
    let n = normalize(in.normal + vec3<f32>(1e-8));
    let enc = n * 0.5 + vec3<f32>(0.5);
    let layer = f32(min(in.layer, 255u)) / 255.0;
    return Fout(vec4<f32>(layer, enc), in.order);
}

@fragment
fn fs_main(in: Vout) -> Fout {
    return encode(in);
}

// The scene-depth test the fixed-function `LessEqual` did: `clip.z` is the depth that test
// compared, the texel is the one the fragment covers (the target matches the receiver).
@fragment
fn fs_depth(in: Vout) -> Fout {
    let scene = textureLoad(scene_depth, vec2<i32>(in.clip.xy), 0);
    if (in.clip.z > scene) {
        discard;
    }
    return encode(in);
}
"#;

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "decal_receiver.rs",
        "Item",
        ReceiverItem { model, skin_layer }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{TrackedQueue, test_device};
    use regia_core::glam::camera::rh::proj::directx::perspective;
    use regia_core::glam::camera::rh::view::look_at_mat4;
    use regia_core::math::{Mat4, Quat, Vec3};
    use wgpu::util::DeviceExt as _;

    const W: u32 = 96;
    const H: u32 = 64;

    fn vertex(position: [f32; 3], normal: [f32; 3]) -> LabVertex {
        LabVertex {
            position,
            normal,
            uv: [0.0; 2],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        }
    }

    /// Three meshes: a quad, a triangle, and two coplanar triangles with different normals
    /// (inside one stamp the later triangle keeps the pixel).
    fn meshes(device: &wgpu::Device) -> Vec<(wgpu::Buffer, wgpu::Buffer, u32)> {
        let z = [0.0, 0.0, 1.0];
        let sets: [(Vec<LabVertex>, Vec<u32>); 3] = [
            (
                vec![
                    vertex([-1.0, -1.0, 0.0], z),
                    vertex([1.0, -1.0, 0.0], z),
                    vertex([1.0, 1.0, 0.0], z),
                    vertex([-1.0, 1.0, 0.0], z),
                ],
                vec![0, 1, 2, 0, 2, 3],
            ),
            (
                vec![
                    vertex([-1.2, -0.8, 0.0], [0.6, 0.0, 0.8]),
                    vertex([1.2, -0.8, 0.0], [0.6, 0.0, 0.8]),
                    vertex([0.0, 1.3, 0.0], [0.6, 0.0, 0.8]),
                ],
                vec![0, 1, 2],
            ),
            (
                vec![
                    vertex([-1.0, -1.0, 0.0], [0.0, 0.8, 0.6]),
                    vertex([1.0, -1.0, 0.0], [0.0, 0.8, 0.6]),
                    vertex([0.0, 1.0, 0.0], [0.0, 0.8, 0.6]),
                    vertex([-1.0, -1.0, 0.0], [-0.8, 0.0, 0.6]),
                    vertex([1.0, -1.0, 0.0], [-0.8, 0.0, 0.6]),
                    vertex([0.0, 1.0, 0.0], [-0.8, 0.0, 0.6]),
                ],
                vec![0, 1, 2, 3, 4, 5],
            ),
        ];
        sets.iter()
            .map(|(vertices, indices)| {
                let vb = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("test stamp vb"),
                    contents: bytemuck::cast_slice(vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                });
                let ib = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("test stamp ib"),
                    contents: bytemuck::cast_slice(indices),
                    usage: wgpu::BufferUsages::INDEX,
                });
                (vb, ib, indices.len() as u32)
            })
            .collect()
    }

    /// Dense, interleaved, overlapping stamps: the three meshes alternate in draw order,
    /// some stamps repeat an earlier transform exactly (coplanar, equal depth), some face
    /// away (culled single-sided).
    fn stamps(meshes: &[(wgpu::Buffer, wgpu::Buffer, u32)]) -> Vec<Stamp> {
        let mut seed = 0x2545_f491_u32;
        let mut next = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32
        };
        let mut models: Vec<Mat4> = Vec::new();
        let mut out = Vec::new();
        for i in 0..60u32 {
            let model = if i % 9 == 4 && models.len() > 3 {
                models[models.len() - 3]
            } else {
                let at = Vec3::new(next() * 7.0 - 3.5, next() * 4.0 - 2.0, next() * 4.0 - 2.0);
                let axis = Vec3::new(next() - 0.5, next() - 0.5, next() - 0.5).normalize_or(Vec3::Y);
                let angle = (next() - 0.5) * 4.0;
                Mat4::from_scale_rotation_translation(
                    Vec3::splat(1.6),
                    Quat::from_axis_angle(axis, angle),
                    at,
                )
            };
            models.push(model);
            let mesh = (i % 3) as usize;
            let (vb, ib, index_count) = &meshes[mesh];
            out.push(Stamp {
                mesh: mesh as u64 + 100,
                vb: vb.clone(),
                ib: ib.clone(),
                index_count: *index_count,
                first_index: 0,
                base_vertex: 0,
                model: model.to_cols_array_2d(),
                joint_base: 0,
                layer: 1 + i % 7,
            });
        }
        out
    }

    fn view_proj() -> [[f32; 4]; 4] {
        let proj = perspective(0.9, W as f32 / H as f32, 0.5, 40.0);
        let view = look_at_mat4(Vec3::new(0.0, 0.0, 9.0), Vec3::ZERO, Vec3::Y);
        (proj * view).to_cols_array_2d()
    }

    fn items_buffer(device: &wgpu::Device, stamps: &[Stamp]) -> wgpu::Buffer {
        let items: Vec<ReceiverItem> = stamps
            .iter()
            .map(|stamp| ReceiverItem {
                model: stamp.model,
                skin_layer: [stamp.joint_base, stamp.layer, 0, 0],
            })
            .collect();
        device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("test items"),
            contents: bytemuck::cast_slice(&items),
            usage: wgpu::BufferUsages::STORAGE,
        })
    }

    fn group0(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        items: &wgpu::Buffer,
        skin: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("test vp"),
            contents: bytemuck::cast_slice(&view_proj()),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("test receiver bg"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: items.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: skin.as_entire_binding(),
                },
            ],
        })
    }

    /// A pipeline from the receiver shader: `fragment` `None` = depth only.
    fn test_pipeline(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        fragment: Option<&str>,
        depth: Option<(bool, wgpu::CompareFunction)>,
        double_sided: bool,
    ) -> wgpu::RenderPipeline {
        let source = format!(
            "{SHADER}\n@fragment\nfn fs_ref(in: Vout) -> @location(0) vec4<f32> {{ return encode(in).receiver; }}\n"
        );
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("test receiver"),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("test receiver layout"),
            bind_group_layouts: &[Some(layout)],
            immediate_size: 0,
        });
        let targets = [Some(wgpu::ColorTargetState {
            format: wgpu::TextureFormat::Rgba8Unorm,
            blend: None,
            write_mask: wgpu::ColorWrites::ALL,
        })];
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("test receiver"),
            layout: Some(&pl),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[Some(LabVertex::layout())],
                compilation_options: Default::default(),
            },
            fragment: fragment.map(|entry| wgpu::FragmentState {
                module: &module,
                entry_point: Some(entry),
                targets: &targets,
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                cull_mode: if double_sided {
                    None
                } else {
                    Some(wgpu::Face::Back)
                },
                ..Default::default()
            },
            depth_stencil: depth.map(|(write, compare)| wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(write),
                depth_compare: Some(compare),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&device).as_deref(),
        })
    }

    fn draw_each(pass: &mut wgpu::RenderPass<'_>, stamps: &[Stamp], keep: impl Fn(usize) -> bool) {
        for (index, stamp) in stamps.iter().enumerate() {
            if !keep(index) {
                continue;
            }
            pass.set_vertex_buffer(0, stamp.vb.slice(..));
            pass.set_index_buffer(stamp.ib.slice(..), wgpu::IndexFormat::Uint32);
            let first = index as u32;
            pass.counted_draw_indexed(
                stamp.first_index..stamp.first_index + stamp.index_count,
                stamp.base_vertex,
                first..first + 1,
            );
        }
    }

    fn texture(device: &wgpu::Device, format: wgpu::TextureFormat, label: &str) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: W,
                height: H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    }

    const CLEAR: wgpu::Color = wgpu::Color {
        r: 0.0,
        g: 0.5,
        b: 1.0,
        a: 0.5,
    };

    /// The scene depth: every third stamp drawn over a 0.985 clear (stamps behind it fail,
    /// stamps in front pass, the drawn ones pass at exactly their own depth).
    fn scene_depth(
        device: &crate::RenderDevice,
        layout: &wgpu::BindGroupLayout,
        stamps: &[Stamp],
        skin: &wgpu::Buffer,
    ) -> wgpu::Texture {
        let d = &device.device;
        let depth = texture(d, wgpu::TextureFormat::Depth32Float, "test scene depth");
        let view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let pipeline = test_pipeline(
            d,
            layout,
            None,
            Some((true, wgpu::CompareFunction::LessEqual)),
            true,
        );
        let bg = group0(d, layout, &items_buffer(d, stamps), skin);
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("test scene depth"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(0.985),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.counted_set_pipeline(&pipeline);
            pass.counted_set_bind_group(0, &bg, &[]);
            draw_each(&mut pass, stamps, |index| index % 3 == 0);
        }
        device.queue.submit([encoder.finish()]);
        depth
    }

    /// The pass before instancing: one draw per stamp in draw order against the scene depth
    /// (`LessEqual`, no write).
    fn reference(
        device: &crate::RenderDevice,
        layout: &wgpu::BindGroupLayout,
        stamps: &[Stamp],
        skin: &wgpu::Buffer,
        depth: Option<&wgpu::TextureView>,
        double_sided: bool,
    ) -> Vec<u8> {
        let d = &device.device;
        let color = texture(d, wgpu::TextureFormat::Rgba8Unorm, "test reference receiver");
        let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
        let compare = depth.map(|_| (false, wgpu::CompareFunction::LessEqual));
        let pipeline = test_pipeline(d, layout, Some("fs_ref"), compare, double_sided);
        let bg = group0(d, layout, &items_buffer(d, stamps), skin);
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("test reference receiver"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(CLEAR),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: depth.map(|view| wgpu::RenderPassDepthStencilAttachment {
                    view,
                    depth_ops: None,
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.counted_set_pipeline(&pipeline);
            pass.counted_set_bind_group(0, &bg, &[]);
            draw_each(&mut pass, stamps, |_| true);
        }
        device.queue.submit([encoder.finish()]);
        crate::frame::read_rgba8(d, &device.queue, &color, W, H).expect("reference readback")
    }

    #[test]
    fn instanced_stamps_match_drawing_them_one_by_one() {
        let device = test_device();
        let d = &device.device;
        let skin = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test skin"),
            size: 256,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let meshes = meshes(d);
        let stamps = stamps(&meshes);
        let mut receiver = DecalReceiver::default();
        receiver.prewarm(d, &skin).expect("prewarm");
        receiver.ensure(d, W, H);
        let layout = receiver.bgl.clone().expect("receiver layout");
        let depth = scene_depth(&device, &layout, &stamps, &skin);
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let queue = TrackedQueue::observe(&device.queue);
        for (with_depth, double_sided) in [(true, false), (true, true), (false, false), (false, true)] {
            let depth = with_depth.then_some(&depth_view);
            let expected = reference(&device, &layout, &stamps, &skin, depth, double_sided);
            let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            receiver
                .record(
                    d,
                    &queue,
                    &mut encoder,
                    depth,
                    view_proj(),
                    [0.0, 0.0, W as f32, H as f32],
                    double_sided,
                    &skin,
                    &stamps,
                )
                .expect("record");
            device.queue.submit([encoder.finish()]);
            let texture = receiver.texture.clone().expect("receiver texture");
            let got = crate::frame::read_rgba8(d, &device.queue, &texture, W, H).expect("readback");
            let differ = expected
                .chunks(4)
                .zip(got.chunks(4))
                .filter(|(a, b)| a != b)
                .count();
            assert_eq!(
                differ, 0,
                "depth {with_depth}, double-sided {double_sided}: {differ} pixels differ"
            );

            // Teeth: grouping by mesh without the order key changes this scene.
            let mut grouped: Vec<Stamp> = Vec::new();
            for mesh in 100..103 {
                grouped.extend(stamps.iter().filter(|s| s.mesh == mesh).map(|s| Stamp {
                    mesh: s.mesh,
                    vb: s.vb.clone(),
                    ib: s.ib.clone(),
                    index_count: s.index_count,
                    first_index: s.first_index,
                    base_vertex: s.base_vertex,
                    model: s.model,
                    joint_base: s.joint_base,
                    layer: s.layer,
                }));
            }
            let unordered = reference(&device, &layout, &grouped, &skin, depth, double_sided);
            let moved = expected
                .chunks(4)
                .zip(unordered.chunks(4))
                .filter(|(a, b)| a != b)
                .count();
            assert!(
                moved > 20,
                "the scene must be order-sensitive ({moved} pixels move when grouped)"
            );
            let touched = expected.chunks(4).filter(|px| px[0] != 0).count();
            assert!(
                touched > (W * H / 4) as usize,
                "stamps must cover the target ({touched} pixels)"
            );
        }
    }

    /// The same stamps and view-projection must not be written again. Moving one model writes
    /// the items; changing only the view-projection writes that uniform.
    #[test]
    fn unchanged_stamps_are_not_uploaded_again() {
        let device = test_device();
        let d = &device.device;
        let skin = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test skin"),
            size: 256,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let meshes = meshes(d);
        let mut stamps = stamps(&meshes);
        let mut receiver = DecalReceiver::default();
        receiver.ensure(d, W, H);
        let queue = TrackedQueue::observe(&device.queue);
        let vp = view_proj();
        let viewport = [0.0, 0.0, W as f32, H as f32];
        let submit = |receiver: &mut DecalReceiver, stamps: &[Stamp], vp: [[f32; 4]; 4]| {
            let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            receiver
                .record(d, &queue, &mut encoder, None, vp, viewport, false, &skin, stamps)
                .expect("record");
            device.queue.submit([encoder.finish()]);
        };
        submit(&mut receiver, &stamps, vp);
        assert_eq!(
            receiver.upload_counts(),
            (1, 1),
            "the first record must upload both"
        );
        let item_bytes = stamps.len() * 80;
        submit(&mut receiver, &stamps, vp);
        assert_eq!(
            receiver.upload_counts(),
            (1, 1),
            "an unchanged frame rewrote the buffers ({item_bytes} item bytes and 64 view-proj bytes)"
        );
        stamps[0].model[3][0] += 0.25;
        submit(&mut receiver, &stamps, vp);
        assert_eq!(
            receiver.upload_counts(),
            (2, 1),
            "moving one model must write items and leave the view-projection"
        );
        let mut shifted = vp;
        shifted[3][0] += 0.1;
        submit(&mut receiver, &stamps, shifted);
        assert_eq!(
            receiver.upload_counts(),
            (2, 2),
            "a new view-projection must write that uniform and leave the items"
        );
    }

    #[test]
    fn explicit_layer_overrides_rigid_and_skinned() {
        assert_eq!(stamp_layer(true, 0), 1, "a rigid mesh defaults to static");
        assert_eq!(stamp_layer(false, 0), 2, "a skinned mesh defaults to character");
        assert_eq!(stamp_layer(true, 2), 2, "a rigid prop can be a character");
        assert_eq!(stamp_layer(false, 1), 1, "a skinned mesh can be static");
        assert_eq!(stamp_layer(true, 40), 31, "layers stay in the mask's 32 bits");
    }
}
