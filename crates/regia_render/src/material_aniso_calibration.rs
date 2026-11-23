// SPDX-License-Identifier: MPL-2.0
//! Per-device calibration of the material sampler's anisotropic mip blend (ADR-5670).
//!
//! Vulkan leaves the anisotropic filter implementation-defined (the spec's "Texel Anisotropic
//! Filtering": the scheme is implementation-dependent); only the isotropic trilinear filter has an
//! exact definition (the two levels blend by the LOD's fraction). Drivers narrow the blend between
//! two mips once anisotropy is on, and not all in the same place. A probe (an isotropic footprint
//! through `textureSampleBias` and `textureSampleGrad`, 8x and 16x samplers, a 2D texture whose mip
//! `k` holds `k`) read, for a level fraction `f`:
//!
//! * isotropic sampler, every adapter: `f` (exact);
//! * NVIDIA (RTX 5090, 615.71): `clamp(2 f, 0, 1)`, never sharper than exact;
//! * Mesa RADV (RX 7900 XTX, Mesa 26.2.6): `clamp(2 f - 0.5, 0, 1)`, sharper than exact below `f = 0.5`.
//!
//! The material look and the TSR texture-bias bars were tuned on the first, so on RADV a TSR frame's
//! biased and unbiased mips came out closer than the bias is meant to make them
//! (`tsr_at_067_keeps_native_texture_detail`). RADV's curve moved a quarter mip up is NVIDIA's exactly.
//!
//! The renderer therefore measures each device once, when it builds the material sampler: the
//! largest amount by which the anisotropic sampler reads a sharper level than the isotropic one at
//! quarter-mip fractions becomes a mip offset ([`anisotropy_lod_offset_from_blend`]) that every
//! material sample through that sampler adds to the frame's texture bias. An adapter whose
//! anisotropic blend is never sharper than exact (NVIDIA, lavapipe) measures 0 and draws exactly as
//! before. [`MATERIAL_ANISO_LOD_OFFSET_ENV`] overrides the measurement.

use std::sync::atomic::{AtomicU32, Ordering};

/// The environment variable that sets the material sampler's LOD offset in mips (clamped to
/// 0..=[`MAX_MATERIAL_ANISO_LOD_OFFSET`]) instead of measuring it; `0` turns the calibration off.
pub const MATERIAL_ANISO_LOD_OFFSET_ENV: &str = "REGIA_MATERIAL_ANISO_LOD_OFFSET";

/// The largest material sampler LOD offset the probe or the environment may set, in mips.
pub const MAX_MATERIAL_ANISO_LOD_OFFSET: f32 = 0.5;

/// A measured deficit below this (mips) counts as exact: filter-weight rounding, not a narrowed blend.
const DEFICIT_FLOOR: f32 = 1.0 / 16.0;

/// The probe's level fractions (within level 1..2, so level 0's magnification never enters).
pub const PROBE_FRACTIONS: [f32; 3] = [0.125, 0.25, 0.375];

/// Number of [`PROBE_FRACTIONS`] (one compute invocation each).
const PROBE_COUNT: u32 = PROBE_FRACTIONS.len() as u32;

/// Bits of the most recently calibrated device's offset (`f32::NAN` bits before any).
static LAST_OFFSET: AtomicU32 = AtomicU32::new(0x7fc0_0000);

/// The material sampler LOD offset (mips) of the most recently calibrated device in this process
/// (`None` before any renderer built its pipelines), as the agent protocol reports it.
#[must_use]
pub fn last_material_anisotropy_lod_offset() -> Option<f32> {
    let v = f32::from_bits(LAST_OFFSET.load(Ordering::Relaxed));
    (!v.is_nan()).then_some(v)
}

/// The offset (mips) that keeps an anisotropic sampler from filtering an isotropic footprint
/// sharper than the exact trilinear level: the largest `exact - anisotropic` over the probe's
/// `(exact, anisotropic)` level pairs, 0 when under 1/16 mip, rounded to 1/32 mip and clamped to
/// [`MAX_MATERIAL_ANISO_LOD_OFFSET`]. Non-finite pairs are ignored.
#[must_use]
pub fn anisotropy_lod_offset_from_blend(levels: &[(f32, f32)]) -> f32 {
    let deficit = levels
        .iter()
        .filter(|(exact, aniso)| exact.is_finite() && aniso.is_finite())
        .map(|(exact, aniso)| exact - aniso)
        .fold(0.0f32, f32::max);
    if deficit < DEFICIT_FLOOR {
        return 0.0;
    }
    ((deficit * 32.0).round() / 32.0).clamp(0.0, MAX_MATERIAL_ANISO_LOD_OFFSET)
}

