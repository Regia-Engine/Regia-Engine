// SPDX-License-Identifier: MPL-2.0
//! The GPU half of the FFT ocean (ADR-1969, `docs/specs/water-ocean.md`): the compute pass that
//! evolves the spectrum and runs the inverse FFT, and the maps `water_body.wgsl` reads.
//!
//! The work is in storage buffers, so nothing here needs a storage-texture format. Each frame that
//! a High or Ultra ocean is on screen, the pass records one spectrum dispatch and `2 log2 N` FFT
//! dispatches per band, then copies the result into an `Rgba32Float` array (two layers per band).
//! A water frame on Low, an ocean switched off, or a GPU without the compute limits binds a 1 × 1
//! stand-in and no body rides the ocean: the swell draws alone.
//!
//! One frame simulates one ocean: the first drawn body's. Bodies with the same [`FftOcean`] share
//! its maps; any other ocean body falls to its swell and is counted as dropped.
//!
//! The map size is the tier's, or the ocean's [`FftOcean::grid`] (ADR-2126). A grid the device has
//! no limits for drops that ocean to its swell. `OceanGpu::measure` times the pass at any grid.
//!
//! Lingering foam (ADR-2843, [`super::ocean_foam`]): an ocean with [`FftOcean::foam_decay`] on
//! gets one more dispatch per band after the FFT, which steps a history map kept in the tail of
//! the first work buffer, and three more layers in the map array (layer `2 · bands + band`). An
//! ocean without it allocates and dispatches exactly what it did before.

use std::sync::Arc;
use std::time::Instant;

use bytemuck::{Pod, Zeroable};
use regia_render::RenderError;
use serde::Serialize;

use super::ocean::{FftOcean, OCEAN_BANDS, OCEAN_GRIDS, OceanTables, loop_fraction, tables_for};
use super::ocean_foam::{FoamRates, band_foam_scales, foam_interval};
use super::swell::{SwellSample, crest_foam};
use super::{MAX_BODIES, WaterBody, WaterTier};

/// The compute pass's name in the frame graph (`render.graph`).
pub const OCEAN_PASS_NAME: &str = "water_ocean_fft";

/// The compute shader (`water_ocean.wgsl`).
pub const OCEAN_SHADER: &str = include_str!("water_ocean.wgsl");

/// `vec4`s of the ocean uniform (binding 12): the header, the shore, two per band.
pub(super) const OCEAN_VEC4S: usize = 2 + 2 * OCEAN_BANDS;

/// Invocations per dispatch side of `water_ocean.wgsl`.
const WORKGROUP: u32 = 16;

/// Layers of the map array per band.
const LAYERS_PER_BAND: u32 = 2;

/// The compute pass's maps read back from the GPU ([`super::WaterRenderer::read_ocean_maps`]).
#[derive(Debug, Clone, PartialEq)]
pub struct OceanMaps {
    /// The ocean the maps hold: the frame's, whichever body it came from.
    pub ocean: FftOcean,
    /// Texels per side.
    pub size: u32,
    /// Every texel: per band, layer 0 `(Dx, h, Dz, Dxz)` then layer 1 `(hx, hz, Dxx, Dzz)`, row
    /// along z.
    pub texels: Vec<[f32; 4]>,
}

impl OceanMaps {
    /// The texel `(column, row)` of `layer` of `band`.
    #[must_use]
    pub fn texel(&self, band: usize, layer: usize, column: u32, row: u32) -> [f32; 4] {
        let n = self.size as usize;
        let first = (2 * band + layer) * n * n;
        self.texels
            .get(first + row as usize * n + column as usize)
            .copied()
            .unwrap_or([0.0; 4])
    }

    /// The displacement of `band` at a texel: `[Dx, h, Dz]` (m).
    #[must_use]
    pub fn displacement(&self, band: usize, column: u32, row: u32) -> [f32; 3] {
        let [dx, h, dz, _] = self.texel(band, 0, column, row);
        [dx, h, dz]
    }

    /// The surface `band` alone makes at a texel: displacement and tangents, whose
    /// [`SwellSample::normal`] is the normal map and whose [`SwellSample::jacobian`] drives the
    /// foam map. The maps hold no velocity (the CPU mirror does): it is zero here.
    #[must_use]
    pub fn band_sample(&self, band: usize, column: u32, row: u32) -> SwellSample {
        let [dx, h, dz, dxz] = self.texel(band, 0, column, row);
        let [hx, hz, dxx, dzz] = self.texel(band, 1, column, row);
        let dpdx = [1.0 + dxx, hx, dxz];
        let dpdz = [dxz, hz, 1.0 + dzz];
        SwellSample {
            offset: [dx, h, dz],
            dpdx,
            dpdz,
            velocity: [0.0; 3],
            jacobian: dpdx[0] * dpdz[2] - dpdx[2] * dpdz[0],
        }
    }

    /// The crest foam `band` alone gives at a texel, with this ocean's foam settings. The drawn
    /// foam takes the Jacobian of the sum of the bands, in the fragment shader.
    #[must_use]
    pub fn band_foam(&self, band: usize, column: u32, row: u32) -> f32 {
        let jacobian = self.band_sample(band, column, row).jacobian;
        crest_foam(jacobian, self.ocean.crest_foam, self.ocean.foam_onset)
    }

    /// The lingering-foam history of `band` at a texel (ADR-2843): `None` when the ocean keeps
    /// none ([`FftOcean::foam_decay`] 0), so the maps hold no history layers.
    #[must_use]
    pub fn history(&self, band: usize, column: u32, row: u32) -> Option<f32> {
        let n = self.size as usize;
        if band >= OCEAN_BANDS || column as usize >= n || row as usize >= n {
            return None;
        }
        let first = (LAYERS_PER_BAND as usize * OCEAN_BANDS + band) * n * n;
        self.texels
            .get(first + row as usize * n + column as usize)
            .map(|t| t[0])
    }
}

