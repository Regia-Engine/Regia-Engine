// SPDX-License-Identifier: MPL-2.0
//! The GPU twin of [`super::GiDenoiser`]: the same passes in compute
//! (`shaders/gi_denoise.wgsl`: reproject, anti-lag, temporal, variance, à-trous, store), on
//! storage buffers.
//!
//! [`GiDenoiseGpu::denoise`] uploads one frame, runs the passes in one compute pass and reads
//! the result back (it blocks on the readback: an offline, tool and test entry point, not a
//! frame-path one). The history stays on the GPU between calls. The CPU reference is the
//! specification: `tests/gi_denoise_gpu.rs` holds the two within a small tolerance on every
//! regression scenario.

use super::{GiDenoiseError, GiDenoiseInput, GiDenoiseOutput, GiDenoiseSettings};
use crate::RenderDevice;
use crate::frame_work::counted::*;

const SOURCE: &str = include_str!("../../shaders/gi_denoise.wgsl");
const WORKGROUP: u32 = 8;
/// `vec4<f32>` slots per pixel in the frame input, history and temporal buffers.
const INPUT_SLOTS: u64 = 3;
const HISTORY_SLOTS: u64 = 3;
const ACC_SLOTS: u64 = 2;
const VEC4: u64 = 16;
/// Parameter slots: one for the reproject, anti-lag, temporal, variance and store passes, one
/// per à-trous pass.
const PARAM_SLOTS: u64 = 1 + super::MAX_ATROUS_ITERATIONS as u64;
const FLAG_HISTORY: u32 = 1;
const FLAG_STORE_FIRST_PASS: u32 = 2;

/// `Params` in the shader.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct ParamsGpu {
    size: [u32; 4],
    temporal: [f32; 4],
    edge: [f32; 4],
    extra: [f32; 4],
    antilag: [f32; 4],
}

struct Buffers {
    size: (u32, u32),
    input: wgpu::Buffer,
    /// Bound only (the bind groups read and write it); held for ownership.
    _history: wgpu::Buffer,
    /// Anti-lag squares, sized for the smallest square (2 px); bound only.
    _blocks: wgpu::Buffer,
    acc: wgpu::Buffer,
    ping: wgpu::Buffer,
    pong: wgpu::Buffer,
    /// `src` = pong, `dst` = ping.
    to_ping: wgpu::BindGroup,
    /// `src` = ping, `dst` = pong.
    to_pong: wgpu::BindGroup,
}

/// The GPU denoiser: pipelines, buffers sized to the last frame, and the history.
pub struct GiDenoiseGpu {
    device: RenderDevice,
    settings: GiDenoiseSettings,
    layout: wgpu::BindGroupLayout,
    reproject: wgpu::ComputePipeline,
    antilag: wgpu::ComputePipeline,
    temporal: wgpu::ComputePipeline,
    variance: wgpu::ComputePipeline,
    atrous: wgpu::ComputePipeline,
    store: wgpu::ComputePipeline,
    params: wgpu::Buffer,
    param_stride: u64,
    buffers: Option<Buffers>,
    has_history: bool,
    frames: u64,
}

impl std::fmt::Debug for GiDenoiseGpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GiDenoiseGpu")
            .field("settings", &self.settings)
            .field("size", &self.buffers.as_ref().map(|b| b.size))
            .field("has_history", &self.has_history)
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}

