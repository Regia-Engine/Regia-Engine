// SPDX-License-Identifier: MPL-2.0
//! Single-pass depth pyramid (SPD) shared by GPU occlusion culling and stochastic SSR.
//!
//! One compute dispatch reduces 64×64 tiles of a source level into the next six levels in
//! workgroup memory (`shaders/sssr_spd.wgsl`). A chain longer than seven levels takes one more
//! dispatch per six levels. The culler's pyramid keeps the farthest depth ([`Reduce::Max`]) and
//! the reflection tracer's keeps the nearest ([`Reduce::Min`]). Level 0 is a copy of the depth
//! buffer in both.
//!
//! The result is texel-for-texel the per-mip `hiz_down.wgsl` chain: wgpu mip sizes round down,
//! and a child past the end of its level is replaced by its in-range sibling, which is that
//! shader's clamp.
//!
//! ## Provenance
//!
//! AMD FidelityFX SDK v1.1.4 SPD (`sdk/include/FidelityFX/gpu/spd/ffx_spd.h`, MIT) and Bevy's
//! `downsample_depth.wesl` (MIT OR Apache-2.0); see `docs/ported-sources.toml`.

use crate::frame_work::counted::*;
use std::borrow::Cow;
use std::collections::HashMap;

use crate::RenderError;
use crate::bind_cache::BindCache;
use crate::shader_lib::compose_source;

/// Storage textures one SPD dispatch binds (level 0 plus six reduced levels).
pub(crate) const SPD_STORAGE_TEXTURES: u32 = 7;
/// Levels one dispatch reduces after its source level.
const LEVELS_PER_PASS: u32 = 6;
/// Source texels per workgroup edge.
const TILE: u32 = 64;

/// Reduction of a 2×2 footprint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reduce {
    /// Nearest standard-Z depth (ray marching never skips a surface).
    Min,
    /// Farthest standard-Z depth (an occluder test never hides a visible object).
    Max,
}

/// SPD pipelines for one reduction.
pub(crate) struct SpdKernel {
    depth_bgl: wgpu::BindGroupLayout,
    depth_pipe: wgpu::ComputePipeline,
    mip_bgl: wgpu::BindGroupLayout,
    mip_pipe: wgpu::ComputePipeline,
}

impl SpdKernel {
    /// `true` when the device binds enough storage textures per compute stage.
    ///
    /// The WebGPU default is 4. A device below [`SPD_STORAGE_TEXTURES`] keeps the per-mip path.
    #[must_use]
    pub(crate) fn supported(device: &wgpu::Device) -> bool {
        device.limits().max_storage_textures_per_shader_stage >= SPD_STORAGE_TEXTURES
    }

    /// Compiles the two entry points (depth source, mip source) for `reduce`.
    ///
    /// # Errors
    ///
    /// The shader fails to compose.
    pub(crate) fn new(device: &wgpu::Device, reduce: Reduce) -> Result<Self, RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        let module = |from_depth: bool, label: &str| -> Result<wgpu::ShaderModule, RenderError> {
            let mut defs = HashMap::new();
            if reduce == Reduce::Min {
                defs.insert(
                    "SPD_MIN".to_string(),
                    naga_oil::compose::ShaderDefValue::Bool(true),
                );
            }
            if from_depth {
                defs.insert(
                    "SPD_FROM_DEPTH".to_string(),
                    naga_oil::compose::ShaderDefValue::Bool(true),
                );
            }
            let naga = compose_source(include_str!("../shaders/sssr_spd.wgsl"), "sssr_spd.wgsl", defs)?;
            Ok(device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(label),
                source: wgpu::ShaderSource::Naga(Cow::Owned(naga)),
            }))
        };
        let depth_mod = module(true, "spd depth")?;
        let mip_mod = module(false, "spd mip")?;
        let mut depth_entries = vec![
            uniform_entry(0),
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            storage_entry(2),
        ];
        let mut mip_entries = vec![
            uniform_entry(0),
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
        ];
        for binding in 3..=8 {
            depth_entries.push(storage_entry(binding));
            mip_entries.push(storage_entry(binding));
        }
        let depth_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("spd depth bgl"),
            entries: &depth_entries,
        });
        let mip_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("spd mip bgl"),
            entries: &mip_entries,
        });
        let pipe = |bgl: &wgpu::BindGroupLayout, module: &wgpu::ShaderModule, label: &str| {
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &[Some(bgl)],
                immediate_size: 0,
            });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                module,
                entry_point: Some("spd"),
                compilation_options: Default::default(),
                cache: crate::pipeline_cache::cache_for(&device).as_deref(),
            })
        };
        let depth_pipe = pipe(&depth_bgl, &depth_mod, "spd depth");
        let mip_pipe = pipe(&mip_bgl, &mip_mod, "spd mip");
        Ok(Self {
            depth_bgl,
            depth_pipe,
            mip_bgl,
            mip_pipe,
        })
    }
}

