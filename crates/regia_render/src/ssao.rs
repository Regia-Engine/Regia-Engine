// SPDX-License-Identifier: MPL-2.0
//! Screen-space ambient occlusion (`specs/26` profile table).
//!
//! Off leaves indirect lighting untouched (the frame binds a 1×1 white texture).
//! Every other row runs the same view-space obscurance kernel (Alchemy AO, McGuire et
//! al. HPG 2011) followed by a 4×4 depth-aware blur; the rows differ in resolution
//! and sample count. With reflection occlusion on, the GTAO rows also estimate the specular
//! lobe's visibility (G, B). With bent normals on ([`crate::FrameQuality::bent_normals`]) the
//! same samples also give each texel's bent normal, stored as its lean from the depth normal in a
//! second layer of the AO texture (ADR-1375; the CPU mirror is `ssao/bent_normal.rs`). See
//! `shaders/ssao.wgsl` and `docs/provenance.md`.

// CPU mirror of the bent normal and the cone specular occlusion (tests only).
#[cfg(test)]
mod bent_normal;

use crate::frame_work::counted::*;
use std::collections::HashMap;

use crate::RenderError;
use crate::profile::SsaoMode;
use crate::shader_lib::compose_source;

/// World-space AO radius in metres: contact scale for props and vehicles.
pub const SSAO_RADIUS_M: f32 = 1.2;
/// AO strength (1 = the obscurance estimate as computed).
pub const SSAO_INTENSITY: f32 = 1.6;
/// Tangent-plane bias (cosine) that keeps flat ground from self-occluding.
pub const SSAO_BIAS: f32 = 0.08;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SsaoParams {
    screen: [f32; 4],
    mode: [f32; 4],
    proj: [f32; 4],
    noise: [f32; 4],
    probe: [u32; 4],
    to_world: [[f32; 4]; 3],
}

/// The baked GI volume's probe-relative AO split for one dispatch (ADR-1104,
/// `baked_gi/ao_split.rs`): the baked screen lookup's buffer, the first word of its split words
/// and the lookup's size.
#[derive(Clone, Debug)]
pub(crate) struct SsaoProbeSplit {
    /// The baked screen lookup's words (written earlier in the same encoder).
    pub(crate) buffer: wgpu::Buffer,
    /// First word of the per-texel split words.
    pub(crate) first_word: u32,
    /// The lookup's width and height (texels).
    pub(crate) size: (u32, u32),
}

impl SsaoProbeSplit {
    /// `SsaoParams::probe` for this split (all zero without one: the whole kernel).
    fn params(split: Option<&Self>) -> [u32; 4] {
        split.map_or([0; 4], |s| [s.first_word, s.size.0, s.size.1, 0])
    }
}

/// Layers of the AO texture: layer 0 holds the AO and the reflection visibility, layer 1 the bent
/// normal's lean (`ssao.wgsl` `bent_encode`).
pub(crate) const SSAO_LAYERS: u32 = 2;

/// `cs_horizon` (ADR-3202) with bent normals off and on, and their blue-noise variants.
struct HorizonPipelines {
    plain: wgpu::ComputePipeline,
    bent: wgpu::ComputePipeline,
    blue: Option<(wgpu::ComputePipeline, wgpu::ComputePipeline)>,
}

/// Whether `mode` is a GTAO row, the rows the horizon kernel replaces (ADR-3202).
fn horizon_row(mode: SsaoMode) -> bool {
    matches!(mode, SsaoMode::GtaoHalfRes | SsaoMode::GtaoFull)
}