fn storage_entry(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

impl GiDenoiseGpu {
    /// Compiles the passes on `device`.
    ///
    /// # Errors
    ///
    /// [`GiDenoiseError::InvalidSetting`] when `settings` fails validation.
    pub fn new(device: &RenderDevice, settings: GiDenoiseSettings) -> Result<Self, GiDenoiseError> {
        settings.validate()?;
        let d = device.tracked();
        let module = device.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gi denoise"),
            source: crate::wgsl_source(SOURCE),
        });
        let layout = device
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("gi denoise bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: true,
                            min_binding_size: wgpu::BufferSize::new(size_of::<ParamsGpu>() as u64),
                        },
                        count: None,
                    },
                    storage_entry(1, true),
                    storage_entry(2, false),
                    storage_entry(3, false),
                    storage_entry(4, true),
                    storage_entry(5, false),
                    storage_entry(6, false),
                ],
            });
        let pipeline_layout = device
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("gi denoise"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
        let pipe = |entry: &str| {
            d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: crate::pipeline_cache::cache_for(&device.device).as_deref(),
            })
        };
        let reproject = pipe("reproject");
        let antilag = pipe("antilag");
        let temporal = pipe("temporal");
        let variance = pipe("variance");
        let atrous = pipe("atrous");
        let store = pipe("store_history");
        let align = u64::from(device.device.limits().min_uniform_buffer_offset_alignment).max(1);
        let param_stride = (size_of::<ParamsGpu>() as u64).div_ceil(align) * align;
        let params = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gi denoise params"),
            size: param_stride * PARAM_SLOTS,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Ok(Self {
            device: device.clone(),
            settings,
            layout,
            reproject,
            antilag,
            temporal,
            variance,
            atrous,
            store,
            params,
            param_stride,
            buffers: None,
            has_history: false,
            frames: 0,
        })
    }

    /// The settings in use.
    #[must_use]
    pub fn settings(&self) -> &GiDenoiseSettings {
        &self.settings
    }

    /// Replaces the settings; the history is kept (as [`super::GiDenoiser::set_settings`]).
    ///
    /// # Errors
    ///
    /// [`GiDenoiseError::InvalidSetting`]; the old settings stay.
    pub fn set_settings(&mut self, settings: GiDenoiseSettings) -> Result<(), GiDenoiseError> {
        settings.validate()?;
        self.settings = settings;
        Ok(())
    }

    /// Drops the history (a camera cut).
    pub fn reset(&mut self) {
        self.has_history = false;
    }

    /// Frames denoised since construction.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames
    }

    fn ensure_buffers(&mut self, width: u32, height: u32) -> Result<(), GiDenoiseError> {
        if self.buffers.as_ref().is_some_and(|b| b.size == (width, height)) {
            return Ok(());
        }
        let pixels = u64::from(width) * u64::from(height);
        let largest = pixels * INPUT_SLOTS.max(HISTORY_SLOTS) * VEC4;
        let limit = self.device.device.limits().max_storage_buffer_binding_size;
        if largest > limit {
            return Err(GiDenoiseError::Gpu(format!(
                "{width}×{height} needs a {largest}-byte storage buffer; the device allows {limit}"
            )));
        }
        let buffer = |label: &str, slots: u64, extra: wgpu::BufferUsages| {
            self.device.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: pixels * slots * VEC4,
                usage: wgpu::BufferUsages::STORAGE | extra,
                mapped_at_creation: false,
            })
        };
        let input = buffer("gi denoise input", INPUT_SLOTS, wgpu::BufferUsages::COPY_DST);
        let history = buffer("gi denoise history", HISTORY_SLOTS, wgpu::BufferUsages::empty());
        let acc = buffer("gi denoise temporal", ACC_SLOTS, wgpu::BufferUsages::COPY_SRC);
        let squares = u64::from(width.div_ceil(2)) * u64::from(height.div_ceil(2));
        let blocks = self.device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gi denoise anti-lag squares"),
            size: squares * VEC4,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let ping = buffer("gi denoise ping", 1, wgpu::BufferUsages::COPY_SRC);
        let pong = buffer("gi denoise pong", 1, wgpu::BufferUsages::COPY_SRC);
        let d = self.device.tracked();
        let bind = |src: &wgpu::Buffer, dst: &wgpu::Buffer, label: &str| {
            d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &self.params,
                            offset: 0,
                            size: wgpu::BufferSize::new(size_of::<ParamsGpu>() as u64),
                        }),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: input.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: history.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: acc.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: src.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 5,
                        resource: dst.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 6,
                        resource: blocks.as_entire_binding(),
                    },
                ],
            })
        };
        let to_ping = bind(&pong, &ping, "gi denoise to ping");
        let to_pong = bind(&ping, &pong, "gi denoise to pong");
        self.buffers = Some(Buffers {
            size: (width, height),
            input,
            _history: history,
            _blocks: blocks,
            acc,
            ping,
            pong,
            to_ping,
            to_pong,
        });
        self.has_history = false;
        Ok(())
    }

    fn params_for(&self, width: u32, height: u32, stride: u32, flags: u32) -> ParamsGpu {
        let s = &self.settings;
        let side = s.antilag_block;
        ParamsGpu {
            size: [width, height, stride, flags],
            temporal: [s.alpha_color, s.alpha_moments, s.max_history, s.depth_tolerance],
            edge: [s.normal_tolerance, s.sigma_depth, s.depth_epsilon, s.sigma_normal],
            extra: [
                s.sigma_luminance,
                s.spatial_variance_frames,
                s.variance_radius as f32,
                0.0,
            ],
            antilag: [
                s.antilag_sigma,
                side as f32,
                width.div_ceil(side) as f32,
                height.div_ceil(side) as f32,
            ],
        }
    }

    /// Filters one frame on the GPU and reads it back (blocking). Same contract as
    /// [`super::GiDenoiser::denoise`].
    ///
    /// # Errors
    ///
    /// [`GiDenoiseError::EmptyFrame`], [`GiDenoiseError::SizeMismatch`], or
    /// [`GiDenoiseError::Gpu`] when the frame does not fit the device's storage buffers or the
    /// readback fails.
    pub fn denoise(&mut self, input: &GiDenoiseInput<'_>) -> Result<GiDenoiseOutput, GiDenoiseError> {
        let _span = tracing::trace_span!("gi_denoise_gpu").entered();
        let pixels = input.check()?;
        let (width, height) = (input.width, input.height);
        self.ensure_buffers(width, height)?;
        let Some(buffers) = self.buffers.as_ref() else {
            return Err(GiDenoiseError::Gpu("buffers were not created".into()));
        };

        let mut packed = Vec::with_capacity(pixels * INPUT_SLOTS as usize);
        for ((c, g), m) in input.noisy.iter().zip(input.guide).zip(input.motion) {
            packed.push([c[0], c[1], c[2], 0.0]);
            packed.push(*g);
            packed.push([m[0], m[1], 0.0, 0.0]);
        }
        let queue = &self.device.queue;
        queue.write_buffer(&buffers.input, 0, bytemuck::cast_slice(&packed));

        let iterations = self.settings.atrous_iterations;
        let store_first = self.settings.history_from_first_pass && iterations > 0;
        let mut flags = 0;
        if self.has_history {
            flags |= FLAG_HISTORY;
        }
        if store_first {
            flags |= FLAG_STORE_FIRST_PASS;
        }
        let mut param_bytes = vec![0u8; (self.param_stride * PARAM_SLOTS) as usize];
        let mut write_slot = |slot: u64, p: ParamsGpu| {
            let start = (slot * self.param_stride) as usize;
            param_bytes[start..start + size_of::<ParamsGpu>()].copy_from_slice(bytemuck::bytes_of(&p));
        };
        write_slot(0, self.params_for(width, height, 1, flags));
        for k in 0..iterations {
            write_slot(u64::from(k) + 1, self.params_for(width, height, 1 << k, flags));
        }
        queue.write_buffer(&self.params, 0, &param_bytes);

        let groups = (width.div_ceil(WORKGROUP), height.div_ceil(WORKGROUP));
        let side = self.settings.antilag_block;
        let square_groups = (
            width.div_ceil(side).div_ceil(WORKGROUP),
            height.div_ceil(side).div_ceil(WORKGROUP),
        );
        let slot_offset = |slot: u32| (u64::from(slot) * self.param_stride) as u32;
        let mut encoder = self
            .device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("gi denoise"),
            });
        {
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("gi denoise"),
                timestamp_writes: None,
            });
            // Reproject, anti-lag, temporal and variance: the variance pass writes ping.
            pass.counted_set_bind_group(0, &buffers.to_ping, &[slot_offset(0)]);
            pass.counted_set_pipeline(&self.reproject);
            pass.counted_dispatch(groups.0, groups.1, 1);
            pass.counted_set_pipeline(&self.antilag);
            pass.counted_dispatch(square_groups.0, square_groups.1, 1);
            pass.counted_set_pipeline(&self.temporal);
            pass.counted_dispatch(groups.0, groups.1, 1);
            pass.counted_set_pipeline(&self.variance);
            pass.counted_dispatch(groups.0, groups.1, 1);
            // À-trous: ping -> pong on even passes, pong -> ping on odd ones.
            pass.counted_set_pipeline(&self.atrous);
            for k in 0..iterations {
                let bind_group = if k.is_multiple_of(2) {
                    &buffers.to_pong
                } else {
                    &buffers.to_ping
                };
                pass.counted_set_bind_group(0, bind_group, &[slot_offset(k + 1)]);
                pass.counted_dispatch(groups.0, groups.1, 1);
                if k == 0 && store_first {
                    // `to_ping` reads pong, the first pass's output, as `src`.
                    pass.counted_set_bind_group(0, &buffers.to_ping, &[slot_offset(0)]);
                    pass.counted_set_pipeline(&self.store);
                    pass.counted_dispatch(groups.0, groups.1, 1);
                    pass.counted_set_pipeline(&self.atrous);
                }
            }
            if !store_first {
                pass.counted_set_bind_group(0, &buffers.to_ping, &[slot_offset(0)]);
                pass.counted_set_pipeline(&self.store);
                pass.counted_dispatch(groups.0, groups.1, 1);
            }
        }
        let result = if iterations.is_multiple_of(2) {
            &buffers.ping
        } else {
            &buffers.pong
        };
        let color_bytes = pixels as u64 * VEC4;
        let acc_bytes = pixels as u64 * ACC_SLOTS * VEC4;
        let staging = self.device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gi denoise readback"),
            size: color_bytes + acc_bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_buffer_to_buffer(result, 0, &staging, 0, color_bytes);
        encoder.copy_buffer_to_buffer(&buffers.acc, 0, &staging, color_bytes, acc_bytes);
        queue.submit([encoder.finish()]);

        let (tx, rx) = std::sync::mpsc::channel();
        staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device
            .tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| GiDenoiseError::Gpu(format!("poll: {e}")))?;
        rx.recv()
            .map_err(|_| GiDenoiseError::Gpu("map callback dropped".into()))?
            .map_err(|e| GiDenoiseError::Gpu(format!("map: {e}")))?;
        let mapped = staging
            .slice(..)
            .get_mapped_range()
            .map_err(|e| GiDenoiseError::Gpu(format!("mapped range: {e}")))?;
        let floats: Vec<f32> = mapped
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        drop(mapped);
        staging.unmap();
        let (color_part, acc_part) = floats.split_at(pixels * 4);
        let (color_px, _) = color_part.as_chunks::<4>();
        let (acc_px, _) = acc_part.as_chunks::<8>();
        let out = GiDenoiseOutput {
            color: color_px.iter().map(|v| [v[0], v[1], v[2]]).collect(),
            variance: color_px.iter().map(|v| v[3]).collect(),
            history_len: acc_px.iter().map(|v| v[3]).collect(),
        };
        self.has_history = true;
        self.frames += 1;
        Ok(out)
    }

    /// Buffer bytes the denoiser holds on the GPU at a `width × height` frame: input,
    /// history, temporal and the two ping-pong buffers (160 bytes per pixel) plus the anti-lag
    /// squares (16 bytes per 2×2 pixels).
    #[must_use]
    pub fn gpu_bytes(width: u32, height: u32) -> u64 {
        u64::from(width) * u64::from(height) * (INPUT_SLOTS + HISTORY_SLOTS + ACC_SLOTS + 2) * VEC4
            + u64::from(width.div_ceil(2)) * u64::from(height.div_ceil(2)) * VEC4
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "gi_denoise.wgsl",
        "Params",
        ParamsGpu {
            size,
            temporal,
            edge,
            extra,
            antilag
        }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_match_the_shader_layout() {
        // Five vec4s; the uniform slot stride rounds up to the offset alignment.
        assert_eq!(size_of::<ParamsGpu>(), 80);
        // Input 3, history 3, temporal 2 and the ping-pong pair: 10 vec4s per pixel.
        assert_eq!(GiDenoiseGpu::gpu_bytes(64, 48), 64 * 48 * 160 + 32 * 24 * 16);
    }

    #[test]
    fn the_shader_parses_and_validates() {
        let module = naga::front::wgsl::parse_str(SOURCE).expect("gi_denoise.wgsl parses");
        let _info = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("gi_denoise.wgsl validates");
        for entry in [
            "reproject",
            "antilag",
            "temporal",
            "variance",
            "atrous",
            "store_history",
        ] {
            assert!(
                module.entry_points.iter().any(|e| e.name == entry),
                "entry point {entry}"
            );
        }
    }
}
