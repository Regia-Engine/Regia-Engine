// SPDX-License-Identifier: MPL-2.0
//! The baked GI lookup run on the GPU at given receivers ([`gpu_sample_static_gi`],
//! `shaders/baked_gi_points.wgsl`): the shader's own `static_gi`, read back. It holds the shader
//! to its CPU mirror ([`super::sample_static_gi`]) in tests, and lets an agent compare the two on
//! the installed volume (`render.sample_baked_gi` with `"gpu": true`). Not for per-frame use: each
//! call uploads the volume, compiles the kernel and waits for the GPU.

use crate::frame_work::counted::*;
use regia_asset::baked_gi::{BakedGiVolume, GI_COMPONENTS, ShRgb};

use super::{
    BakedGiSettings, BakedGiUniformGpu, GiEval, LOOKUP, StaticGiSample, SunRelight, VolumeWords,
    storage_entry, uniform_buffer, uniform_entry, write_volume,
};

const POINTS: &str = include_str!("../../shaders/baked_gi_points.wgsl");

/// Most receivers one [`gpu_sample_static_gi`] call takes.
pub const MAX_GI_POINTS: usize = 16_384;

/// Result `vec4`s per receiver and dispatch (`baked_gi_points.wgsl`).
const RESULT_VEC4S: usize = 4;

/// Dispatches per call: all components together, then each component alone.
const PASSES: usize = 1 + GI_COMPONENTS;

/// A receiver for [`gpu_sample_static_gi`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StaticGiQuery {
    /// World position.
    pub p: [f32; 3],
    /// Unit normal.
    pub n: [f32; 3],
    /// Unit vector toward the viewer.
    pub v: [f32; 3],
}

/// Why [`gpu_sample_static_gi`] returned no samples.
#[derive(Debug, thiserror::Error)]
pub enum GiPointsError {
    /// More receivers than [`MAX_GI_POINTS`].
    #[error("{given} receivers; at most {max} per call")]
    TooManyPoints {
        /// Receivers asked for.
        given: usize,
        /// [`MAX_GI_POINTS`].
        max: usize,
    },
    /// The volume does not fit one storage binding on this device.
    #[error("baked GI volume needs {bytes} bytes in one storage binding; this device allows {allowed}")]
    VolumeTooLarge {
        /// Bytes the volume buffer needs.
        bytes: u64,
        /// The device's limit.
        allowed: u64,
    },
    /// Mapping a buffer or waiting for the GPU failed.
    #[error(transparent)]
    Gpu(#[from] crate::RenderError),
}

/// `BgiPoint` in `baked_gi_points.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct PointGpu {
    p: [f32; 4],
    n: [f32; 4],
    v: [f32; 4],
}

/// The points kernel's full source.
#[must_use]
pub(crate) fn points_source() -> String {
    format!("{LOOKUP}\n{POINTS}")
}