/// What the compute pass costs at one grid on the device in use
/// ([`super::WaterRenderer::measure_ocean_cost`]). Times are measured, not promised: they move with
/// the card, the driver and the load.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct OceanCost {
    /// Texels per side.
    pub size: u32,
    /// Evolves timed.
    pub frames: u32,
    /// Compute dispatches per evolve.
    pub dispatches: u32,
    /// Non-empty modes of the three bands.
    pub modes: u64,
    /// GPU memory the ocean holds at this grid: the modes, both work buffers, the map array and
    /// the small tables (bytes).
    pub gpu_bytes: u64,
    /// Median GPU time of the compute pass, by timestamps (ms; `None` without encoder timestamps).
    pub compute_ms: Option<f64>,
    /// Median GPU time of the copy into the map array (ms; `None` as above).
    pub copy_ms: Option<f64>,
    /// Median wall time from submit to done, the whole evolve (ms). An upper bound on its GPU time.
    pub submit_ms: f64,
    /// CPU time to build the spectrum tables, once per ocean and grid (ms).
    pub tables_ms: f64,
    /// CPU time to create and fill the GPU buffers, once per ocean and grid (ms).
    pub upload_ms: f64,
    /// GPU memory of the lingering-foam history, included in [`Self::gpu_bytes`]: the history
    /// tail of the work buffer and its layers of the map array (bytes; 0 with the history off,
    /// ADR-2843).
    pub history_bytes: u64,
    /// Dispatches of the history step, included in [`Self::dispatches`] (0 with it off).
    pub history_dispatches: u32,
}

/// One entry of the stage table (`Params` in `water_ocean.wgsl`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct StageParams {
    size: u32,
    base: u32,
    table: u32,
    tau_or_p: u32,
    axis: u32,
    choppiness: f32,
    /// The foam stage's growth and decay ([`FoamRates`]); zero in the other stages.
    extra: [f32; 2],
}

/// The FFT stages of one transform of `size` texels: `(axis, sorted run p, twiddle step)` of each,
/// along x and then along z.
fn fft_stages(size: u32) -> Vec<(u32, u32, u32)> {
    let mut stages = Vec::new();
    for axis in 0..2 {
        let mut p = 1;
        while p < size {
            stages.push((axis, p, size / (2 * p)));
            p *= 2;
        }
    }
    stages
}

/// The compute pipelines, built once for the first ocean.
struct Pipelines {
    spectrum: wgpu::ComputePipeline,
    fft: wgpu::ComputePipeline,
    /// The lingering-foam step (ADR-2843), built on the first ocean that keeps a history, so a
    /// default ocean never compiles it (`None` inside: the device refused it, logged once).
    foam: std::sync::OnceLock<Option<wgpu::ComputePipeline>>,
    module: wgpu::ShaderModule,
    pipeline_layout: wgpu::PipelineLayout,
    layout: wgpu::BindGroupLayout,
}

impl Pipelines {
    fn new(device: &wgpu::Device) -> Option<Self> {
        let storage = |binding: u32, read_only: bool| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("water ocean layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: wgpu::BufferSize::new(size_of::<StageParams>() as u64),
                    },
                    count: None,
                },
                storage(1, true),
                storage(2, true),
                storage(3, true),
                storage(4, false),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("water ocean pipeline layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("water_ocean.wgsl"),
            source: regia_render::wgsl_source(OCEAN_SHADER),
        });
        let spectrum = compute_pipeline(
            device,
            &pipeline_layout,
            &module,
            "water ocean spectrum",
            "cs_spectrum",
        );
        let fft = compute_pipeline(device, &pipeline_layout, &module, "water ocean fft", "cs_fft");
        if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
            tracing::error!(target: "regia_terrain", "water ocean pipelines: {err}");
            return None;
        }
        Some(Self {
            spectrum,
            fft,
            foam: std::sync::OnceLock::new(),
            module,
            pipeline_layout,
            layout,
        })
    }

    /// The lingering-foam pipeline, built on the first ask; `None` when the device refused it (an
    /// ocean then keeps its instant foam only).
    fn foam(&self, device: &wgpu::Device) -> Option<&wgpu::ComputePipeline> {
        self.foam
            .get_or_init(|| {
                let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
                let (layout, module) = (&self.pipeline_layout, &self.module);
                let foam = compute_pipeline(device, layout, module, "water ocean foam", "cs_foam");
                if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
                    tracing::error!(
                        target: "regia_terrain",
                        "water ocean foam pipeline: {err}; foam stays instantaneous"
                    );
                    return None;
                }
                Some(foam)
            })
            .as_ref()
    }
}

/// One compute pipeline of `water_ocean.wgsl`.
fn compute_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    label: &'static str,
    entry: &'static str,
) -> wgpu::ComputePipeline {
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        module,
        entry_point: Some(entry),
        compilation_options: Default::default(),
        cache: regia_render::pipeline_cache::cache_for(device).as_deref(),
    })
}

/// The GPU state of one ocean at one map size.
struct Simulation {
    ocean: FftOcean,
    size: u32,
    tables: Arc<OceanTables>,
    /// Maps: after an even number of stages the result is in `a`.
    a: wgpu::Buffer,
    params: wgpu::Buffer,
    /// Bytes between two entries of `params` (the device's dynamic offset alignment).
    stride: u32,
    /// Reads `a`, writes `b`.
    forward: wgpu::BindGroup,
    /// Reads `b`, writes `a`.
    backward: wgpu::BindGroup,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// The ocean keeps a lingering-foam history (ADR-2843): a tail of `a` and three more layers.
    history: bool,
    /// Each band's normalization of its own compression ([`band_foam_scales`]).
    foam_scales: [f32; OCEAN_BANDS],
}

