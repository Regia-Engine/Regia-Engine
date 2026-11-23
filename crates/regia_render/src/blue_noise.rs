// SPDX-License-Identifier: MPL-2.0
//! Spatiotemporal blue noise (N5, ADR-2075): a precomputed noise table for the stochastic passes
//! of a frame, off in every preset ([`crate::RenderProfile::blue_noise`]).
//!
//! The build script runs [`generate`] and the crate embeds its [`BYTES`] bytes: [`SLICES`] slices of
//! [`SIZE`]² texels, one byte each. Every slice is a threshold mask with a blue-noise spectrum
//! (a pixel's neighbors hold far-away values) and every pixel's sequence along time is blue too (its
//! next frames hold far-away values). So one sample per pixel per frame looks like fine grain
//! that a spatial filter or a temporal resolve removes quickly, where a hash leaves low-frequency
//! blotches and interleaved gradient noise leaves its lattice.
//!
//! **Where it goes.** With the knob on: the ray directions of stochastic SSR
//! (`sssr_intersect.wgsl`), the spiral rotation of screen-space GI (`ssgi.wgsl`) and of SSAO
//! (`ssao.wgsl`), which the temporal resolve accumulates. The temporal resolve itself draws no
//! random numbers. The GI denoiser's scenarios can draw their input from it
//! ([`crate::gi_denoise::scenario::GiScenario::blue_noise`]). Every use is deterministic: the slice
//! is the frame's noise index modulo [`SLICES`].
//!
//! **Cost.** With the knob off, nothing: the texture is not created, the passes bind and run
//! what they always did. On: one 64 KiB texture, and one more pipeline for each pass, built by the
//! frame warm (the `blue_noise` warm key) and bound through a second bind group.

mod generate;

pub use generate::{BYTES, REVISION, SIZE, SLICES, generate};

use std::sync::{Arc, OnceLock};

static TABLE: &[u8] = include_bytes!(env!("REGIA_STBN_TABLE"));

/// The embedded table: one byte per voxel, slice by slice, row by row.
#[must_use]
pub fn table() -> &'static [u8] {
    TABLE
}

/// FNV-1a of the embedded table, as agents read it back (`render.blue_noise`).
#[must_use]
pub fn table_hash() -> u64 {
    static HASH: OnceLock<u64> = OnceLock::new();
    *HASH.get_or_init(|| {
        TABLE.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    })
}

/// The number the shader's `scalar` returns at pixel `(x, y)` of frame `frame`, in `(0, 1)`.
#[must_use]
pub fn scalar(x: u32, y: u32, frame: u32) -> f32 {
    let (s, f) = (SIZE as u32, SLICES as u32);
    let i = (frame % f) * s * s + (y % s) * s + x % s;
    (f32::from(TABLE.get(i as usize).copied().unwrap_or(128)) + 0.5) / 256.0
}

/// The two numbers the shader's `pair` returns: the table at the pixel and at a far-off place in
/// space and time, whose values are unrelated to it.
#[must_use]
pub fn pair(x: u32, y: u32, frame: u32) -> [f32; 2] {
    [scalar(x, y, frame), scalar(x + 23, y + 41, frame + 5)]
}

/// The table on the GPU, with the bind group every variant pass takes as its group 1.
pub(crate) struct BlueNoiseGpu {
    /// Group 1's layout.
    pub(crate) layout: wgpu::BindGroupLayout,
    /// Group 1.
    pub(crate) group: wgpu::BindGroup,
    /// The table as a 2D array (the forward pass binds it in its view group, ADR-2652).
    pub(crate) view: wgpu::TextureView,
}

impl BlueNoiseGpu {
    fn new(device: &crate::device::TrackedDevice, queue: &wgpu::Queue) -> Self {
        let _span = tracing::info_span!("blue_noise_upload").entered();
        let extent = wgpu::Extent3d {
            width: SIZE as u32,
            height: SIZE as u32,
            depth_or_array_layers: SLICES as u32,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("blue noise"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            TABLE,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(SIZE as u32),
                rows_per_image: Some(SIZE as u32),
            },
            extent,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blue noise"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2Array,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("blue noise"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            }],
        });
        Self { layout, group, view }
    }
}

/// The frame renderer's blue-noise state: the table, once a frame asked for it, and whether this
/// frame uses it.
#[derive(Default)]
pub(crate) struct BlueNoiseState {
    gpu: Option<Arc<BlueNoiseGpu>>,
    on: bool,
}