/// Runs the shader's lookup (`baked_gi.wgsl` `static_gi`) at each receiver on `device` and reads
/// the results back, in the shape of [`super::sample_static_gi`] with the same `volume`,
/// `settings` and component factors `scales` ([`super::component_scales`]): irradiance / π for
/// the receiver's normal, each component's (one more dispatch per component), coverage and
/// visibility. Blocks until the GPU is done. An empty `queries` returns an empty list without
/// touching the GPU.
///
/// # Errors
///
/// [`GiPointsError::TooManyPoints`] past [`MAX_GI_POINTS`], [`GiPointsError::VolumeTooLarge`]
/// when the volume exceeds the device's storage binding, [`GiPointsError::Gpu`] when the read
/// back fails.
pub fn gpu_sample_static_gi(
    device: &crate::RenderDevice,
    volume: &BakedGiVolume,
    settings: &BakedGiSettings,
    scales: &[[f32; 3]; GI_COMPONENTS],
    queries: &[StaticGiQuery],
) -> Result<Vec<StaticGiSample>, GiPointsError> {
    if queries.len() > MAX_GI_POINTS {
        return Err(GiPointsError::TooManyPoints {
            given: queries.len(),
            max: MAX_GI_POINTS,
        });
    }
    if queries.is_empty() {
        return Ok(Vec::new());
    }
    let _span = tracing::info_span!("baked_gi points", points = queries.len()).entered();
    let d = &device.device;
    let words = VolumeWords::of(volume);
    let bytes = (words.end as u64 * 4).max(16);
    let limits = d.limits();
    let allowed = limits.max_storage_buffer_binding_size.min(limits.max_buffer_size);
    if bytes > allowed {
        return Err(GiPointsError::VolumeTooLarge { bytes, allowed });
    }
    let volume_buffer = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("baked gi points volume"),
        size: bytes,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut volume_bytes = vec![0u8; bytes as usize];
    write_volume(volume, &words, |at, b| {
        volume_bytes[at..at + b.len()].copy_from_slice(b);
    });
    device.queue.write_buffer(&volume_buffer, 0, &volume_bytes);

    let points: Vec<PointGpu> = queries
        .iter()
        .map(|q| PointGpu {
            p: [q.p[0], q.p[1], q.p[2], 0.0],
            n: [q.n[0], q.n[1], q.n[2], 0.0],
            v: [q.v[0], q.v[1], q.v[2], 0.0],
        })
        .collect();
    let point_buffer = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("baked gi points"),
        size: std::mem::size_of_val(points.as_slice()) as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    device
        .queue
        .write_buffer(&point_buffer, 0, bytemuck::cast_slice(&points));
    let per_pass = queries.len() * RESULT_VEC4S;
    let result_bytes = (PASSES * per_pass * 16) as u64;
    let results = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("baked gi point results"),
        size: result_bytes,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let staging = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("baked gi point readback"),
        size: result_bytes,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("baked gi points bgl"),
        entries: &[
            uniform_entry(0),
            storage_entry(1, true),
            storage_entry(2, true),
            storage_entry(3, false),
            uniform_entry(4),
        ],
    });
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("baked gi points"),
        source: wgpu::ShaderSource::Wgsl(points_source().into()),
    });
    let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("baked gi points"),
        bind_group_layouts: &[Some(&bgl)],
        immediate_size: 0,
    });
    let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("baked gi point lookup"),
        layout: Some(&layout),
        module: &module,
        entry_point: Some("baked_point_lookup"),
        compilation_options: Default::default(),
        cache: None,
    });

    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("baked gi points"),
    });
    for pass_index in 0..PASSES {
        // Pass 0: every component; pass 1 + c: component c alone (the corner weights do not
        // depend on the factors, so each pass normalises the same way).
        let pass_scales: [[f32; 3]; GI_COMPONENTS] = if pass_index == 0 {
            *scales
        } else {
            std::array::from_fn(|c| if c + 1 == pass_index { scales[c] } else { [0.0; 3] })
        };
        let uniform = BakedGiUniformGpu::of(volume, &words, settings, &pass_scales, &SunRelight::NONE);
        let bgi_uniform = uniform_buffer(
            d,
            "baked gi points uniform",
            std::mem::size_of::<BakedGiUniformGpu>() as u64,
        );
        device
            .queue
            .write_buffer(&bgi_uniform, 0, bytemuck::bytes_of(&uniform));
        let range = [queries.len() as u32, (pass_index * per_pass) as u32, 0, 0];
        let range_uniform = uniform_buffer(d, "baked gi points range", 16);
        device
            .queue
            .write_buffer(&range_uniform, 0, bytemuck::cast_slice(&range));
        let bind_group = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("baked gi points"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: bgi_uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: volume_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: point_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: results.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: range_uniform.as_entire_binding(),
                },
            ],
        });
        let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("baked gi point lookup"),
            timestamp_writes: None,
        });
        pass.counted_set_pipeline(&pipeline);
        pass.counted_set_bind_group(0, &bind_group, &[]);
        pass.counted_dispatch((queries.len() as u32).div_ceil(64), 1, 1);
    }
    encoder.copy_buffer_to_buffer(&results, 0, &staging, 0, result_bytes);
    device.queue.submit([encoder.finish()]);

    let (tx, rx) = std::sync::mpsc::channel();
    staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .tracked()
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(crate::RenderError::Poll)?;
    rx.recv()
        .map_err(|_| crate::RenderError::Readback("map callback dropped".into()))?
        .map_err(|e| crate::RenderError::Readback(e.to_string()))?;
    let floats: Vec<f32> = bytemuck::pod_collect_to_vec(
        &staging
            .slice(..)
            .get_mapped_range()
            .map_err(|e| crate::RenderError::Readback(e.to_string()))?,
    );
    staging.unmap();
    Ok(samples_from_results(&floats, queries, GiEval::of(settings)))
}

/// The brick's and the terrain layer's fractions of receiver `i`'s light (pass 0, the spare
/// lanes of its last two vec4; ADR-1104).
fn layer_fractions(floats: &[f32], i: usize) -> (f32, f32) {
    let at = i * RESULT_VEC4S * 4;
    match floats.get(at..at + RESULT_VEC4S * 4) {
        Some(r) => (r[11], r[15]),
        None => (0.0, 0.0),
    }
}