impl Simulation {
    fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipes: &Pipelines,
        ocean: &FftOcean,
        size: u32,
    ) -> Self {
        Self::with_tables(device, queue, pipes, ocean, size, tables_for(ocean, size))
    }

    fn with_tables(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipes: &Pipelines,
        ocean: &FftOcean,
        size: u32,
        tables: Arc<OceanTables>,
    ) -> Self {
        let _span = tracing::info_span!("water_ocean_upload", size).entered();
        use wgpu::util::DeviceExt;
        let texels = (size * size) as usize;
        let per_band = LAYERS_PER_BAND as usize * texels;
        let mut modes = vec![[0.0f32; 4]; OCEAN_BANDS * per_band];
        for (b, band) in tables.bands.iter().enumerate() {
            for m in &band.modes {
                let at = b * per_band + 2 * m.index as usize;
                modes[at] = [m.a[0], m.a[1], m.b[0], m.b[1]];
                modes[at + 1] = [m.k[0], m.k[1], m.n as f32, m.inv_k];
            }
        }
        let modes = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("water ocean modes"),
            contents: bytemuck::cast_slice(&modes),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let (tc, ts) = tables.twiddles();
        let twiddles: Vec<[f32; 2]> = tc.iter().zip(ts).map(|(c, s)| [*c, *s]).collect();
        let twiddles = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("water ocean twiddles"),
            contents: bytemuck::cast_slice(&twiddles),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let bytes = (OCEAN_BANDS * per_band * 16) as u64;
        let history = has_history(ocean);
        if history {
            // Only an ocean that keeps a history builds the foam pipeline (ADR-2843).
            let _ = pipes.foam(device);
        }
        let map = |label: &'static str, size: u64| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            })
        };
        // The history lives after the maps in `a`, where no FFT stage writes: zeroed at creation.
        let a = map(
            "water ocean map a",
            bytes + if history { history_tail_bytes(size) } else { 0 },
        );
        let b = map("water ocean map b", bytes);
        let stride = device
            .limits()
            .min_uniform_buffer_offset_alignment
            .max(size_of::<StageParams>() as u32);
        let stages = fft_stages(size);
        let entries = OCEAN_BANDS * (1 + stages.len()) + if history { OCEAN_BANDS } else { 0 };
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("water ocean stages"),
            size: entries as u64 * u64::from(stride),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        for band in 0..OCEAN_BANDS {
            let first = (band * (1 + stages.len())) as u64 * u64::from(stride);
            let base = (band * per_band) as u32;
            for (i, (axis, p, step)) in stages.iter().enumerate() {
                let entry = StageParams {
                    size,
                    base,
                    table: *step,
                    tau_or_p: *p,
                    axis: *axis,
                    choppiness: 0.0,
                    extra: [0.0; 2],
                };
                queue.write_buffer(
                    &params,
                    first + (1 + i) as u64 * u64::from(stride),
                    bytemuck::bytes_of(&entry),
                );
            }
        }
        let group = |label: &'static str, src: &wgpu::Buffer, dst: &wgpu::Buffer| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &pipes.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &params,
                            offset: 0,
                            size: wgpu::BufferSize::new(size_of::<StageParams>() as u64),
                        }),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: modes.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: twiddles.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: src.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: dst.as_entire_binding(),
                    },
                ],
            })
        };
        let forward = group("water ocean a to b", &a, &b);
        let backward = group("water ocean b to a", &b, &a);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("water ocean maps"),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: map_layers(history),
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let foam_scales = band_foam_scales(&tables);
        Self {
            ocean: *ocean,
            size,
            tables,
            a,
            params,
            stride,
            forward,
            backward,
            texture,
            view,
            history,
            foam_scales,
        }
    }

    /// Records the spectrum and the FFT at the loop fraction `tau`, the history step with `foam`
    /// (its rates and whether it restarts; ignored without a history), and the copy to the maps.
    /// Returns the dispatches.
    fn record(
        &self,
        queue: &wgpu::Queue,
        pipes: &Pipelines,
        encoder: &mut wgpu::CommandEncoder,
        tau: u32,
        foam: (FoamRates, bool),
    ) -> u32 {
        let dispatches = self.record_compute(queue, pipes, encoder, tau, foam);
        self.record_copy(encoder);
        dispatches
    }

    /// The compute half of [`Self::record`]. Returns the dispatches.
    fn record_compute(
        &self,
        queue: &wgpu::Queue,
        pipes: &Pipelines,
        encoder: &mut wgpu::CommandEncoder,
        tau: u32,
        foam: (FoamRates, bool),
    ) -> u32 {
        let size = self.size;
        let stages = fft_stages(size);
        let per_entry = u64::from(self.stride);
        let per_band = LAYERS_PER_BAND * size * size;
        let table_row = (1 + stages.len()) as u64;
        for band in 0..OCEAN_BANDS as u32 {
            let base = band * per_band;
            let entry = StageParams {
                size,
                base,
                table: base,
                tau_or_p: tau,
                axis: 0,
                choppiness: self.ocean.choppiness,
                extra: [0.0; 2],
            };
            queue.write_buffer(
                &self.params,
                u64::from(band) * table_row * per_entry,
                bytemuck::bytes_of(&entry),
            );
        }
        let foam_row = OCEAN_BANDS as u64 * table_row;
        if self.history {
            let (rates, restart) = foam;
            let tail = OCEAN_BANDS as u32 * per_band;
            for band in 0..OCEAN_BANDS as u32 {
                let entry = StageParams {
                    size,
                    base: band * per_band,
                    table: tail + band * size * size,
                    tau_or_p: u32::from(restart),
                    axis: self.ocean.foam_onset.to_bits(),
                    choppiness: self.foam_scales[band as usize],
                    extra: [rates.growth, rates.decay],
                };
                queue.write_buffer(
                    &self.params,
                    (foam_row + u64::from(band)) * per_entry,
                    bytemuck::bytes_of(&entry),
                );
            }
        }
        let mut dispatches = 0;
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("water ocean fft"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&pipes.spectrum);
            for band in 0..OCEAN_BANDS as u64 {
                pass.set_bind_group(0, &self.backward, &[(band * table_row * per_entry) as u32]);
                pass.dispatch_workgroups(size / WORKGROUP, size / WORKGROUP, 1);
                dispatches += 1;
            }
            pass.set_pipeline(&pipes.fft);
            for band in 0..OCEAN_BANDS as u64 {
                for stage in 0..stages.len() as u64 {
                    let offset = (band * table_row + 1 + stage) * per_entry;
                    let group = if stage.is_multiple_of(2) {
                        &self.forward
                    } else {
                        &self.backward
                    };
                    pass.set_bind_group(0, group, &[offset as u32]);
                    pass.dispatch_workgroups((size / 2) / WORKGROUP, size / WORKGROUP, 1);
                    dispatches += 1;
                }
            }
            // Built by `Simulation::with_tables` for an ocean with a history; missing only when the
            // device refused it.
            if let (true, Some(Some(foam_pipeline))) = (self.history, pipes.foam.get()) {
                // The finished maps and the history are both in `a`, the `backward` group's `dst`.
                pass.set_pipeline(foam_pipeline);
                for band in 0..OCEAN_BANDS as u64 {
                    let offset = (foam_row + band) * per_entry;
                    pass.set_bind_group(0, &self.backward, &[offset as u32]);
                    pass.dispatch_workgroups(size / WORKGROUP, size / WORKGROUP, 1);
                    dispatches += 1;
                }
            }
        }
        dispatches
    }

    /// The copy half of [`Self::record`]: the finished maps into the array the water shader reads.
    fn record_copy(&self, encoder: &mut wgpu::CommandEncoder) {
        let size = self.size;
        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &self.a,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size * 16),
                    rows_per_image: Some(size),
                },
            },
            self.texture.as_image_copy(),
            wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: map_layers(self.history),
            },
        );
    }
}

