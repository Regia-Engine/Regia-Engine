// SPDX-License-Identifier: MPL-2.0
//! The compute software rasterizer for sub-pixel clusters (`docs/specs/98-visibility-buffer.md`
//! § 8, ADR-1887): pipelines, the per-pixel word buffers and the record of one round. The cull
//! routes clusters here (`vis_cull.wgsl` `takes_software`); the rasterizer and its two-pass
//! depth-then-ID scheme are `vis_software.wgsl`, the merge into the ID target and the scene depth
//! is `vis_software_merge.wgsl`.

use super::{VIS_FORMAT, buffer_entry, make_buffer, module, storage, whole};
use crate::RenderError;
use crate::device::{TrackedDevice, TrackedQueue};
use crate::frame_work::counted::*;

/// First word of the software clusters' indirect dispatch in `state` (`vis_cull.wgsl`).
pub(super) const SOFT_ARGS_WORD: u64 = 26;
/// Software clusters listed so far this frame, in `state` (`vis_cull.wgsl`).
pub(super) const SOFT_COUNT_WORD: u64 = 29;
/// Byte offset of the software dispatch in the `dispatch_args` copy.
pub(super) const ARGS_OFFSET: u64 = 16;
/// Largest cluster diameter the cull may route here, pixels: `vis_software.wgsl` keeps its edge
/// products inside an `i32` up to this size.
pub(super) const MAX_SOFTWARE_PX: u32 = 64;

/// Clusters the software range of the cluster buffer holds. IDs carry 24 bits of slot, and a
/// software slot is `capacity + k`; `vis_cull.wgsl` computes the same number.
pub(super) fn software_capacity(capacity: u32) -> u32 {
    capacity.min(0x00ff_ffff - capacity.min(0x00ff_ffff))
}

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct SoftwareParams {
    view_proj: [[f32; 4]; 4],
    info: [u32; 4],
    mode: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct MergeParams {
    info: [u32; 4],
}

struct Pipes {
    raster_bgl: wgpu::BindGroupLayout,
    raster: wgpu::ComputePipeline,
    merge_bgl: wgpu::BindGroupLayout,
    merge: wgpu::RenderPipeline,
}

/// The per-pixel words of one target size.
struct Words {
    width: u32,
    height: u32,
    depth: wgpu::Buffer,
    ids: wgpu::Buffer,
}

/// A frame's bind groups, keyed by the path's generation and the target size.
struct Binds {
    key: (u64, u32, u32),
    /// Depth pass, ID pass.
    raster: [wgpu::BindGroup; 2],
    merge: wgpu::BindGroup,
}

/// What a round's bind groups read from the path.
pub(super) struct SoftwareBindings<'a> {
    /// Bumped by the path whenever a bound buffer is replaced.
    pub generation: u64,
    pub instances: &'a wgpu::Buffer,
    pub meshlets: &'a wgpu::Buffer,
    pub geo: &'a wgpu::Buffer,
    pub clusters: &'a wgpu::Buffer,
    pub state: &'a wgpu::Buffer,
    /// The sway records (ADR-3916).
    pub sway: &'a wgpu::Buffer,
}

/// What one frame draws with.
pub(super) struct SoftwareView {
    pub view_proj: [[f32; 4]; 4],
    pub width: u32,
    pub height: u32,
    /// Cluster capacity per hardware list.
    pub capacity: u32,
    /// Back faces are culled (the camera is not double-sided).
    pub cull_back: bool,
}

/// One round's targets and indirect inputs.
pub(super) struct SoftwareRound<'a> {
    pub state: &'a wgpu::Buffer,
    pub args: &'a wgpu::Buffer,
    pub ids: &'a wgpu::TextureView,
    pub depth: &'a wgpu::TextureView,
}

/// The rasterizer's GPU objects, made the first time the switch is on.
#[derive(Default)]
pub(super) struct SoftwareRaster {
    pipes: Option<Pipes>,
    words: Option<Words>,
    /// Depth pass, ID pass.
    params: Option<[wgpu::Buffer; 2]>,
    merge_params: Option<wgpu::Buffer>,
    binds: Option<Binds>,
}

impl SoftwareRaster {
    /// Takes `warmed`'s pipelines, where this holds none (`VisBufferPass::adopt_pipelines`).
    pub(super) fn adopt_pipelines(&mut self, warmed: &mut Self) {
        if self.pipes.is_none() {
            self.pipes = warmed.pipes.take();
        }
    }