/// Persistent SSAO targets. Rebuilt when the resolution changes.
#[derive(Default)]
pub struct SsaoPass {
    /// The kernel and the blur with bent normals off (`ssao.wgsl` `BENT_NORMALS` false: no lean,
    /// layer 1 not written).
    pipeline: Option<wgpu::ComputePipeline>,
    blur_pipeline: Option<wgpu::ComputePipeline>,
    /// The kernel with bent normals on (`BENT_NORMALS` true), and the blur from a workgroup tile
    /// (`cs_blur_tiled`): the same bytes as the per-tap blur, faster when the lean is blurred too
    /// (ADR-1491).
    pipeline_bent: Option<wgpu::ComputePipeline>,
    blur_tiled_pipeline: Option<wgpu::ComputePipeline>,
    bgl: Option<wgpu::BindGroupLayout>,
    blur_bgl: Option<wgpu::BindGroupLayout>,
    params: Option<wgpu::Buffer>,
    texture: Option<wgpu::Texture>,
    raw_texture: Option<wgpu::Texture>,
    /// Layer 0 alone (AO and reflection visibility): the terrain's engine AO.
    view: Option<wgpu::TextureView>,
    /// Both layers: sampled by the forward shader (binding 17), which reads the bent normal's lean
    /// from layer 1 when it is on.
    layers: Option<wgpu::TextureView>,
    /// Written by the blur kernel (storage usage).
    storage: Option<wgpu::TextureView>,
    /// Unblurred AO: written by the main kernel, read by the blur.
    raw_storage: Option<wgpu::TextureView>,
    raw_sample: Option<wgpu::TextureView>,
    size: (u32, u32),
    /// Generations of the pooled `texture` and `raw_texture` the views were made from.
    generations: (u64, u64),
    /// Last uploaded params. A static camera skips the copy.
    param_bytes: Vec<u8>,
    /// Bound as the probe split when a dispatch has none (`probe.y` = 0 keeps it unread).
    no_split: Option<wgpu::Buffer>,
    /// The view's AO strength scale (`PostSettings::ao_intensity`, ADR-1464); `None` is 1.
    intensity_scale: Option<f32>,
    /// The table of the next dispatches, `None` with the knob off (ADR-2075).
    blue_noise: Option<std::sync::Arc<crate::blue_noise::BlueNoiseGpu>>,
    /// The kernel's blue-noise variants: plain and bent.
    blue_pipelines: Option<(wgpu::ComputePipeline, wgpu::ComputePipeline)>,
    /// The horizon kernel (`ssao.wgsl` `cs_horizon`, ADR-3202) on the GTAO rows instead of the
    /// point kernel: [`crate::FrameQuality::horizon_ao`], set by the frame before each dispatch.
    pub(crate) horizon: bool,
    /// The horizon kernel's pipelines (built on first use): plain, bent, and the blue-noise
    /// variants of both once a blue-noise table is set.
    horizon_pipelines: Option<HorizonPipelines>,
    bg_cache:
        crate::bind_cache::BindCache<(wgpu::Buffer, wgpu::TextureView, wgpu::TextureView, wgpu::Buffer)>,
    blur_cache: crate::bind_cache::BindCache<(
        wgpu::Buffer,
        wgpu::TextureView,
        wgpu::TextureView,
        wgpu::TextureView,
    )>,
}

impl SsaoPass {
    /// Ensures the occlusion texture exists and returns its layer 0 (AO, reflection visibility),
    /// the view the terrain samples. [`Self::layers_view`] is the forward pass's view of both
    /// layers.
    ///
    /// The compute pass ([`Self::dispatch`]) writes the matching storage view.
    pub fn prepare(
        &mut self,
        device: &crate::TrackedDevice,
        pool: &mut crate::TransientPool,
        width: u32,
        height: u32,
        mode: SsaoMode,
    ) -> Result<wgpu::TextureView, RenderError> {
        let (out_w, out_h) = match mode {
            SsaoMode::Off => (1, 1),
            SsaoMode::HalfRes | SsaoMode::GtaoHalfRes => {
                (width.div_ceil(2).max(1), height.div_ceil(2).max(1))
            }
            SsaoMode::GtaoFull => (width.max(1), height.max(1)),
        };
        self.ensure(device, pool, out_w, out_h)?;
        self.view
            .clone()
            .ok_or_else(|| RenderError::Shader("SSAO view missing".into()))
    }