/// Whether `ocean` keeps a lingering-foam history (ADR-2843).
pub(super) fn has_history(ocean: &FftOcean) -> bool {
    ocean.foam_decay > 0.0
}

/// Layers of the map array: two per band, and one more per band with a history.
fn map_layers(history: bool) -> u32 {
    LAYERS_PER_BAND * OCEAN_BANDS as u32 + if history { OCEAN_BANDS as u32 } else { 0 }
}

/// Bytes of the history tail of the first work buffer at `size` texels per side (a `vec4` per
/// texel, value in `x`, so the copy into the map array is one copy).
fn history_tail_bytes(size: u32) -> u64 {
    OCEAN_BANDS as u64 * u64::from(size) * u64::from(size) * 16
}

/// GPU memory the history adds at `size` texels: its tail of the work buffer and its layers of the
/// map array.
#[must_use]
pub(super) fn history_bytes(size: u32) -> u64 {
    2 * history_tail_bytes(size)
}

/// Bytes of one map buffer, or of the map array, at `size` texels per side.
fn map_bytes(size: u32) -> u64 {
    (OCEAN_BANDS as u64) * u64::from(LAYERS_PER_BAND) * u64::from(size) * u64::from(size) * 16
}

/// GPU memory an ocean holds at `size` texels: the modes, two work buffers, the map array, the
/// twiddles and the stage table.
#[must_use]
pub(super) fn ocean_gpu_bytes(size: u32, stride: u32) -> u64 {
    let entries = OCEAN_BANDS as u64 * (1 + u64::from(2 * size.trailing_zeros()));
    4 * map_bytes(size) + u64::from(size / 2) * 8 + entries * u64::from(stride)
}

/// Why a device cannot run the ocean's compute pass at a grid (ADR-3251): the renderer draws that
/// ocean as swell instead, and the agent reports the error's [`Self::code`]. The size check runs
/// before any buffer is made, so a grid that is too large is a refusal, never a validation
/// error of the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum OceanGridError {
    /// The work buffer (the maps, plus the lingering-foam history after them) is larger than the
    /// device's storage binding: 1024 with a history needs 151 MB, above the 128 MiB default.
    #[error(
        "the {grid} ocean grid (history {history}) needs a {needed}-byte storage binding; this device allows {limit}"
    )]
    StorageBinding {
        /// Texels per side.
        grid: u32,
        /// With the lingering-foam history.
        history: bool,
        /// Bytes the binding needs.
        needed: u64,
        /// The device's `max_storage_buffer_binding_size`.
        limit: u64,
    },
    /// The work buffer is larger than the device's largest buffer.
    #[error(
        "the {grid} ocean grid (history {history}) needs a {needed}-byte buffer; this device allows {limit}"
    )]
    BufferSize {
        /// Texels per side.
        grid: u32,
        /// With the lingering-foam history.
        history: bool,
        /// Bytes the buffer needs.
        needed: u64,
        /// The device's `max_buffer_size`.
        limit: u64,
    },
    /// The map array is wider than the device's 2D textures.
    #[error("the {grid} ocean grid needs {grid}-texel textures; this device allows {limit}")]
    TextureDimension {
        /// Texels per side.
        grid: u32,
        /// The device's `max_texture_dimension_2d`.
        limit: u32,
    },
    /// The FFT stages dispatch more workgroups per dimension than the device allows.
    #[error("the {grid} ocean grid dispatches {needed} workgroups per dimension; this device allows {limit}")]
    Workgroups {
        /// Texels per side.
        grid: u32,
        /// Workgroups per dimension.
        needed: u32,
        /// The device's `max_compute_workgroups_per_dimension`.
        limit: u32,
    },
}

impl OceanGridError {
    /// The stable code (lowercase ASCII) the agent reports.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::StorageBinding { .. } => "storage_binding",
            Self::BufferSize { .. } => "buffer_size",
            Self::TextureDimension { .. } => "texture_dimension",
            Self::Workgroups { .. } => "workgroups",
        }
    }
}

/// Bytes of the ocean's largest binding at `grid` texels: the first work buffer, which carries
/// the lingering-foam history after the maps when `history` (ADR-2843).
#[must_use]
pub fn ocean_binding_bytes(grid: u32, history: bool) -> u64 {
    map_bytes(grid) + if history { history_tail_bytes(grid) } else { 0 }
}

/// Checks that a device with `limits` can run the ocean's compute pass at `grid` texels per side,
/// with or without the lingering-foam history (ADR-3251).
///
/// # Errors
///
/// The first limit the grid exceeds, as an [`OceanGridError`].
pub fn check_ocean_grid(limits: &wgpu::Limits, grid: u32, history: bool) -> Result<(), OceanGridError> {
    let needed = ocean_binding_bytes(grid, history);
    if limits.max_storage_buffer_binding_size < needed {
        return Err(OceanGridError::StorageBinding {
            grid,
            history,
            needed,
            limit: limits.max_storage_buffer_binding_size,
        });
    }
    if limits.max_buffer_size < needed {
        return Err(OceanGridError::BufferSize {
            grid,
            history,
            needed,
            limit: limits.max_buffer_size,
        });
    }
    if limits.max_texture_dimension_2d < grid {
        return Err(OceanGridError::TextureDimension {
            grid,
            limit: limits.max_texture_dimension_2d,
        });
    }
    if limits.max_compute_workgroups_per_dimension < grid / WORKGROUP {
        return Err(OceanGridError::Workgroups {
            grid,
            needed: grid / WORKGROUP,
            limit: limits.max_compute_workgroups_per_dimension,
        });
    }
    Ok(())
}

/// Whether a device with `limits` can run the pass at `size` texels per side.
pub(super) fn grid_fits(limits: &wgpu::Limits, size: u32, history: bool) -> bool {
    check_ocean_grid(limits, size, history).is_ok()
}

/// Whether `grid` is warned about for the first time, marking it in `warned` (one bit per entry of
/// [`OCEAN_GRIDS`], the last bit for any other size). Spec `water-ocean.md` § 3: one warning per
/// grid, also when two bodies with grids the device cannot hold take turns.
fn first_warning(warned: &mut u32, grid: u32) -> bool {
    let bit = 1u32 << OCEAN_GRIDS.iter().position(|g| *g == grid).unwrap_or(31);
    let first = *warned & bit == 0;
    *warned |= bit;
    first
}