/// The value of [`MATERIAL_ANISO_LOD_OFFSET_ENV`] as an offset, if it parses as a finite number.
#[must_use]
pub fn offset_from_env(value: Option<&str>) -> Option<f32> {
    value
        .and_then(|s| s.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite())
        .map(|v| v.clamp(0.0, MAX_MATERIAL_ANISO_LOD_OFFSET))
}

/// Level-0 size of the probe texture (texels).
const PROBE_SIZE: u32 = 64;
/// Mip levels of the probe texture.
const PROBE_LEVELS: u32 = 7;

/// One query per invocation: `g` = the footprint in normalized UV (x axis, y axis);
/// out = (isotropic level, anisotropic level, 0, 0).
const PROBE_WGSL: &str = r"
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var s_iso: sampler;
@group(0) @binding(2) var s_aniso: sampler;
@group(0) @binding(3) var<storage, read> qs: array<vec4<f32>>;
@group(0) @binding(4) var<storage, read_write> out: array<vec4<f32>>;
@compute @workgroup_size(1) fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let g = qs[id.x];
    let uv = vec2<f32>(0.37, 0.61);
    let iso = textureSampleGrad(tex, s_iso, uv, g.xy, g.zw).r;
    let aniso = textureSampleGrad(tex, s_aniso, uv, g.xy, g.zw).r;
    out[id.x] = vec4<f32>(iso, aniso, 0.0, 0.0);
}
";

/// [`PROBE_WGSL`] over a one-layer 2D array (the terrain's layer, triplanar and macro textures are
/// arrays, and a driver may filter an array's explicit-gradient samples differently, ADR-3957).
const PROBE_ARRAY_WGSL: &str = r"
@group(0) @binding(0) var tex: texture_2d_array<f32>;
@group(0) @binding(1) var s_iso: sampler;
@group(0) @binding(2) var s_aniso: sampler;
@group(0) @binding(3) var<storage, read> qs: array<vec4<f32>>;
@group(0) @binding(4) var<storage, read_write> out: array<vec4<f32>>;
@compute @workgroup_size(1) fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let g = qs[id.x];
    let uv = vec2<f32>(0.37, 0.61);
    let iso = textureSampleGrad(tex, s_iso, uv, 0, g.xy, g.zw).r;
    let aniso = textureSampleGrad(tex, s_aniso, uv, 0, g.xy, g.zw).r;
    out[id.x] = vec4<f32>(iso, aniso, 0.0, 0.0);
}
";

/// The texture kind the probe samples ([`measure_anisotropy_blend_on`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProbeTexture {
    /// A 2D texture: the material textures.
    D2,
    /// A 2D array texture: the terrain's layer, triplanar and macro textures.
    D2Array,
}

/// IEEE half bits of a small non-negative integer `v` (exact for 0..=2048).
fn half_bits(v: u32) -> u16 {
    if v == 0 {
        return 0;
    }
    let bits = (v as f32).to_bits();
    let exp = ((bits >> 23) & 0xff) + 15 - 127;
    let mant = (bits >> 13) & 0x3ff;
    u16::try_from((exp << 10) | mant).unwrap_or(0)
}

/// Measures, for each of [`PROBE_FRACTIONS`], the level an isotropic footprint of `2^(1 + f)`
/// texels filters at through an isotropic trilinear sampler and through the material sampler's
/// descriptor with `anisotropy` (repeat, linear, trilinear): `(isotropic, anisotropic)` pairs.
/// `None` where the device has no compute or storage buffers, on the web (no blocking readback),
/// or when the readback fails. Creates its own resources and submits on `queue` once.
#[must_use]
pub fn measure_anisotropy_blend(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    anisotropy: u16,
) -> Option<Vec<(f32, f32)>> {
    measure_anisotropy_blend_on(device, queue, anisotropy, ProbeTexture::D2)
}