    /// Both layers of the occlusion texture (a 2D array view: layer 0 AO and reflection
    /// visibility, layer 1 the bent normal's lean), once [`Self::prepare`] made it.
    pub(crate) fn layers_view(&self) -> Option<wgpu::TextureView> {
        self.layers.clone()
    }

    /// The next dispatches' blue-noise table (ADR-2075): `Some` runs the kernel's variant, which
    /// it builds here on the first call, and `None` runs the plain kernel.
    pub(crate) fn set_blue_noise(
        &mut self,
        device: &wgpu::Device,
        noise: Option<&std::sync::Arc<crate::blue_noise::BlueNoiseGpu>>,
    ) -> Result<(), RenderError> {
        self.blue_noise = noise.cloned();
        let (Some(noise), None) = (noise, &self.blue_pipelines) else {
            return Ok(());
        };
        self.ensure_pipelines(device)?;
        let device = &crate::device::TrackedDevice::observe(device);
        let module = compose_source(
            include_str!("../shaders/ssao.wgsl"),
            "ssao.wgsl",
            crate::blue_noise::variant_defs(),
        )?;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ssao blue noise"),
            source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module)),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ssao blue noise layout"),
            bind_group_layouts: &[self.bgl.as_ref(), Some(&noise.layout)],
            immediate_size: 0,
        });
        let compute = |label: &str, constants: &[(&str, f64)]| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                module: &module,
                entry_point: Some("cs_main"),
                compilation_options: wgpu::PipelineCompilationOptions {
                    constants,
                    ..Default::default()
                },
                cache: crate::pipeline_cache::cache_for(&device).as_deref(),
            })
        };
        self.blue_pipelines = Some((
            compute("ssao blue noise", &[]),
            compute("ssao bent blue noise", &[("BENT_NORMALS", 1.0)]),
        ));
        Ok(())
    }

    /// Scales the AO strength of the next dispatches by `scale` (the view's
    /// `PostSettings::ao_intensity_scale`, 0–4; 1 leaves [`SSAO_INTENSITY`] as it is).
    pub fn set_intensity_scale(&mut self, scale: f32) {
        self.intensity_scale = Some(if scale.is_finite() {
            scale.clamp(0.0, 4.0)
        } else {
            1.0
        });
    }

    /// Dispatches the kernel and the blur, and returns the occlusion view (layer 0; R in `0..1`,
    /// white = none). With `reflection` ([`crate::FrameQuality::reflection_occlusion`]) the
    /// GTAO rows also write the reflection visibility into G (a glossy specular lobe) and B (a
    /// rough one): the share of the lobe whose rays leave the depth buffer's enclosure
    /// (`ssao.wgsl` `reflection_visibility`). Otherwise, and on the half-res row, G and B are 1.
    ///
    /// `depth` must be a single-sample depth texture. `proj` is
    /// `[projection[0][0], projection[1][1], near, far]` of the camera that wrote it
    /// (DirectX-convention perspective, 0 = near). `noise_index` offsets the
    /// per-pixel kernel rotation (non-zero only under temporal AA). Callers skip this
    /// when MSAA is on and bind white instead.
    ///
    /// `probe` is the baked GI volume's probe-relative split (ADR-1104): where the volume's
    /// light covers a texel, the AO counts only the occlusion its probes do not already hold.
    /// `None` runs the whole kernel everywhere.
    ///
    /// `bent` = the camera-to-world rotation's columns (view-space x, y, z axes in world space)
    /// writes the bent normal's lean into layer 1 (`ssao/bent_normal.rs`, ADR-1375); `None`
    /// leaves layer 1 as it was (ADR-1491): the forward pass reads it only with bent normals on.
    #[allow(clippy::too_many_arguments)]
    pub fn dispatch(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        depth: &wgpu::TextureView,
        width: u32,
        height: u32,
        mode: SsaoMode,
        proj: [f32; 4],
        noise_index: f32,
        reflection: bool,
        probe: Option<&SsaoProbeSplit>,
        bent: Option<[[f32; 4]; 3]>,
    ) -> Result<wgpu::TextureView, RenderError> {
        let (out_w, out_h, samples, horizon) = match mode {
            SsaoMode::Off => return Err(RenderError::Shader("SSAO dispatch with mode off".into())),
            SsaoMode::HalfRes => (width.div_ceil(2).max(1), height.div_ceil(2).max(1), 8.0, 0.0),
            SsaoMode::GtaoHalfRes => (width.div_ceil(2).max(1), height.div_ceil(2).max(1), 12.0, 1.0),
            SsaoMode::GtaoFull => (width.max(1), height.max(1), 16.0, 1.0),
        };
        // The targets are this frame's from [`Self::prepare`] (same size): acquiring them again
        // here would take two more textures from the pool.
        self.ensure_pipelines(device)?;
        // The horizon kernel integrates the visible arc itself: `mode.z` is the post volume's
        // strength alone, not the point kernel's `SSAO_INTENSITY` gain.
        let horizon_kernel = self.horizon && horizon_row(mode);
        if horizon_kernel {
            self.ensure_horizon_pipelines(device)?;
        }
        let strength = if horizon_kernel { 1.0 } else { SSAO_INTENSITY };
        if self.texture.is_none() || self.size != (out_w, out_h) {
            return Err(RenderError::Shader(
                "SSAO dispatch before prepare at its size".into(),
            ));
        }
        // Cap the gather at a tenth of the AO target height: a close-up wall would
        // otherwise scatter samples across the whole screen. The horizon kernel takes a quarter:
        // its taps per texel are fixed (slices × steps), and at a tenth a 1 m box seen from 3.5 m
        // was searched only 29 cm up its face (0.83× the analytic occlusion of its foot, 0.94× at a
        // quarter; ADR-3202).
        let max_radius_px = (out_h as f32 * if horizon_kernel { 0.25 } else { 0.1 }).max(4.0);
        let params = SsaoParams {
            screen: [out_w as f32, out_h as f32, SSAO_RADIUS_M, samples],
            mode: [
                horizon,
                SSAO_BIAS,
                strength * self.intensity_scale.unwrap_or(1.0),
                max_radius_px,
            ],
            proj,
            noise: [
                noise_index,
                if reflection { horizon } else { 0.0 },
                if bent.is_some() { 1.0 } else { 0.0 },
                // Always 0: the blurs' sums start from it (`ssao.wgsl`, ADR-1560).
                0.0,
            ],
            probe: SsaoProbeSplit::params(probe),
            to_world: bent.unwrap_or([[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]]),
        };
        let device = &crate::device::TrackedDevice::observe(device);
        let params_buf = self.params.clone().expect("ssao params");
        belt.write_if_changed(
            device,
            encoder,
            &params_buf,
            bytemuck::bytes_of(&params),
            &mut self.param_bytes,
        );
        let sample = self.view.clone().expect("ssao view");
        let raw_storage = self.raw_storage.clone().expect("ssao raw storage");
        let depth_key = depth.clone();
        let layout = self.bgl.clone().expect("ssao bgl");
        let split_buf = match probe {
            Some(p) => p.buffer.clone(),
            None => self.no_split.clone().expect("ssao placeholder split"),
        };
        let bg = self.bg_cache.get_or_insert_with(
            (
                params_buf.clone(),
                depth_key,
                raw_storage.clone(),
                split_buf.clone(),
            ),
            || {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("ssao"),
                    layout: &layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: params_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(depth),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(&raw_storage),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: split_buf.as_entire_binding(),
                        },
                    ],
                })
            },
        );
        let storage = self.storage.clone().expect("ssao storage");
        let raw_sample = self.raw_sample.clone().expect("ssao raw sample");
        let blur_layout = self.blur_bgl.clone().expect("ssao blur bgl");
        let blur_bg = self.blur_cache.get_or_insert_with(
            (
                params_buf.clone(),
                depth.clone(),
                storage.clone(),
                raw_sample.clone(),
            ),
            || {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("ssao blur"),
                    layout: &blur_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: params_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(depth),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(&storage),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::TextureView(&raw_sample),
                        },
                    ],
                })
            },
        );
        {
            // One pass, two dispatches: wgpu syncs the raw AO write before the blur reads it.
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ssao"),
                timestamp_writes: None,
            });
            // Bent normals select the pipelines compiled with them (ADR-1491): their kernel sums
            // the lean, and their blur is the tiled one (the same bytes as tap by tap).
            let (main, blur) = if bent.is_some() {
                (self.pipeline_bent.as_ref(), self.blur_tiled_pipeline.as_ref())
            } else {
                (self.pipeline.as_ref(), self.blur_pipeline.as_ref())
            };
            let horizon_pipes = self.horizon_pipelines.as_ref().filter(|_| horizon_kernel);
            match (&self.blue_noise, &self.blue_pipelines, horizon_pipes) {
                (
                    Some(noise),
                    _,
                    Some(HorizonPipelines {
                        blue: Some((plain, bent_pipe)),
                        ..
                    }),
                )
                | (Some(noise), Some((plain, bent_pipe)), None) => {
                    pass.counted_set_pipeline(if bent.is_some() { bent_pipe } else { plain });
                    pass.counted_set_bind_group(1, &noise.group, &[]);
                }
                (_, _, Some(h)) => pass.counted_set_pipeline(if bent.is_some() { &h.bent } else { &h.plain }),
                _ => pass.counted_set_pipeline(main.expect("ssao pipeline")),
            }
            pass.counted_set_bind_group(0, &bg, &[]);
            pass.counted_dispatch(out_w.div_ceil(8), out_h.div_ceil(8), 1);
            pass.counted_set_pipeline(blur.expect("ssao blur pipeline"));
            pass.counted_set_bind_group(0, &blur_bg, &[]);
            pass.counted_dispatch(out_w.div_ceil(8), out_h.div_ceil(8), 1);
        }
        Ok(sample)
    }

    /// The horizon kernel's pipelines (ADR-3202), once, and its blue-noise variants once a table
    /// is set: the same layouts as the point kernel's.
    fn ensure_horizon_pipelines(&mut self, device: &wgpu::Device) -> Result<(), RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        let compute = |label: &str,
                       layout: &wgpu::PipelineLayout,
                       module: &wgpu::ShaderModule,
                       constants: &[(&str, f64)]| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                module,
                entry_point: Some("cs_horizon"),
                compilation_options: wgpu::PipelineCompilationOptions {
                    constants,
                    ..Default::default()
                },
                cache: crate::pipeline_cache::cache_for(device).as_deref(),
            })
        };
        const BENT: &[(&str, f64)] = &[("BENT_NORMALS", 1.0)];
        if self.horizon_pipelines.is_none() {
            let module = compose_source(include_str!("../shaders/ssao.wgsl"), "ssao.wgsl", HashMap::new())?;
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("ssao horizon"),
                source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module)),
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("ssao horizon layout"),
                bind_group_layouts: &[self.bgl.as_ref()],
                immediate_size: 0,
            });
            self.horizon_pipelines = Some(HorizonPipelines {
                plain: compute("ssao horizon", &layout, &module, &[]),
                bent: compute("ssao horizon bent", &layout, &module, BENT),
                blue: None,
            });
        }
        if let (Some(noise), Some(pipes)) = (&self.blue_noise, self.horizon_pipelines.as_mut())
            && pipes.blue.is_none()
        {
            let module = compose_source(
                include_str!("../shaders/ssao.wgsl"),
                "ssao.wgsl",
                crate::blue_noise::variant_defs(),
            )?;
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("ssao horizon blue noise"),
                source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module)),
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("ssao horizon blue noise layout"),
                bind_group_layouts: &[self.bgl.as_ref(), Some(&noise.layout)],
                immediate_size: 0,
            });
            pipes.blue = Some((
                compute("ssao horizon blue noise", &layout, &module, &[]),
                compute("ssao horizon bent blue noise", &layout, &module, BENT),
            ));
        }
        Ok(())
    }

    /// The pipelines and buffers ([`Self::ensure_pipelines`]) and the two targets at `width` ×
    /// `height`.
    fn ensure(
        &mut self,
        device: &crate::TrackedDevice,
        pool: &mut crate::TransientPool,
        width: u32,
        height: u32,
    ) -> Result<(), RenderError> {
        self.ensure_pipelines(device)?;
        self.ensure_targets(device, pool, width, height);
        Ok(())
    }

    /// The kernel and blur pipelines, their layouts and the parameter buffers (once).
    fn ensure_pipelines(&mut self, device: &wgpu::Device) -> Result<(), RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        if self.pipeline.is_none() {
            let module = compose_source(include_str!("../shaders/ssao.wgsl"), "ssao.wgsl", HashMap::new())?;
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("ssao"),
                source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module)),
            });
            let uniform = wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            };
            let depth = wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            };
            let out = wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    view_dimension: wgpu::TextureViewDimension::D2Array,
                },
                count: None,
            };
            let raw = wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2Array,
                    multisampled: false,
                },
                count: None,
            };
            let split = wgpu::BindGroupLayoutEntry {
                binding: 4,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            };
            self.bgl = Some(device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("ssao bgl"),
                entries: &[uniform, depth, out, split],
            }));
            self.blur_bgl = Some(device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("ssao blur bgl"),
                entries: &[uniform, depth, out, raw],
            }));
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("ssao layout"),
                bind_group_layouts: &[Some(self.bgl.as_ref().expect("ssao bgl"))],
                immediate_size: 0,
            });
            let blur_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("ssao blur layout"),
                bind_group_layouts: &[Some(self.blur_bgl.as_ref().expect("ssao blur bgl"))],
                immediate_size: 0,
            });
            // `BENT_NORMALS` (ADR-1491): off is the shader's default; the bent pipelines set it.
            const BENT: &[(&str, f64)] = &[("BENT_NORMALS", 1.0)];
            let compute =
                |label: &str, layout: &wgpu::PipelineLayout, entry: &str, constants: &[(&str, f64)]| {
                    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                        label: Some(label),
                        layout: Some(layout),
                        module: &module,
                        entry_point: Some(entry),
                        compilation_options: wgpu::PipelineCompilationOptions {
                            constants,
                            ..Default::default()
                        },
                        cache: crate::pipeline_cache::cache_for(&device).as_deref(),
                    })
                };
            self.pipeline = Some(compute("ssao", &layout, "cs_main", &[]));
            self.blur_pipeline = Some(compute("ssao blur", &blur_layout, "cs_blur", &[]));
            self.pipeline_bent = Some(compute("ssao bent", &layout, "cs_main", BENT));
            self.blur_tiled_pipeline = Some(compute("ssao blur tiled", &blur_layout, "cs_blur_tiled", BENT));
            self.params = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ssao params"),
                size: std::mem::size_of::<SsaoParams>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.no_split = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ssao no probe split"),
                size: 16,
                usage: wgpu::BufferUsages::STORAGE,
                mapped_at_creation: false,
            }));
        }
        Ok(())
    }

    /// The kernel's raw AO and the blurred result, with their views and groups.
    fn ensure_targets(
        &mut self,
        device: &crate::TrackedDevice,
        pool: &mut crate::TransientPool,
        width: u32,
        height: u32,
    ) {
        // The kernel's raw AO and the blurred result: transients of the view's pool keyed by
        // size (ADR-2018), the same two textures every frame at one size. The views and bind
        // groups are rebuilt when the pool hands out a different texture.
        let desc = crate::TextureDesc::new(width, height, wgpu::TextureFormat::Rgba8Unorm)
            .with_layers(SSAO_LAYERS)
            .with_usage(
                wgpu::TextureUsages::STORAGE_BINDING
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | crate::resource_inspector::debug_usage(),
            );
        let pooled = pool.acquire(device, &desc, "ssao");
        let pooled_raw = pool.acquire(device, &desc, "ssao raw");
        if self.generations != (pooled.generation, pooled_raw.generation) || self.texture.is_none() {
            let views = |texture: &wgpu::Texture, label: &str| {
                // Two views of both layers: storage (written by a kernel) and sampled.
                let storage = texture.create_view(&wgpu::TextureViewDescriptor {
                    label: Some(label),
                    format: Some(wgpu::TextureFormat::Rgba8Unorm),
                    dimension: Some(wgpu::TextureViewDimension::D2Array),
                    usage: Some(wgpu::TextureUsages::STORAGE_BINDING),
                    ..Default::default()
                });
                let sample = texture.create_view(&wgpu::TextureViewDescriptor {
                    label: Some(label),
                    format: Some(wgpu::TextureFormat::Rgba8Unorm),
                    dimension: Some(wgpu::TextureViewDimension::D2Array),
                    usage: Some(wgpu::TextureUsages::TEXTURE_BINDING),
                    ..Default::default()
                });
                (storage, sample)
            };
            let texture = pooled.texture;
            let raw = pooled_raw.texture;
            let (storage, sample) = views(&texture, "ssao");
            let (raw_storage, raw_sample) = views(&raw, "ssao raw");
            // Layer 0 alone, for the terrain's engine AO (a plain 2D binding).
            self.view = Some(texture.create_view(&wgpu::TextureViewDescriptor {
                label: Some("ssao layer 0"),
                format: Some(wgpu::TextureFormat::Rgba8Unorm),
                dimension: Some(wgpu::TextureViewDimension::D2),
                usage: Some(wgpu::TextureUsages::TEXTURE_BINDING),
                base_array_layer: 0,
                array_layer_count: Some(1),
                ..Default::default()
            }));
            self.layers = Some(sample);
            self.storage = Some(storage);
            self.raw_storage = Some(raw_storage);
            self.raw_sample = Some(raw_sample);
            self.texture = Some(texture);
            self.raw_texture = Some(raw);
            self.size = (width, height);
            self.generations = (pooled.generation, pooled_raw.generation);
            self.bg_cache.clear();
            self.blur_cache.clear();
        }
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::ContractRow::new(
        "ssao.wgsl",
        "SsaoParams",
        size_of::<SsaoParams>(),
        vec![
            ("screen", std::mem::offset_of!(SsaoParams, screen)),
            ("mode", std::mem::offset_of!(SsaoParams, mode)),
            ("proj", std::mem::offset_of!(SsaoParams, proj)),
            ("noise", std::mem::offset_of!(SsaoParams, noise)),
            ("probe", std::mem::offset_of!(SsaoParams, probe)),
            ("to_world_x", std::mem::offset_of!(SsaoParams, to_world)),
            ("to_world_y", std::mem::offset_of!(SsaoParams, to_world) + 16),
            ("to_world_z", std::mem::offset_of!(SsaoParams, to_world) + 32),
        ],
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_params_match_the_shader_struct() {
        // Eight vec4 rows: the probe split, then the camera-to-world columns (`ssao.wgsl`
        // `SsaoParams`).
        assert_eq!(std::mem::size_of::<SsaoParams>(), 128);
        let src = include_str!("../shaders/ssao.wgsl");
        let start = src.find("struct SsaoParams {").expect("struct");
        let end = src[start..].find('}').expect("struct end");
        let fields: Vec<&str> = src[start..start + end]
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//") && l.contains(": vec4<"))
            .collect();
        assert_eq!(
            fields,
            [
                "screen: vec4<f32>,",
                "mode: vec4<f32>,",
                "proj: vec4<f32>,",
                "noise: vec4<f32>,",
                "probe: vec4<u32>,",
                "to_world_x: vec4<f32>,",
                "to_world_y: vec4<f32>,",
                "to_world_z: vec4<f32>,"
            ]
        );
        // No split: the kernel reads no split words.
        assert_eq!(SsaoProbeSplit::params(None), [0; 4]);
    }
}