/// The middle value of `values` (sorted here): the upper of the two for an even count, 0 for none.
fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

/// Blocks until `buffer`'s mapping is done, then reads it as `T`s and unmaps it.
fn read_mapped<T: Pod>(device: &wgpu::Device, buffer: &wgpu::Buffer) -> Result<Vec<T>, RenderError> {
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(RenderError::Poll)?;
    rx.recv()
        .map_err(|_| RenderError::Readback("map callback dropped".into()))?
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    let mapped = buffer
        .slice(..)
        .get_mapped_range()
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    let values = bytemuck::pod_collect_to_vec::<u8, T>(&mapped);
    drop(mapped);
    buffer.unmap();
    Ok(values)
}

/// What the frame's ocean selection found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct OceanSelection {
    /// Body slots that ride the frame's ocean.
    pub slots: [bool; MAX_BODIES],
    /// Ocean bodies drawn on the frame's ocean.
    pub bodies: u32,
    /// Ocean bodies with another spectrum, left to their swell.
    pub dropped: u32,
    /// Why the first ocean asked for could not run on this device, if it could not (ADR-3251).
    pub refused: Option<OceanGridError>,
}

/// The ocean state behind `WaterGpu`.
pub(super) struct OceanGpu {
    /// Binding 12.
    pub uniform: wgpu::Buffer,
    stand_in: wgpu::TextureView,
    pipes: Option<Pipelines>,
    /// The device has no compute limits for this, or the pipelines failed.
    refused: bool,
    sim: Option<Simulation>,
    /// The loop fraction the maps hold.
    evolved: Option<u32>,
    /// Grids already warned about as too large for this device ([`first_warning`]: once per grid).
    warned_grids: u32,
    /// Bumped whenever the bound view changes (the bind group is rebuilt).
    pub generation: u64,
    /// Dispatches of the last evolve.
    pub dispatches: u32,
    /// Evolves since the renderer was created.
    pub evolves: u64,
    /// The frame's selection.
    pub selection: OceanSelection,
    /// The clock of the history's last step (s); `None` restarts it at the next (ADR-2843).
    history_seconds: Option<f64>,
    /// History steps since the renderer was created.
    pub history_steps: u64,
    /// History steps that restarted it (a reset, a new simulation, a step back or a jump).
    pub history_restarts: u64,
}