/// [`measure_anisotropy_blend`] over the texture kind `texture`.
#[must_use]
pub fn measure_anisotropy_blend_on(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    anisotropy: u16,
    texture: ProbeTexture,
) -> Option<Vec<(f32, f32)>> {
    if cfg!(target_arch = "wasm32") {
        return None;
    }
    let limits = device.limits();
    if limits.max_compute_invocations_per_workgroup == 0 || limits.max_storage_buffers_per_shader_stage < 2 {
        return None;
    }
    let _span = tracing::info_span!("material_aniso_calibration").entered();
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("material aniso calibration"),
        size: wgpu::Extent3d {
            width: PROBE_SIZE,
            height: PROBE_SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: PROBE_LEVELS,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for level in 0..PROBE_LEVELS {
        let s = (PROBE_SIZE >> level).max(1);
        let (v, one) = (half_bits(level), half_bits(1));
        let px: Vec<u8> = (0..s * s)
            .flat_map(|_| [v, v, v, one])
            .flat_map(u16::to_le_bytes)
            .collect();
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: level,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &px,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(8 * s),
                rows_per_image: Some(s),
            },
            wgpu::Extent3d {
                width: s,
                height: s,
                depth_or_array_layers: 1,
            },
        );
    }
    let sampler = |anisotropy_clamp: u16| {
        device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("material aniso calibration"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            anisotropy_clamp,
            ..Default::default()
        })
    };
    let (s_iso, s_aniso) = (sampler(1), sampler(anisotropy.max(1)));
    let n = PROBE_SIZE as f32;
    let queries: Vec<u8> = PROBE_FRACTIONS
        .iter()
        .flat_map(|f| {
            let s = (1.0 + f).exp2() / n;
            [s, 0.0, 0.0, s]
        })
        .flat_map(f32::to_le_bytes)
        .collect();
    let qs = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("material aniso calibration queries"),
        size: queries.len() as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&qs, 0, &queries);
    let size = (PROBE_FRACTIONS.len() * 16) as u64;
    let out = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("material aniso calibration out"),
        size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let read = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("material aniso calibration read"),
        size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("material aniso calibration"),
        source: wgpu::ShaderSource::Wgsl(
            match texture {
                ProbeTexture::D2 => PROBE_WGSL,
                ProbeTexture::D2Array => PROBE_ARRAY_WGSL,
            }
            .into(),
        ),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("material aniso calibration"),
        layout: None,
        module: &module,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(match texture {
            ProbeTexture::D2 => wgpu::TextureViewDimension::D2,
            ProbeTexture::D2Array => wgpu::TextureViewDimension::D2Array,
        }),
        ..Default::default()
    });
    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("material aniso calibration"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&s_iso),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(&s_aniso),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: qs.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: out.as_entire_binding(),
            },
        ],
    });
    let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("material aniso calibration"),
    });
    {
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(PROBE_COUNT, 1, 1);
    }
    enc.copy_buffer_to_buffer(&out, 0, &read, 0, size);
    let index = queue.submit([enc.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    read.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r.is_ok());
    });
    if let Err(err) = device.poll(wgpu::PollType::Wait {
        submission_index: Some(index),
        timeout: None,
    }) {
        tracing::warn!(target: "regia_render", "material aniso calibration: poll failed ({err}); no offset");
        return None;
    }
    if !rx.try_recv().unwrap_or(false) {
        return None;
    }
    let levels = {
        let data = read.slice(..).get_mapped_range().ok()?;
        data.as_chunks::<16>()
            .0
            .iter()
            .map(|px| {
                let f = |i: usize| f32::from_le_bytes([px[i], px[i + 1], px[i + 2], px[i + 3]]);
                (f(0), f(4))
            })
            .collect::<Vec<_>>()
    };
    read.unmap();
    Some(levels)
}

/// The measured LOD offset of an anisotropic sampler with clamp `anisotropy` over the texture kind
/// `texture` on a device, in mips: [`measure_anisotropy_blend_on`] through
/// [`anisotropy_lod_offset_from_blend`]. `None` when the probe cannot run or its isotropic control
/// is not the exact level, so a caller with another source for the offset can fall back to it (the
/// terrain's sampler path, ADR-5797). Reads no environment variable and records nothing.
#[must_use]
pub fn measured_anisotropy_lod_offset(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    anisotropy: u16,
    texture: ProbeTexture,
) -> Option<f32> {
    if anisotropy <= 1 {
        return Some(0.0);
    }
    let levels = measure_anisotropy_blend_on(device, queue, anisotropy, texture)?;
    let control_ok = levels
        .iter()
        .zip(PROBE_FRACTIONS)
        .all(|(&(iso, _), f)| (iso - (1.0 + f)).abs() < 0.1);
    if !control_ok {
        tracing::warn!(
            target: "regia_render",
            "material aniso calibration: the isotropic control read {levels:?}, not 1 + {PROBE_FRACTIONS:?}; no offset"
        );
        return None;
    }
    Some(anisotropy_lod_offset_from_blend(&levels))
}