    pub(super) fn ensure_pipes(&mut self, d: &TrackedDevice) -> Result<(), RenderError> {
        if self.pipes.is_some() {
            return Ok(());
        }
        let compute = wgpu::ShaderStages::COMPUTE;
        let raster_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vis software"),
            entries: &[
                buffer_entry(0, compute, wgpu::BufferBindingType::Uniform),
                buffer_entry(1, compute, storage(true)),
                buffer_entry(2, compute, storage(true)),
                buffer_entry(3, compute, storage(true)),
                buffer_entry(4, compute, storage(true)),
                buffer_entry(5, compute, storage(true)),
                buffer_entry(6, compute, storage(false)),
                buffer_entry(7, compute, storage(false)),
                buffer_entry(8, compute, wgpu::BufferBindingType::Uniform),
            ],
        });
        let raster_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vis software"),
            bind_group_layouts: &[Some(&raster_bgl)],
            immediate_size: 0,
        });
        let raster_module = module(d, "vis_software.wgsl", include_str!("vis_software.wgsl"))?;
        let raster = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("vis software"),
            layout: Some(&raster_layout),
            module: &raster_module,
            entry_point: Some("rasterize_clusters"),
            compilation_options: Default::default(),
            cache: crate::pipeline_cache::cache_for(&d).as_deref(),
        });
        let fragment = wgpu::ShaderStages::FRAGMENT;
        let merge_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vis software merge"),
            entries: &[
                buffer_entry(0, fragment, wgpu::BufferBindingType::Uniform),
                buffer_entry(1, fragment, storage(true)),
                buffer_entry(2, fragment, storage(true)),
            ],
        });
        let merge_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vis software merge"),
            bind_group_layouts: &[Some(&merge_bgl)],
            immediate_size: 0,
        });
        let merge_module = module(
            d,
            "vis_software_merge.wgsl",
            include_str!("vis_software_merge.wgsl"),
        )?;
        let merge = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vis software merge"),
            layout: Some(&merge_layout),
            vertex: wgpu::VertexState {
                module: &merge_module,
                entry_point: Some("vs_fullscreen"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &merge_module,
                entry_point: Some("fs_merge"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: VIS_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            // The hardware raster's depth state (`VisBufferPass::raster_pipeline`).
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&d).as_deref(),
        });
        self.pipes = Some(Pipes {
            raster_bgl,
            raster,
            merge_bgl,
            merge,
        });
        Ok(())
    }

    /// Makes everything a frame needs and uploads its parameters. `bindings` name the path's
    /// buffers (already sized for the software list).
    ///
    /// # Errors
    ///
    /// A shader failed to build.
    pub(super) fn prepare(
        &mut self,
        d: &TrackedDevice,
        queue: &TrackedQueue,
        bindings: &SoftwareBindings<'_>,
        view: &SoftwareView,
    ) -> Result<(), RenderError> {
        self.ensure_pipes(d)?;
        let (width, height) = (view.width.max(1), view.height.max(1));
        if self
            .words
            .as_ref()
            .is_none_or(|w| w.width != width || w.height != height)
        {
            let bytes = u64::from(width) * u64::from(height) * 4;
            let usage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
            self.words = Some(Words {
                width,
                height,
                depth: make_buffer(d, "vis software depth", bytes, usage),
                ids: make_buffer(d, "vis software ids", bytes, usage),
            });
            self.binds = None;
        }
        if self.params.is_none() {
            let uniform = wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST;
            let size = std::mem::size_of::<SoftwareParams>() as u64;
            self.params = Some([
                make_buffer(d, "vis software depth pass", size, uniform),
                make_buffer(d, "vis software id pass", size, uniform),
            ]);
            self.merge_params = Some(make_buffer(
                d,
                "vis software merge",
                std::mem::size_of::<MergeParams>() as u64,
                uniform,
            ));
        }
        let (Some(params), Some(merge_params)) = (&self.params, &self.merge_params) else {
            return Ok(());
        };
        for (pass, buffer) in params.iter().enumerate() {
            let block = SoftwareParams {
                view_proj: view.view_proj,
                info: [width, height, view.capacity * 2, view.capacity],
                mode: [pass as u32, u32::from(view.cull_back), 0, 0],
            };
            queue.write_buffer(buffer, 0, bytemuck::bytes_of(&block));
        }
        queue.write_buffer(
            merge_params,
            0,
            bytemuck::bytes_of(&MergeParams {
                info: [width, height, 0, 0],
            }),
        );
        let key = (bindings.generation, width, height);
        if self.binds.as_ref().is_none_or(|b| b.key != key) {
            let (Some(pipes), Some(words)) = (self.pipes.as_ref(), self.words.as_ref()) else {
                return Ok(());
            };
            let raster = |params: &wgpu::Buffer| {
                d.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("vis software"),
                    layout: &pipes.raster_bgl,
                    entries: &[
                        whole(0, params),
                        whole(1, bindings.instances),
                        whole(2, bindings.meshlets),
                        whole(3, bindings.geo),
                        whole(4, bindings.clusters),
                        whole(5, bindings.state),
                        whole(6, &words.depth),
                        whole(7, &words.ids),
                        whole(8, bindings.sway),
                    ],
                })
            };
            let merge = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("vis software merge"),
                layout: &pipes.merge_bgl,
                entries: &[
                    whole(0, merge_params),
                    whole(1, &words.depth),
                    whole(2, &words.ids),
                ],
            });
            self.binds = Some(Binds {
                key,
                raster: [raster(&params[0]), raster(&params[1])],
                merge,
            });
        }
        Ok(())
    }

    /// Clears the pixel words: call once per frame, before the first round.
    pub(super) fn record_clear(&self, encoder: &mut wgpu::CommandEncoder) {
        if let Some(words) = &self.words {
            encoder.clear_buffer(&words.depth, 0, None);
            encoder.clear_buffer(&words.ids, 0, None);
        }
    }

    /// Rasterizes the software clusters the last cull phase listed and merges every software
    /// pixel so far into the ID target and the depth.
    pub(super) fn record_round(&self, encoder: &mut wgpu::CommandEncoder, round: &SoftwareRound<'_>) {
        let _span = tracing::info_span!("visbuffer_software_raster").entered();
        let _crumb = crate::breadcrumbs::frame_pass("visbuffer_software_raster");
        let (Some(pipes), Some(binds)) = (&self.pipes, &self.binds) else {
            return;
        };
        encoder.copy_buffer_to_buffer(round.state, SOFT_ARGS_WORD * 4, round.args, ARGS_OFFSET, 12);
        {
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("vis software raster"),
                timestamp_writes: None,
            });
            pass.counted_set_pipeline(&pipes.raster);
            for group in &binds.raster {
                pass.counted_set_bind_group(0, group, &[]);
                pass.counted_dispatch_indirect(round.args, ARGS_OFFSET);
            }
        }
        let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("vis software merge"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: round.ids,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: round.depth,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            ..Default::default()
        });
        pass.counted_set_pipeline(&pipes.merge);
        pass.counted_set_bind_group(0, &binds.merge, &[]);
        pass.counted_draw(0..3, 0..1);
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![
        crate::contract_row!(
            "vis_software.wgsl",
            "SoftwareParams",
            SoftwareParams {
                view_proj,
                info,
                mode
            }
        ),
        crate::contract_row!("vis_software_merge.wgsl", "MergeParams", MergeParams { info }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_match_the_shader_layout() {
        assert_eq!(std::mem::size_of::<SoftwareParams>(), 96);
        assert_eq!(std::mem::size_of::<MergeParams>(), 16);
    }

    #[test]
    fn software_slots_fit_the_visibility_id() {
        // Slot `capacity + k` stays inside 24 bits, and the all-ones ID stays free.
        for capacity in [1024, 1 << 20, 1 << 23, (1 << 24) - 1, 1 << 24] {
            let room = software_capacity(capacity);
            assert!(
                room == 0 || capacity + room <= 0x00ff_ffff,
                "capacity {capacity}, room {room}"
            );
        }
        assert_eq!(software_capacity(1 << 20), 1 << 20);
        assert_eq!(software_capacity(1 << 24), 0);
    }

    #[test]
    fn the_shader_constants_match() {
        let source = include_str!("vis_cull.wgsl");
        assert!(source.contains(&format!("const SOFT_ARGS: u32 = {SOFT_ARGS_WORD}u;")));
        assert!(source.contains(&format!("const SOFT_COUNT: u32 = {SOFT_COUNT_WORD}u;")));
    }
}