impl OceanGpu {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("water ocean"),
            size: (16 * OCEAN_VEC4S) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&uniform, 0, bytemuck::cast_slice(&[[0.0f32; 4]; OCEAN_VEC4S]));
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("water ocean stand-in"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            texture.as_image_copy(),
            &[0u8; 16],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(16),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let stand_in = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        Self {
            uniform,
            stand_in,
            pipes: None,
            refused: false,
            sim: None,
            evolved: None,
            warned_grids: 0,
            generation: 0,
            dispatches: 0,
            evolves: 0,
            selection: OceanSelection::default(),
            history_seconds: None,
            history_steps: 0,
            history_restarts: 0,
        }
    }

    /// GPU memory of the bound lingering-foam history (0 without one).
    pub fn history_bytes(&self) -> u64 {
        self.sim
            .as_ref()
            .filter(|s| s.history)
            .map_or(0, |s| history_bytes(s.size))
    }

    /// Forgets the lingering foam: the next evolve restarts the history from no foam (a camera
    /// cut, a change of the bodies).
    pub fn reset_history(&mut self) {
        self.history_seconds = None;
    }

    /// The array the water shader binds.
    pub fn view(&self) -> &wgpu::TextureView {
        self.sim.as_ref().map_or(&self.stand_in, |s| &s.view)
    }

    /// Texels per side of the bound maps (0 with none).
    pub fn size(&self) -> u32 {
        self.sim.as_ref().map_or(0, |s| s.size)
    }

    /// Picks the frame's ocean and sets its simulation up. `tier_size` is the tier's map size (0:
    /// none); the ocean's own [`FftOcean::grid`] goes first where the device can run it.
    pub fn select(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bodies: &[WaterBody],
        slots: &[Option<u32>],
        tier_size: u32,
    ) {
        let mut selection = OceanSelection::default();
        let mut chosen: Option<FftOcean> = None;
        if tier_size > 0 && self.ready(device) {
            let limits = device.limits();
            for (body, slot) in bodies.iter().zip(slots) {
                let (Some(slot), Some(ocean)) = (slot, body.active_ocean()) else {
                    continue;
                };
                if chosen.is_none()
                    && let Err(error) =
                        check_ocean_grid(&limits, ocean.size_on(tier_size), has_history(ocean))
                {
                    let grid = ocean.size_on(tier_size);
                    if first_warning(&mut self.warned_grids, grid) {
                        tracing::warn!(target: "regia_terrain", grid, %error, "water ocean: this device has no limits for the grid; the ocean draws as swell");
                    }
                    selection.refused.get_or_insert(error);
                    selection.dropped += 1;
                    continue;
                }
                if *chosen.get_or_insert(*ocean) == *ocean {
                    selection.slots[(*slot as usize).min(MAX_BODIES - 1)] = true;
                    selection.bodies += 1;
                } else {
                    selection.dropped += 1;
                }
            }
        } else {
            selection.dropped = bodies.iter().filter(|b| b.active_ocean().is_some()).count() as u32;
        }
        self.selection = selection;
        let chosen = chosen.map(|ocean| (ocean, ocean.size_on(tier_size)));
        let current = self.sim.as_ref().map(|s| (s.ocean, s.size));
        if current == chosen {
            return;
        }
        self.evolved = None;
        self.history_seconds = None;
        self.generation = self.generation.wrapping_add(1);
        self.sim = match (chosen, &self.pipes) {
            (Some((ocean, size)), Some(pipes)) => Some(Simulation::new(device, queue, pipes, &ocean, size)),
            _ => None,
        };
    }

    /// `true` when the pipelines exist, building them on the first ask; a device that cannot
    /// run them is refused once and for all (logged), and its oceans draw as swell.
    fn ready(&mut self, device: &wgpu::Device) -> bool {
        if self.pipes.is_some() {
            return true;
        }
        if self.refused {
            return false;
        }
        let limits = device.limits();
        // The tiers' grids must fit; a larger `FftOcean::grid` is checked by `grid_fits` when asked.
        let fits = limits.max_storage_buffers_per_shader_stage >= 4
            && limits.max_compute_invocations_per_workgroup >= WORKGROUP * WORKGROUP
            && limits.max_compute_workgroup_size_x >= WORKGROUP
            && limits.max_compute_workgroup_size_y >= WORKGROUP
            && grid_fits(&limits, super::ocean::OCEAN_MAX_SIZE, false);
        if fits {
            self.pipes = Pipelines::new(device);
        }
        self.refused = self.pipes.is_none();
        if self.refused {
            tracing::warn!(target: "regia_terrain", "water ocean: no compute path on this device; oceans draw as swell");
        }
        self.pipes.is_some()
    }

    /// Records the evolution to `seconds` on `encoder`, unless the maps already hold that instant.
    /// Returns whether it recorded.
    pub fn evolve(&mut self, queue: &wgpu::Queue, encoder: &mut wgpu::CommandEncoder, seconds: f64) -> bool {
        let tau = loop_fraction(seconds);
        if self.evolved == Some(tau) {
            return false;
        }
        let (Some(sim), Some(pipes)) = (&self.sim, &self.pipes) else {
            return false;
        };
        let _span = tracing::info_span!("water_ocean_fft").entered();
        let foam = if sim.history {
            let dt = foam_interval(self.history_seconds, seconds);
            self.history_seconds = Some(seconds);
            self.history_steps += 1;
            self.history_restarts += u64::from(dt.is_none());
            (FoamRates::of(&sim.ocean, dt.unwrap_or(0.0)), dt.is_none())
        } else {
            (FoamRates::of(&sim.ocean, 0.0), true)
        };
        self.dispatches = sim.record(queue, pipes, encoder, tau, foam);
        self.evolved = Some(tau);
        self.evolves += 1;
        true
    }

    /// Writes binding 12 for a ring grid centered on `origin` (world x and z).
    pub fn write_uniform(&self, queue: &wgpu::Queue, origin: [f64; 2]) {
        let packed = self
            .sim
            .as_ref()
            .map_or([[0.0f32; 4]; OCEAN_VEC4S], |s| ocean_uniform(&s.tables, origin));
        queue.write_buffer(&self.uniform, 0, bytemuck::cast_slice(&packed));
    }

    /// Times `frames` evolves of `ocean` at `size` texels on a simulation of its own, so the maps
    /// the frame binds are not disturbed. `None` when `size` is not one of [`OCEAN_GRIDS`] or the
    /// device cannot run it. Blocks until the GPU is done, for every frame.
    pub fn measure(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        ocean: &FftOcean,
        size: u32,
        frames: u32,
    ) -> Result<Option<OceanCost>, RenderError> {
        if !OCEAN_GRIDS.contains(&size)
            || !self.ready(device)
            || !grid_fits(&device.limits(), size, has_history(ocean))
        {
            return Ok(None);
        }
        let Some(pipes) = &self.pipes else {
            return Ok(None);
        };
        let _span = tracing::info_span!("water_ocean_measure", size, frames).entered();
        let started = Instant::now();
        let tables = Arc::new(OceanTables::new(ocean, size));
        let tables_ms = started.elapsed().as_secs_f64() * 1000.0;
        let modes = tables.bands.iter().map(|b| b.modes.len() as u64).sum();
        let started = Instant::now();
        let sim = Simulation::with_tables(device, queue, pipes, ocean, size, tables);
        // The upload is queued: finish it, so the first timed evolve does not pay for it.
        queue.submit([]);
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(RenderError::Poll)?;
        let upload_ms = started.elapsed().as_secs_f64() * 1000.0;
        let stamped = device
            .features()
            .contains(wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS);
        let stamps = stamped.then(|| {
            let set = device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("water ocean cost"),
                ty: wgpu::QueryType::Timestamp,
                count: 3,
            });
            let buffer = |label: &'static str, usage: wgpu::BufferUsages| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(label),
                    size: 24,
                    usage,
                    mapped_at_creation: false,
                })
            };
            let resolve = buffer(
                "water ocean cost resolve",
                wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            );
            let staging = buffer(
                "water ocean cost staging",
                wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            );
            (set, resolve, staging)
        });
        let period = f64::from(queue.get_timestamp_period());
        let frames = frames.max(1);
        let mut dispatches = 0;
        let (mut compute, mut copy, mut submit) = (Vec::new(), Vec::new(), Vec::new());
        for frame in 0..frames {
            // Steps of a sixtieth of a second, so every evolve computes a new instant.
            let tau = loop_fraction(f64::from(frame) / 60.0);
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("water ocean cost"),
            });
            if let Some((set, _, _)) = &stamps {
                encoder.write_timestamp(set, 0);
            }
            // Steps of a sixtieth of a second for the history too, the first one a restart.
            let foam = (
                FoamRates::of(ocean, if frame == 0 { 0.0 } else { 1.0 / 60.0 }),
                frame == 0,
            );
            dispatches = sim.record_compute(queue, pipes, &mut encoder, tau, foam);
            if let Some((set, _, _)) = &stamps {
                encoder.write_timestamp(set, 1);
            }
            sim.record_copy(&mut encoder);
            if let Some((set, resolve, staging)) = &stamps {
                encoder.write_timestamp(set, 2);
                encoder.resolve_query_set(set, 0..3, resolve, 0);
                encoder.copy_buffer_to_buffer(resolve, 0, staging, 0, 24);
            }
            let started = Instant::now();
            queue.submit([encoder.finish()]);
            device
                .poll(wgpu::PollType::wait_indefinitely())
                .map_err(RenderError::Poll)?;
            submit.push(started.elapsed().as_secs_f64() * 1000.0);
            if let Some((_, _, staging)) = &stamps
                && let [a, b, c] = read_mapped::<u64>(device, staging)?[..]
            {
                let ms = |from: u64, to: u64| to.saturating_sub(from) as f64 * period / 1.0e6;
                compute.push(ms(a, b));
                copy.push(ms(b, c));
            }
        }
        let history = if sim.history { history_bytes(size) } else { 0 };
        Ok(Some(OceanCost {
            size,
            frames,
            dispatches,
            modes,
            gpu_bytes: ocean_gpu_bytes(size, sim.stride) + history,
            compute_ms: stamped.then(|| median(&mut compute)),
            copy_ms: stamped.then(|| median(&mut copy)),
            submit_ms: median(&mut submit),
            tables_ms,
            upload_ms,
            history_bytes: history,
            history_dispatches: if sim.history { OCEAN_BANDS as u32 } else { 0 },
        }))
    }

    /// Reads the maps back; blocks until the GPU is done.
    pub fn read_maps(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<Option<OceanMaps>, RenderError> {
        let Some(sim) = &self.sim else { return Ok(None) };
        let size = sim.size;
        let layers = map_layers(sim.history);
        let bytes = u64::from(size * size * layers) * 16;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("water ocean readback"),
            size: bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("water ocean readback"),
        });
        encoder.copy_texture_to_buffer(
            sim.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size * 16),
                    rows_per_image: Some(size),
                },
            },
            wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: layers,
            },
        );
        queue.submit([encoder.finish()]);
        let texels = read_mapped::<[f32; 4]>(device, &readback)?;
        Ok(Some(OceanMaps {
            ocean: sim.ocean,
            size,
            texels,
        }))
    }
}