/// The material sampler LOD offset for a device (ADR-5670): [`MATERIAL_ANISO_LOD_OFFSET_ENV`] when
/// set, else the measurement ([`measured_anisotropy_lod_offset`]); 0 when the probe cannot run or
/// its isotropic control is not the exact level. Logs a non-zero offset and records it
/// ([`last_material_anisotropy_lod_offset`]).
#[must_use]
pub fn material_anisotropy_lod_offset(device: &wgpu::Device, queue: &wgpu::Queue, anisotropy: u16) -> f32 {
    let env = std::env::var(MATERIAL_ANISO_LOD_OFFSET_ENV).ok();
    let offset = offset_from_env(env.as_deref()).unwrap_or_else(|| {
        measured_anisotropy_lod_offset(device, queue, anisotropy, ProbeTexture::D2).unwrap_or(0.0)
    });
    if offset > 0.0 {
        tracing::info!(
            target: "regia_render",
            "material sampler: anisotropic blend LOD offset {offset:.3} mips (ADR-5670)"
        );
    }
    LAST_OFFSET.store(offset.to_bits(), Ordering::Relaxed);
    offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blend_never_sharper_than_exact_needs_no_offset() {
        // NVIDIA's narrowed blend (softer) and lavapipe's exact one (within weight rounding).
        let nvidia: Vec<(f32, f32)> = PROBE_FRACTIONS
            .iter()
            .map(|f| (1.0 + f, 1.0 + (2.0 * f).min(1.0)))
            .collect();
        assert_eq!(anisotropy_lod_offset_from_blend(&nvidia), 0.0);
        let exact: Vec<(f32, f32)> = PROBE_FRACTIONS
            .iter()
            .map(|f| (1.0 + f, 1.0 + f - 0.01))
            .collect();
        assert_eq!(anisotropy_lod_offset_from_blend(&exact), 0.0);
    }

    #[test]
    fn a_centred_narrow_blend_gets_a_quarter_mip() {
        // RADV's measured curve: clamp(2 f - 0.5, 0, 1); moved a quarter mip up it is clamp(2 f).
        let radv: Vec<(f32, f32)> = PROBE_FRACTIONS
            .iter()
            .map(|f| (1.0 + f, 1.0 + (2.0 * f - 0.5).clamp(0.0, 1.0)))
            .collect();
        let offset = anisotropy_lod_offset_from_blend(&radv);
        assert_eq!(offset, 0.25);
        for f in (0..=32).map(|i| i as f32 / 32.0) {
            let shifted = 2.0 * (f + offset) - 0.5;
            let nvidia = 2.0 * f;
            assert!(
                (shifted.clamp(0.0, 1.0) - nvidia.clamp(0.0, 1.0)).abs() < 1e-6,
                "f {f}"
            );
            assert!(
                shifted.clamp(0.0, 1.0) >= f - 1e-6,
                "never sharper than exact at f {f}"
            );
        }
    }

    #[test]
    fn the_offset_is_rounded_clamped_and_ignores_garbage() {
        assert_eq!(anisotropy_lod_offset_from_blend(&[(1.3, 1.0)]), 0.3125);
        assert_eq!(
            anisotropy_lod_offset_from_blend(&[(2.0, 1.0)]),
            MAX_MATERIAL_ANISO_LOD_OFFSET
        );
        assert_eq!(
            anisotropy_lod_offset_from_blend(&[(f32::NAN, 0.0), (1.0, f32::INFINITY)]),
            0.0
        );
        assert_eq!(anisotropy_lod_offset_from_blend(&[]), 0.0);
    }

    #[test]
    fn the_environment_overrides_within_range() {
        assert_eq!(offset_from_env(Some(" 0.25 ")), Some(0.25));
        assert_eq!(offset_from_env(Some("0")), Some(0.0));
        assert_eq!(offset_from_env(Some("-1")), Some(0.0));
        assert_eq!(offset_from_env(Some("9")), Some(MAX_MATERIAL_ANISO_LOD_OFFSET));
        assert_eq!(offset_from_env(Some("nan")), None);
        assert_eq!(offset_from_env(Some("soft")), None);
        assert_eq!(offset_from_env(None), None);
    }

    #[test]
    fn half_bits_are_exact_for_levels() {
        assert_eq!(half_bits(0), 0);
        assert_eq!(half_bits(1), 0x3c00);
        assert_eq!(half_bits(2), 0x4000);
        assert_eq!(half_bits(3), 0x4200);
        assert_eq!(half_bits(6), 0x4600);
    }
}