/// The SH (per channel L00, L1 x, y, z) and the coverage and visibility of receiver `i` in
/// pass `pass_index` of the read-back floats.
fn decode(floats: &[f32], per_pass: usize, pass_index: usize, i: usize) -> (ShRgb, f32, f32) {
    let at = (pass_index * per_pass + i * RESULT_VEC4S) * 4;
    let Some(r) = floats.get(at..at + RESULT_VEC4S * 4) else {
        return ([[0.0; 4]; 3], 0.0, 0.0);
    };
    let sh = [
        [r[0], r[4], r[5], r[6]],
        [r[1], r[8], r[9], r[10]],
        [r[2], r[12], r[13], r[14]],
    ];
    (sh, r[3], r[7])
}

/// The samples of every receiver from the read-back floats ([`PASSES`] passes), the SH evaluated
/// as `eval` says (`BakedGiSettings::zh3`, `BakedGiSettings::deringing`).
fn samples_from_results(floats: &[f32], queries: &[StaticGiQuery], eval: GiEval) -> Vec<StaticGiSample> {
    let per_pass = queries.len() * RESULT_VEC4S;
    queries
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let (sh, coverage, visibility) = decode(floats, per_pass, 0, i);
            let (brick, terrain) = layer_fractions(floats, i);
            StaticGiSample {
                irradiance_over_pi: eval.irradiance(&sh, q.n),
                components: std::array::from_fn(|c| {
                    eval.irradiance(&decode(floats, per_pass, 1 + c, i).0, q.n)
                }),
                coverage,
                visibility,
                brick_share: coverage * brick,
                terrain_share: coverage * terrain,
                // The cell sizes are not read back (`brick_cell`, `terrain_cell` stay 0).
                ..StaticGiSample::default()
            }
        })
        .collect()
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "baked_gi_points.wgsl",
        "BgiPoint",
        PointGpu { p, n, v }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_points_shader_parses_and_validates() {
        let src = points_source();
        let module =
            naga::front::wgsl::parse_str(&src).unwrap_or_else(|e| panic!("{}", e.emit_to_string(&src)));
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .unwrap_or_else(|e| panic!("{e:?}"));
    }

    #[test]
    fn results_decode_per_pass_and_receiver() {
        let queries = [
            StaticGiQuery {
                n: [0.0, 1.0, 0.0],
                ..StaticGiQuery::default()
            },
            StaticGiQuery {
                n: [1.0, 0.0, 0.0],
                ..StaticGiQuery::default()
            },
        ];
        let per_pass = queries.len() * RESULT_VEC4S;
        let mut floats = vec![0.0f32; PASSES * per_pass * 4];
        // Receiver 1, pass 0: L00 (2, 4, 6) with a red L1 along +x, coverage 0.5, visibility 0.25.
        let at = RESULT_VEC4S * 4;
        floats[at..at + 8].copy_from_slice(&[2.0, 4.0, 6.0, 0.5, 1.0, 0.0, 0.0, 0.25]);
        // Receiver 1, pass 2 (the sky alone): L00 (1, 1, 1).
        let at = 2 * per_pass * 4 + RESULT_VEC4S * 4;
        floats[at..at + 3].copy_from_slice(&[1.0, 1.0, 1.0]);
        // The ZH3 evaluation: red's L1 is weaker than a hemisphere's, so it reads L1.
        let zh3 = GiEval::of(&BakedGiSettings {
            zh3: true,
            ..BakedGiSettings::default()
        });
        let samples = samples_from_results(&floats, &queries, zh3);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0], StaticGiSample::default());
        let s = samples[1];
        assert_eq!((s.coverage, s.visibility), (0.5, 0.25));
        let y00 = regia_asset::baked_gi::SH_Y00;
        let a1 = regia_asset::baked_gi::SH_Y1 * (2.0 / 3.0);
        // Red sees its L1 along the +x normal; green and blue only their L00.
        assert!((s.irradiance_over_pi[0] - (y00 * 2.0 + a1)).abs() < 1e-6, "{s:?}");
        assert!((s.irradiance_over_pi[1] - y00 * 4.0).abs() < 1e-6, "{s:?}");
        assert!((s.irradiance_over_pi[2] - y00 * 6.0).abs() < 1e-6, "{s:?}");
        assert!((s.components[1][0] - y00).abs() < 1e-6, "{s:?}");
        assert_eq!(s.components[0], [0.0; 3]);
    }

    #[test]
    fn the_too_many_points_error_names_the_cap() {
        let err = GiPointsError::TooManyPoints {
            given: MAX_GI_POINTS + 1,
            max: MAX_GI_POINTS,
        };
        assert!(err.to_string().contains("at most 16384"), "{err}");
    }
}