/// Binding 12 for `tables` on a ring grid centered on `origin`: the origin as tile fractions,
/// reduced in `f64`. Pure, so tests check it without a GPU.
pub(super) fn ocean_uniform(tables: &OceanTables, origin: [f64; 2]) -> [[f32; 4]; OCEAN_VEC4S] {
    let ocean = &tables.ocean;
    let mut packed = [[0.0f32; 4]; OCEAN_VEC4S];
    // Header w: the map array holds history layers; shore z: rescale the whitecaps (ADR-2843).
    let history = if has_history(ocean) { 1.0 } else { 0.0 };
    let rescale = if ocean.whitecap_rescale { 1.0 } else { 0.0 };
    packed[0] = [OCEAN_BANDS as f32, ocean.crest_foam, ocean.foam_onset, history];
    packed[1] = [ocean.shore_start, ocean.shore_end, rescale, 0.0];
    for (b, band) in tables.bands.iter().enumerate() {
        packed[2 + 2 * b] = [
            (origin[0] / band.tile).rem_euclid(1.0) as f32,
            (origin[1] / band.tile).rem_euclid(1.0) as f32,
            (1.0 / band.tile) as f32,
            tables.size as f32,
        ];
        packed[3 + 2 * b] = [band.rms_k as f32, band.slope_variance as f32, 0.0, 0.0];
    }
    packed
}