/// One later dispatch: its bind group and workgroup count.
struct LaterPass {
    bind_group: wgpu::BindGroup,
    groups: (u32, u32),
}

/// Bind groups that build one mip chain with an [`SpdKernel`].
pub(crate) struct SpdChain {
    first_storage: Vec<wgpu::TextureView>,
    first_params: wgpu::Buffer,
    first_groups: (u32, u32),
    later: Vec<LaterPass>,
    /// Keeps the 1×1 stand-ins for levels past the chain alive.
    #[allow(dead_code)]
    dummies: Vec<wgpu::Texture>,
    first_cache: BindCache<wgpu::TextureView>,
}

/// Dispatches needed for a chain of `mips` levels (level 0 is the copy; at least one dispatch).
fn pass_count(mips: u32) -> u32 {
    mips.saturating_sub(1).div_ceil(LEVELS_PER_PASS).max(1)
}

impl SpdChain {
    /// Binds `storage` (one write view per mip, level 0 first) and `sampled` (one read view per
    /// mip) of a `width`×`height` R32Float chain.
    pub(crate) fn new(
        device: &wgpu::Device,
        kernel: &SpdKernel,
        storage: &[wgpu::TextureView],
        sampled: &[wgpu::TextureView],
        width: u32,
        height: u32,
    ) -> Self {
        let device = &crate::device::TrackedDevice::observe(device);
        let mips = storage.len().max(1) as u32;
        let passes = pass_count(mips);
        let mut dummies = Vec::new();
        let mut dummy_views = Vec::new();
        for _ in 0..LEVELS_PER_PASS {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("spd unused level"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R32Float,
                usage: wgpu::TextureUsages::STORAGE_BINDING,
                view_formats: &[],
            });
            dummy_views.push(texture.create_view(&wgpu::TextureViewDescriptor::default()));
            dummies.push(texture);
        }
        // Write view of level `base + j` (j = 1..=6), or the j-th stand-in past the chain.
        let level_view = |base: u32, j: u32| -> wgpu::TextureView {
            let level = base + j;
            if level < mips {
                storage[level as usize].clone()
            } else {
                dummy_views[(j - 1) as usize].clone()
            }
        };
        let groups_for = |level: u32| -> (u32, u32) {
            let w = (width >> level).max(1);
            let h = (height >> level).max(1);
            (w.div_ceil(TILE), h.div_ceil(TILE))
        };
        let params_for = |pass: u32| -> wgpu::Buffer {
            let base = pass * LEVELS_PER_PASS;
            let levels = (mips - 1 - base).min(LEVELS_PER_PASS);
            // Counted like the tracked device's own creates (`create_buffer_init` is not on it).
            crate::frame_stats::note_buffer(Some("spd params"), 16);
            wgpu::util::DeviceExt::create_buffer_init(
                &**device,
                &wgpu::util::BufferInitDescriptor {
                    label: Some("spd params"),
                    contents: bytemuck::cast_slice(&[levels, 0u32, 0, 0]),
                    usage: wgpu::BufferUsages::UNIFORM,
                },
            )
        };
        let mut first_storage = vec![storage[0].clone()];
        for j in 1..=LEVELS_PER_PASS {
            first_storage.push(level_view(0, j));
        }
        let first_params = params_for(0);
        let mut later = Vec::new();
        for pass in 1..passes {
            let base = pass * LEVELS_PER_PASS;
            let params = params_for(pass);
            let mut entries = vec![
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&sampled[base as usize]),
                },
            ];
            let views: Vec<wgpu::TextureView> = (1..=LEVELS_PER_PASS).map(|j| level_view(base, j)).collect();
            for (j, view) in views.iter().enumerate() {
                entries.push(wgpu::BindGroupEntry {
                    binding: 3 + j as u32,
                    resource: wgpu::BindingResource::TextureView(view),
                });
            }
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("spd later"),
                layout: &kernel.mip_bgl,
                entries: &entries,
            });
            later.push(LaterPass {
                bind_group,
                groups: groups_for(base),
            });
        }
        Self {
            first_storage,
            first_params,
            first_groups: groups_for(0),
            later,
            dummies,
            first_cache: BindCache::default(),
        }
    }

    /// Builds the bind group that reads `depth` (the size the chain was built for) now, so the
    /// first [`Self::record`] with that view creates nothing.
    pub(crate) fn prepare(&mut self, device: &wgpu::Device, kernel: &SpdKernel, depth: &wgpu::TextureView) {
        let _ = self.first_group(device, kernel, depth);
    }

    /// The first dispatch's bind group for `depth`, built on first use of the view.
    fn first_group(
        &mut self,
        device: &wgpu::Device,
        kernel: &SpdKernel,
        depth: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        let device = &crate::device::TrackedDevice::observe(device);
        let storage = &self.first_storage;
        let params = &self.first_params;
        self.first_cache.get_or_insert_with(depth.clone(), || {
            let mut entries = vec![
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(depth),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&storage[0]),
                },
            ];
            for (j, view) in storage.iter().enumerate().skip(1) {
                entries.push(wgpu::BindGroupEntry {
                    binding: 2 + j as u32,
                    resource: wgpu::BindingResource::TextureView(view),
                });
            }
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("spd depth"),
                layout: &kernel.depth_bgl,
                entries: &entries,
            })
        })
    }

    /// Records every dispatch of the chain into `pass`, reading `depth` (the size the chain was
    /// built for). wgpu orders the dispatches: a later one reads what an earlier one wrote.
    pub(crate) fn record(
        &mut self,
        device: &wgpu::Device,
        kernel: &SpdKernel,
        pass: &mut wgpu::ComputePass<'_>,
        depth: &wgpu::TextureView,
    ) {
        let first = self.first_group(device, kernel, depth);
        pass.counted_set_pipeline(&kernel.depth_pipe);
        pass.counted_set_bind_group(0, &first, &[]);
        pass.counted_dispatch(self.first_groups.0, self.first_groups.1, 1);
        if !self.later.is_empty() {
            pass.counted_set_pipeline(&kernel.mip_pipe);
            for later in &self.later {
                pass.counted_set_bind_group(0, &later.bind_group, &[]);
                pass.counted_dispatch(later.groups.0, later.groups.1, 1);
            }
        }
    }
}

fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn storage_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format: wgpu::TextureFormat::R32Float,
            view_dimension: wgpu::TextureViewDimension::D2,
        },
        count: None,
    }
}

/// GPU helpers for pyramid tests: a hashed depth buffer and an R32Float mip readback.
#[cfg(test)]
pub(crate) mod test_util {
    use crate::RenderDevice;
    use crate::frame_work::counted::*;

    /// A `width`×`height` Depth32Float filled with a per-pixel hash of `seed` (values in 0..1,
    /// some pixels at exactly 1.0 like a cleared sky).
    pub(crate) fn hashed_depth(
        device: &RenderDevice,
        width: u32,
        height: u32,
        seed: u32,
    ) -> wgpu::TextureView {
        let d = &device.device;
        let texture = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("test depth"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let source = format!(
            "@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {{
                 let x = select(-1.0, 3.0, i == 1u);
                 let y = select(-1.0, 3.0, i == 2u);
                 return vec4<f32>(x, y, 0.5, 1.0);
             }}
             struct Out {{ @builtin(frag_depth) depth: f32 }}
             @fragment fn fs(@builtin(position) p: vec4<f32>) -> Out {{
                 var h = u32(p.x) * 73856093u ^ u32(p.y) * 19349663u ^ {seed}u;
                 h = (h ^ (h >> 16u)) * 0x45d9f3bu;
                 h = (h ^ (h >> 16u)) * 0x45d9f3bu;
                 h = h ^ (h >> 16u);
                 let v = f32(h & 0xffffu) / 65535.0;
                 return Out(select(v, 1.0, (h >> 20u) % 7u == 0u));
             }}"
        );
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("test depth"),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        let pipe = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("test depth"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
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
            cache: crate::pipeline_cache::cache_for(&d).as_deref(),
        });
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("test depth"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &view,
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
            pass.counted_set_pipeline(&pipe);
            pass.counted_draw(0..3, 0..1);
        }
        device.queue.submit([encoder.finish()]);
        view
    }

    /// Reads mip `level` of an R32Float texture as `f32`s, row-major.
    pub(crate) fn read_r32(
        device: &RenderDevice,
        texture: &wgpu::Texture,
        level: u32,
    ) -> (u32, u32, Vec<f32>) {
        let w = (texture.width() >> level).max(1);
        let h = (texture.height() >> level).max(1);
        let padded = (w * 4).div_ceil(256) * 256;
        let staging = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("r32 readback"),
            size: u64::from(padded * h),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: level,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        device.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device
            .tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        rx.recv().expect("map").expect("map ok");
        let mapped = staging.slice(..).get_mapped_range().expect("range");
        let mut out = Vec::with_capacity((w * h) as usize);
        for row in 0..h {
            let start = (row * padded) as usize;
            out.extend_from_slice(bytemuck::cast_slice(&mapped[start..start + (w * 4) as usize]));
        }
        (w, h, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `mips`-level R32Float chain over a hashed depth with `reduce`, returns the texture.
    fn build_chain(device: &crate::RenderDevice, w: u32, h: u32, mips: u32, reduce: Reduce) -> wgpu::Texture {
        let d = &device.device;
        let depth = test_util::hashed_depth(device, w, h, 7);
        let texture = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("chain"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: mips,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R32Float,
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = |level: u32, usage: wgpu::TextureUsages| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                usage: Some(usage),
                base_mip_level: level,
                mip_level_count: Some(1),
                ..Default::default()
            })
        };
        let storage: Vec<_> = (0..mips)
            .map(|l| view(l, wgpu::TextureUsages::STORAGE_BINDING))
            .collect();
        let sampled: Vec<_> = (0..mips)
            .map(|l| view(l, wgpu::TextureUsages::TEXTURE_BINDING))
            .collect();
        let kernel = SpdKernel::new(d, reduce).expect("kernel");
        let mut chain = SpdChain::new(d, &kernel, &storage, &sampled, w, h);
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor::default());
            chain.record(d, &kernel, &mut pass, &depth);
        }
        device.queue.submit([encoder.finish()]);
        texture
    }

    #[test]
    fn min_chain_matches_a_cpu_reduction_with_edge_clamp() {
        let device = crate::test_device();
        if !SpdKernel::supported(&device.device) {
            return;
        }
        for (w, h) in [(333u32, 97u32), (64, 36), (130, 1), (1, 1)] {
            let mips = (w.max(h).ilog2() + 1).min(7);
            let texture = build_chain(&device, w, h, mips, Reduce::Min);
            let (_, _, mut prev) = test_util::read_r32(&device, &texture, 0);
            let (mut pw, mut ph) = (w, h);
            for level in 1..mips {
                let (lw, lh, got) = test_util::read_r32(&device, &texture, level);
                for y in 0..lh {
                    for x in 0..lw {
                        let at = |cx: u32, cy: u32| prev[(cy.min(ph - 1) * pw + cx.min(pw - 1)) as usize];
                        let want = at(2 * x, 2 * y)
                            .min(at(2 * x + 1, 2 * y))
                            .min(at(2 * x, 2 * y + 1))
                            .min(at(2 * x + 1, 2 * y + 1));
                        let have = got[(y * lw + x) as usize];
                        assert_eq!(
                            have.to_bits(),
                            want.to_bits(),
                            "{w}×{h} level {level} texel ({x},{y}): {have} vs {want}"
                        );
                    }
                }
                prev = got;
                pw = lw;
                ph = lh;
            }
        }
    }

    #[test]
    fn pass_count_covers_every_level() {
        let levels = |w: u32, h: u32| w.max(h).ilog2() + 1;
        // 2560×1440 has 12 levels: one dispatch for 1..=6, one for 7..=11.
        assert_eq!(levels(2560, 1440), 12);
        assert_eq!(pass_count(levels(2560, 1440)), 2);
        // A 64×36 lab frame: 7 levels, one dispatch.
        assert_eq!(pass_count(levels(64, 36)), 1);
        // A 1×1 frame still runs the copy.
        assert_eq!(pass_count(levels(1, 1)), 1);
    }
}