impl BlueNoiseState {
    /// Called once per frame with the profile's knob.
    pub(crate) fn begin_frame(
        &mut self,
        device: &crate::device::TrackedDevice,
        queue: &wgpu::Queue,
        on: bool,
    ) {
        self.on = on;
        if on && self.gpu.is_none() {
            let _mem = regia_core::memory::enter_owner(&crate::heap_owners::BLUE_NOISE);
            self.gpu = Some(Arc::new(BlueNoiseGpu::new(device, queue)));
        }
    }

    /// This frame's table: `None` with the knob off.
    pub(crate) fn active(&self) -> Option<&Arc<BlueNoiseGpu>> {
        self.gpu.as_ref().filter(|_| self.on)
    }

    /// Whether the last frame used the table.
    pub(crate) fn is_on(&self) -> bool {
        self.on
    }
}

/// The share of a square image's AC power below `cut` of Nyquist (a radial low-pass, by a plain
/// DFT): blue noise has little there, white noise about `π cut² / 4`. `values` is `size × size`,
/// row-major. The spectrum check of the table's slices and of the forward pass's hashed-alpha
/// mask (ADR-2652); O(size⁴), meant for tests on 64 × 64 windows.
#[must_use]
pub fn low_frequency_share(values: &[u8], size: usize, cut: f32) -> f32 {
    let n = size;
    if n == 0 || values.len() < n * n {
        return 0.0;
    }
    let values = &values[..n * n];
    let mean = values.iter().map(|&v| f32::from(v)).sum::<f32>() / values.len() as f32;
    let tau = std::f32::consts::TAU;
    let (mut low, mut total) = (0.0f32, 0.0f32);
    for v in 0..n {
        let fy = v.min(n - v) as f32 / (n as f32 / 2.0);
        for u in 0..n {
            if u == 0 && v == 0 {
                continue;
            }
            let fx = u.min(n - u) as f32 / (n as f32 / 2.0);
            let (mut re, mut im) = (0.0f32, 0.0f32);
            for y in 0..n {
                for x in 0..n {
                    let a = tau * (u * x + v * y) as f32 / n as f32;
                    let s = f32::from(values[y * n + x]) - mean;
                    re += s * a.cos();
                    im -= s * a.sin();
                }
            }
            let p = re * re + im * im;
            total += p;
            if (fx * fx + fy * fy).sqrt() < cut {
                low += p;
            }
        }
    }
    if total > 0.0 { low / total } else { 0.0 }
}

/// The forward shader's device-level define for the frame's blue-noise binding (ADR-2652).
pub(crate) const FORWARD_DEF: &str = "FRAME_BLUE_NOISE";

/// Sampled textures per fragment stage a device needs for the forward pass's blue-noise
/// binding (38): the view group's 22 with it, a material's 8 and the texture table, with room.
pub(crate) const FORWARD_MIN_SAMPLED_TEXTURES: u32 = 48;

/// Whether a device's forward view layout carries the blue-noise table (binding 38): desktop
/// devices with room for one more fragment texture. Android keeps its layout.
pub(crate) fn forward_binding_supported(limits: &wgpu::Limits) -> bool {
    !cfg!(target_os = "android")
        && limits.max_sampled_textures_per_shader_stage >= FORWARD_MIN_SAMPLED_TEXTURES
}

/// Adds [`FORWARD_DEF`] to a forward module's defines on a device whose view layout has the
/// binding (every forward module of the device, so the prepass and the colour pass agree).
pub(crate) fn add_forward_defs(
    device: &wgpu::Device,
    defs: &mut std::collections::HashMap<String, naga_oil::compose::ShaderDefValue>,
) {
    if forward_binding_supported(&device.limits()) {
        defs.insert(
            FORWARD_DEF.to_string(),
            naga_oil::compose::ShaderDefValue::Bool(true),
        );
    }
}