impl WaterTier {
    /// Texels per side of the ocean maps the tier draws; 0 draws none (the swell alone).
    #[must_use]
    pub fn ocean_size(self) -> u32 {
        match self {
            Self::Low => 0,
            Self::High => super::ocean::OCEAN_QUERY_SIZE,
            Self::Ultra => super::ocean::OCEAN_MAX_SIZE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ocean::{OCEAN_LOOP_SECONDS, OCEAN_MAX_SIZE, OCEAN_QUERY_SIZE, PHASE_SCALE};
    use super::*;

    /// The stage list, its entry layout and the workgroup size agree with the shader.
    #[test]
    fn shader_mirrors_the_rust_side() {
        assert_eq!(size_of::<StageParams>(), 32);
        assert!(OCEAN_SHADER.contains("@compute @workgroup_size(16, 16)"));
        assert_eq!(WORKGROUP, 16);
        // Every grid divides into whole workgroups, and each copy row is 256-aligned.
        assert_eq!(OCEAN_GRIDS[0], OCEAN_QUERY_SIZE);
        for size in OCEAN_GRIDS {
            assert!(size.is_multiple_of(2 * WORKGROUP));
            assert!((size * 16).is_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT));
            assert_eq!(fft_stages(size).len() as u32, 2 * size.trailing_zeros());
            // Even, so the result lands in the buffer the spectrum wrote.
            assert!(fft_stages(size).len().is_multiple_of(2));
        }
        let line = OCEAN_SHADER
            .lines()
            .find(|l| l.starts_with("const PHASE_SCALE: f32 = "))
            .expect("const");
        let text = line.trim_end_matches(';').rsplit(' ').next().unwrap_or("");
        assert_eq!(text.parse::<f32>().ok(), Some(PHASE_SCALE));
        assert_eq!(PHASE_SCALE, (std::f64::consts::TAU / 4_294_967_296.0_f64) as f32);
        const { assert!(OCEAN_LOOP_SECONDS > 0.0) };
    }

    /// The default limits of a desktop device hold every grid; a device with less gives up the
    /// large grids first, by each limit alone.
    #[test]
    fn grids_fit_by_the_device_limits() {
        let good = wgpu::Limits::default();
        for size in OCEAN_GRIDS {
            assert!(grid_fits(&good, size, false), "{size}");
        }
        assert_eq!(map_bytes(1024), 100_663_296);
        let small_binding = wgpu::Limits {
            max_storage_buffer_binding_size: map_bytes(512),
            ..good.clone()
        };
        assert!(grid_fits(&small_binding, 512, false));
        assert!(!grid_fits(&small_binding, 1024, false));
        let small_buffer = wgpu::Limits {
            max_buffer_size: map_bytes(256),
            ..good.clone()
        };
        assert!(grid_fits(&small_buffer, 256, false));
        assert!(!grid_fits(&small_buffer, 512, false));
        let small_texture = wgpu::Limits {
            max_texture_dimension_2d: 512,
            ..good.clone()
        };
        assert!(grid_fits(&small_texture, 512, false));
        assert!(!grid_fits(&small_texture, 1024, false));
        let few_groups = wgpu::Limits {
            max_compute_workgroups_per_dimension: 32,
            ..good
        };
        assert!(grid_fits(&few_groups, 512, false));
        assert!(!grid_fits(&few_groups, 1024, false));
        // Lingering foam makes the first work buffer half again as large (ADR-2843): at 1024 that
        // is past the default storage binding size, so a device with only the defaults draws that
        // ocean as swell.
        assert!(grid_fits(&wgpu::Limits::default(), 512, true));
        assert!(!grid_fits(&wgpu::Limits::default(), 1024, true));
    }

    #[test]
    fn memory_grows_with_the_grid() {
        let grown = ocean_gpu_bytes(1024, 256) - ocean_gpu_bytes(512, 256);
        assert_eq!(grown, 4 * 3 * map_bytes(512) + 256 * 8 + 3 * 2 * 256);
    }

    /// One warning per grid (spec § 3): two oversized grids that take turns frame after frame warn
    /// once each. The earlier bookkeeping kept only the last grid, so it warned on every frame.
    #[test]
    fn each_unfit_grid_warns_once() {
        let mut warned = 0;
        let calls: Vec<bool> = [1024, 512, 1024, 512, 1024, 512]
            .into_iter()
            .map(|g| first_warning(&mut warned, g))
            .collect();
        assert_eq!(calls, [true, true, false, false, false, false]);
        assert!(first_warning(&mut warned, 256));
        assert!(!first_warning(&mut warned, 256));
        // A size outside the list still warns once.
        assert!(first_warning(&mut warned, 2048));
        assert!(!first_warning(&mut warned, 2048));
        assert!(first_warning(&mut warned, 128));
    }

    #[test]
    fn median_takes_the_middle_value() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&mut [4.0, 1.0, 2.0, 3.0]), 3.0);
        assert_eq!(median(&mut []), 0.0);
    }

    #[test]
    fn stages_double_the_sorted_run() {
        assert_eq!(
            fft_stages(16),
            vec![
                (0, 1, 8),
                (0, 2, 4),
                (0, 4, 2),
                (0, 8, 1),
                (1, 1, 8),
                (1, 2, 4),
                (1, 4, 2),
                (1, 8, 1),
            ]
        );
    }

    /// ADR-026 and the DX12 checks hold for the compute shader.
    #[test]
    fn shader_passes_the_engine_shader_rules() {
        let found = regia_render::wgsl_function_layout_errors("water_ocean.wgsl", OCEAN_SHADER)
            .expect("water_ocean.wgsl parses");
        assert!(found.is_empty(), "{found:?}");
        let dx12 = regia_render::shader_hlsl::wgsl_dx12_problems(&[("water_ocean.wgsl", OCEAN_SHADER)]);
        assert!(dx12.is_empty(), "{}", dx12.join("\n"));
    }

    /// The uniform holds the origin as a tile fraction, far from the world origin too.
    #[test]
    fn uniform_reduces_the_origin() {
        let tables = OceanTables::new(&FftOcean::default(), 32);
        let far = 1.0e7f64;
        let packed = ocean_uniform(&tables, [far, -far]);
        assert_eq!(packed[0][0], 3.0);
        for (b, band) in tables.bands.iter().enumerate() {
            let [u, v, inverse, size] = packed[2 + 2 * b];
            assert!((0.0..1.0).contains(&u) && (0.0..1.0).contains(&v));
            assert!((f64::from(inverse) * band.tile - 1.0).abs() < 1.0e-6);
            assert_eq!(size, 32.0);
            assert_eq!(packed[3 + 2 * b][0], band.rms_k as f32);
        }
    }

    /// The counted columns of `docs/specs/water-ocean.md` § 8b for the sea `water_ocean_cost`
    /// measures (14 m/s, wind `[-1, -0.25]`, seed 11): dispatches `3 (1 + 2 log2 N)`, the modes
    /// and the GPU memory, which do not depend on the card. A change that moves them must update
    /// the table.
    #[test]
    fn cost_table_counts_follow_the_spec() {
        let sea = FftOcean {
            wind_speed: 14.0,
            wind_direction: [-1.0, -0.25],
            seed: 11,
            ..FftOcean::default()
        };
        let want = [
            (128u32, 45u32, 22_104u64, 6u64),
            (256, 51, 71_000, 25),
            (512, 57, 267_096, 101),
            (1024, 63, 1_052_504, 403),
        ];
        for (size, dispatches, modes, megabytes) in want {
            assert_eq!(
                OCEAN_BANDS as u32 * (1 + fft_stages(size).len() as u32),
                dispatches,
                "{size}"
            );
            let tables = OceanTables::new(&FftOcean { grid: size, ..sea }, size);
            let got: u64 = tables.bands.iter().map(|b| b.modes.len() as u64).sum();
            assert_eq!(got, modes, "{size}");
            // Four times `3 × 2 × N² × 16` bytes plus the small tables, in decimal megabytes.
            assert_eq!(map_bytes(size), 3 * 2 * u64::from(size * size) * 16);
            let bytes = ocean_gpu_bytes(size, 256);
            assert_eq!(
                (bytes as f64 / 1.0e6).round() as u64,
                megabytes,
                "{size}: {bytes}"
            );
        }
    }

    /// The lingering-foam stage and the fragment shader's rescale use the CPU's constants, and the
    /// history adds three layers and a tail of the first work buffer only when it is on (ADR-2843).
    #[test]
    fn foam_history_mirrors_the_rust_side() {
        use super::super::ocean_foam::WHITECAP_RESCALE_MAX;
        use super::super::swell::FOAM_RAMP;
        assert!(OCEAN_SHADER.contains(&format!("const FOAM_RAMP: f32 = {FOAM_RAMP};")));
        assert!(OCEAN_SHADER.contains("fn cs_foam("));
        assert!(super::super::gpu::SHADER.contains(&format!(
            "const WHITECAP_RESCALE_MAX: f32 = {WHITECAP_RESCALE_MAX:.1};"
        )));
        assert_eq!(map_layers(false), 6);
        assert_eq!(map_layers(true), 9);
        assert_eq!(history_bytes(128), 2 * 3 * 128 * 128 * 16);
        let off = FftOcean::default();
        assert!(!has_history(&off));
        assert!(has_history(&FftOcean {
            foam_decay: 1.0,
            ..off
        }));
        // The uniform says so only when on: a default ocean writes what it wrote before.
        let tables = OceanTables::new(&off, 32);
        let packed = ocean_uniform(&tables, [0.0, 0.0]);
        assert_eq!(packed[0][3], 0.0);
        assert_eq!(packed[1][2], 0.0);
        let on = FftOcean {
            foam_decay: 3.0,
            whitecap_rescale: true,
            ..off
        };
        let packed = ocean_uniform(&OceanTables::new(&on, 32), [0.0, 0.0]);
        assert_eq!(packed[0][3], 1.0);
        assert_eq!(packed[1][2], 1.0);
    }

    #[test]
    fn tiers_set_the_map_size() {
        assert_eq!(WaterTier::Low.ocean_size(), 0);
        assert_eq!(WaterTier::High.ocean_size(), OCEAN_QUERY_SIZE);
        assert_eq!(WaterTier::Ultra.ocean_size(), OCEAN_MAX_SIZE);
    }
}