/// The shader-def map of a pass's blue-noise variant.
pub(crate) fn variant_defs() -> std::collections::HashMap<String, naga_oil::compose::ShaderDefValue> {
    std::collections::HashMap::from([(
        "BLUE_NOISE".to_string(),
        naga_oil::compose::ShaderDefValue::Bool(true),
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn low_share_2d(slice: &[u8], cut: f32) -> f32 {
        low_frequency_share(slice, SIZE, cut)
    }

    #[test]
    fn the_embedded_table_is_what_the_generator_makes() {
        assert_eq!(table().len(), BYTES);
        assert_eq!(
            generate().as_slice(),
            table(),
            "bump REVISION when the generator changes"
        );
    }

    /// The pin an accidental change to the table trips: goldens of the on variant depend on it.
    #[test]
    fn the_table_hash_is_pinned() {
        assert_eq!(
            table_hash(),
            15_616_949_030_692_114_167,
            "ADR-2075: the blue-noise table moved"
        );
    }

    #[test]
    fn every_slice_holds_every_value_equally_often() {
        for slice in table().as_chunks::<{ SIZE * SIZE }>().0 {
            let mut counts = [0u32; 256];
            for &v in slice {
                counts[usize::from(v)] += 1;
            }
            assert!(counts.iter().all(|&c| c == 16), "{counts:?}");
        }
    }

    #[test]
    fn every_slice_has_a_blue_spectrum() {
        for (z, slice) in table()
            .as_chunks::<{ SIZE * SIZE }>()
            .0
            .iter()
            .enumerate()
            .step_by(5)
        {
            // White noise would hold π·0.3²/4 = 7 % of its power below 0.3 of Nyquist.
            let share = low_share_2d(slice, 0.3);
            assert!(share < 0.012, "slice {z}: {share}");
        }
    }

    /// Time: over its 16 frames a pixel sees values spread over the whole range, so a running
    /// average converges far faster than with independent numbers.
    #[test]
    fn a_pixel_averaged_over_time_converges_faster_than_white_noise() {
        let area = SIZE * SIZE;
        let (mut blue_err, mut cross_err) = (0.0f64, 0.0f64);
        // A reference with the same per-pixel distribution but no temporal structure: the value
        // at a far-off, frame-dependent pixel, so the sequence is independent along time.
        for p in 0..area {
            let (mut a, mut b) = (0.0f64, 0.0f64);
            for z in 0..SLICES {
                a += f64::from(table()[z * area + p]) / 255.0;
                b += f64::from(table()[z * area + (p + 97 * z * z + 1013 * z) % area]) / 255.0;
            }
            blue_err += (a / SLICES as f64 - 0.5).powi(2);
            cross_err += (b / SLICES as f64 - 0.5).powi(2);
        }
        let ratio = (blue_err / cross_err).sqrt();
        eprintln!("rms of a 16-frame mean, relative to unstructured numbers: {ratio:.3}");
        assert!(ratio < 0.4, "{ratio}");
    }

    #[test]
    fn neighbors_differ_more_than_chance_in_space_and_time() {
        let area = SIZE * SIZE;
        let at = |x: usize, y: usize, z: usize| {
            f64::from(table()[(z % SLICES) * area + (y % SIZE) * SIZE + x % SIZE])
        };
        let (mut space, mut time, mut n) = (0.0, 0.0, 0.0);
        for z in 0..SLICES {
            for y in 0..SIZE {
                for x in 0..SIZE {
                    space += (at(x, y, z) - at(x + 1, y, z)).abs();
                    time += (at(x, y, z) - at(x, y, z + 1)).abs();
                    n += 1.0;
                }
            }
        }
        // Independent uniform bytes differ by 85 on average.
        let (space, time) = (space / n, time / n);
        eprintln!("mean neighbor difference: space {space:.1}, time {time:.1}");
        assert!(space > 95.0, "{space}");
        assert!(time > 95.0, "{time}");
    }

    #[test]
    fn the_cpu_mirror_wraps_and_stays_inside_the_open_range() {
        assert_eq!(scalar(3, 7, 2), scalar(3 + 64, 7 + 128, 2 + 16));
        let (lo, hi) = (0.5 / 256.0, 255.5 / 256.0);
        for &v in &[scalar(0, 0, 0), scalar(63, 63, 15), pair(1, 2, 3)[1]] {
            assert!(v >= lo && v <= hi);
        }
        let [a, b] = pair(10, 20, 4);
        assert_ne!(a, b);
    }

    /// Every variant composes and validates: the frame's on path compiles what these name.
    #[test]
    fn the_variants_compose() {
        for (source, file) in [
            (
                include_str!("../shaders/sssr_intersect.wgsl"),
                "sssr_intersect.wgsl",
            ),
            (include_str!("../shaders/ssao.wgsl"), "ssao.wgsl"),
            (include_str!("../shaders/ssgi.wgsl"), "ssgi.wgsl"),
        ] {
            let module = crate::shader_lib::compose_source(source, file, variant_defs())
                .unwrap_or_else(|err| panic!("{file}: {err}"));
            naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::all(),
            )
            .validate(&module)
            .unwrap_or_else(|err| panic!("{file}: {err:?}"));
            assert!(
                module
                    .global_variables
                    .iter()
                    .any(|(_, g)| { g.binding.as_ref().is_some_and(|b| b.group == 1 && b.binding == 0) }),
                "{file}: the variant binds the table as group 1"
            );
        }
    }
}
