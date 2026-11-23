// SPDX-License-Identifier: MPL-2.0
//! GPU particle bounds (P-225 (2), ADR-8355): a reduction over the live pool into one world AABB
//! per emitter record, read back on request.
//!
//! One thread per slot of the frame's update window and the child ring (the slots that can be
//! alive, `pool_slot` order). A live particle's centre is grown by its drawn extent
//! ([`crate::bounds::particle_extent`], the same rule as the CPU measured bounds) and folded into
//! its record's six words with `atomicMin` / `atomicMax` on an order-preserving `f32 → u32` key
//! (negative floats flip every bit, positive ones the sign bit), so the result does not depend on
//! thread order. Nothing runs until the first [`VfxGpu::read_particle_bounds`] call: the
//! pipeline, the output and the readback buffers are built then (the editor's opt-in), so the
//! game frame path never pays for it. The read blocks on the device: callers bound its rate.

use regia_core::math::{Aabb, Vec3};
use regia_render::TrackedDevice;

use super::VfxGpu;
use super::layout::{EmitterGpu, MAX_EMITTERS};

/// Words per record: min xyz, max xyz.
const WORDS: u32 = 6;
const _: () = assert!(WORDS == 6);

/// Workgroup size of the reduction.
const GROUP: u32 = 64;

/// The reduction kernel. Override constants: the record stride and the field offsets in vec4s.
pub const BOUNDS_REDUCE: &str = r#"
@group(0) @binding(0) var<storage, read> pos_age: array<vec4<f32>>;
@group(0) @binding(1) var<storage, read> vel_life: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read> attrib: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> emitters: array<vec4<f32>>;
@group(0) @binding(4) var<storage, read> curve_scalars: array<f32>;
@group(0) @binding(5) var<storage, read_write> bounds: array<atomic<u32>>;
// x: first window slot, y: window length, z: main-ring capacity, w: child-ring slots updated.
@group(0) @binding(6) var<uniform> pool_window: vec4<u32>;

override emitter_vec4s: u32 = 17u;
override curve_ids_vec4: u32 = 5u;
override render_vec4: u32 = 6u;
override stretch_vec4: u32 = 8u;
override records: u32 = 256u;

// Order-preserving key: a < b as floats iff key(a) < key(b) as u32.
fn ordered_key(f: f32) -> u32 {
    let b = bitcast<u32>(f);
    if ((b & 0x80000000u) != 0u) { return ~b; }
    return b | 0x80000000u;
}

// `lut_scalar` (8 samples, linear).
fn size_curve(id: u32, t: f32) -> f32 {
    let n = 8u;
    let x = clamp(t, 0.0, 1.0) * f32(n - 1u);
    let i = min(u32(x), n - 2u);
    let f = x - f32(i);
    let a = curve_scalars[id * n + i];
    let b = curve_scalars[id * n + i + 1u];
    return a + (b - a) * f;
}

// `bounds::particle_extent`, operation for operation.
fn particle_extent(mode: u32, size_in: f32, speed: f32, stretch: vec2<f32>) -> f32 {
    let size = abs(size_in);
    if (mode == 1u) {
        let min_len = select(stretch.y, -stretch.y * size, stretch.y < 0.0);
        let along = max(max(stretch.x * speed, min_len), 0.0);
        return sqrt(0.25 * (along * along + size * size));
    }
    if (mode == 2u) { return size * 0.8660254; }
    return size * 0.70710677;
}

@compute @workgroup_size(64)
fn reduce_bounds(@builtin(global_invocation_id) gid: vec3<u32>) {
    let k = gid.x;
    let capacity = max(pool_window.z, 1u);
    let ring = min(pool_window.y, capacity);
    if (k >= ring + pool_window.w) { return; }
    var i = capacity + (k - ring);
    if (k < ring) { i = (pool_window.x + k) % capacity; }
    if (i >= arrayLength(&pos_age)) { return; }
    let vl = vel_life[i];
    if (vl.w <= 0.0) { return; }
    let pa = pos_age[i];
    if (pa.w >= vl.w) { return; }
    let at = attrib[i];
    let slot = u32(at.w);
    if (slot >= records) { return; }
    let base = slot * emitter_vec4s;
    let ids = bitcast<vec4<u32>>(emitters[base + curve_ids_vec4]);
    let mode = u32(emitters[base + render_vec4].x);
    let stretch = emitters[base + stretch_vec4].xy;
    let t = clamp(pa.w / max(vl.w, 1e-5), 0.0, 1.0);
    let r = particle_extent(mode, at.x * size_curve(ids.x, t), length(vl.xyz), stretch);
    let lo = pa.xyz - vec3<f32>(r);
    let hi = pa.xyz + vec3<f32>(r);
    // A NaN fails every comparison, so a non-finite particle is skipped here.
    let big = vec3<f32>(3.0e38);
    if (!(all(abs(lo) < big) && all(abs(hi) < big))) { return; }
    let o = slot * 6u;
    atomicMin(&bounds[o + 0u], ordered_key(lo.x));
    atomicMin(&bounds[o + 1u], ordered_key(lo.y));
    atomicMin(&bounds[o + 2u], ordered_key(lo.z));
    atomicMax(&bounds[o + 3u], ordered_key(hi.x));
    atomicMax(&bounds[o + 4u], ordered_key(hi.y));
    atomicMax(&bounds[o + 5u], ordered_key(hi.z));
}
"#;

/// The kernel's name and source (the shader checks list it, [`super::shader_sources`]).
#[must_use]
pub fn bounds_source() -> (String, String) {
    ("vfx_bounds_reduce".into(), BOUNDS_REDUCE.into())
}

/// `ordered_key` of the kernel.
#[must_use]
pub fn ordered_key(f: f32) -> u32 {
    let b = f.to_bits();
    if b & 0x8000_0000 != 0 { !b } else { b | 0x8000_0000 }
}

/// The inverse of [`ordered_key`].
#[must_use]
pub fn from_ordered_key(k: u32) -> f32 {
    if k & 0x8000_0000 != 0 {
        f32::from_bits(k & 0x7FFF_FFFF)
    } else {
        f32::from_bits(!k)
    }
}

/// The reduction's pipeline and buffers, built on first use.
pub(crate) struct BoundsReduce {
    pipeline: wgpu::ComputePipeline,
    bgl: wgpu::BindGroupLayout,
    out: wgpu::Buffer,
    read: wgpu::Buffer,
    window: wgpu::Buffer,
}

impl BoundsReduce {
    fn new(device: &TrackedDevice) -> Self {
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
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx bounds bgl"),
            entries: &[
                storage(0, true),
                storage(1, true),
                storage(2, true),
                storage(3, true),
                storage(4, true),
                storage(5, false),
                wgpu::BindGroupLayoutEntry {
                    binding: 6,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vfx bounds layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vfx bounds reduce"),
            source: wgpu::ShaderSource::Wgsl(BOUNDS_REDUCE.into()),
        });
        let vec4s = |offset: usize| (offset / 16) as f64;
        let constants = [
            ("emitter_vec4s", vec4s(size_of::<EmitterGpu>())),
            (
                "curve_ids_vec4",
                vec4s(std::mem::offset_of!(EmitterGpu, curve_ids)),
            ),
            ("render_vec4", vec4s(std::mem::offset_of!(EmitterGpu, render))),
            ("stretch_vec4", vec4s(std::mem::offset_of!(EmitterGpu, stretch))),
            ("records", f64::from(MAX_EMITTERS)),
        ];
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("vfx bounds reduce"),
            layout: Some(&layout),
            module: &module,
            entry_point: Some("reduce_bounds"),
            compilation_options: wgpu::PipelineCompilationOptions {
                constants: &constants,
                ..Default::default()
            },
            cache: regia_render::pipeline_cache::cache_for(device).as_deref(),
        });
        let bytes = u64::from(MAX_EMITTERS * WORDS) * 4;
        let out = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx bounds out"),
            size: bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let read = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx bounds read"),
            size: bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let window = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx bounds window"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            bgl,
            out,
            read,
            window,
        }
    }
}

/// A whole-buffer bind group entry.
fn entry(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

/// Decodes the six words of every record (`None` where no particle landed).
fn decode(words: &[u32]) -> Vec<Option<Aabb>> {
    words
        .as_chunks::<6>()
        .0
        .iter()
        .map(|w| {
            // An untouched record keeps its cleared min (all ones) / max (zero) words.
            if w[0] == u32::MAX || w[3] == 0 {
                return None;
            }
            let v = |i: usize| from_ordered_key(w[i]);
            Some(Aabb::new(
                Vec3::new(v(0), v(1), v(2)),
                Vec3::new(v(3), v(4), v(5)),
            ))
        })
        .collect()
}

impl VfxGpu {
    /// Reduces the live pool into one world AABB per emitter record slot (index = record slot,
    /// [`MAX_EMITTERS`] entries; `None` for a record with no live particle): each particle's
    /// centre grown by its drawn extent ([`crate::bounds`]). Blocks on the device; the first call
    /// builds the kernel. Editor diagnostics (P-225): call it at a bounded rate, never per frame
    /// in a game. `None` when the readback fails.
    pub fn read_particle_bounds(&mut self) -> Option<Vec<Option<Aabb>>> {
        let _span = tracing::info_span!("vfx_read_particle_bounds").entered();
        if self.bounds_reduce.is_none() {
            self.bounds_reduce = Some(BoundsReduce::new(&self.device));
        }
        let reduce = self.bounds_reduce.as_ref()?;
        let capacity = self.capacity.max(1);
        let main = self.update_window.1.min(capacity);
        let child = self.frame_cpu.events[0];
        let window = [self.update_window.0, main, capacity, child];
        self.queue
            .write_buffer(&reduce.window, 0, bytemuck::cast_slice(&window));
        let cleared: Vec<u32> = (0..MAX_EMITTERS)
            .flat_map(|_| [u32::MAX, u32::MAX, u32::MAX, 0, 0, 0])
            .collect();
        self.queue
            .write_buffer(&reduce.out, 0, bytemuck::cast_slice(&cleared));
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vfx bounds bg"),
            layout: &reduce.bgl,
            entries: &[
                entry(0, &self.sim.pos_age),
                entry(1, &self.sim.vel_life),
                entry(2, &self.sim.attrib),
                entry(3, &self.sim.emitters),
                entry(4, &self.sim.curves_scalars),
                entry(5, &reduce.out),
                entry(6, &reduce.window),
            ],
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vfx bounds"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("vfx bounds reduce"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&reduce.pipeline);
            pass.set_bind_group(0, Some(&bind_group), &[]);
            pass.dispatch_workgroups((main + child).max(1).div_ceil(GROUP), 1, 1);
        }
        encoder.copy_buffer_to_buffer(&reduce.out, 0, &reduce.read, 0, reduce.out.size());
        self.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        let slice = reduce.read.slice(..);
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let out = (|| {
            self.device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
            rx.recv().ok()?.ok()?;
            let data = slice.get_mapped_range().ok()?;
            Some(decode(bytemuck::cast_slice::<u8, u32>(&data[..])))
        })();
        // Unmap on every path: a map left pending would panic the next `map_async`.
        reduce.read.unmap();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_keys_sort_like_floats_and_round_trip() {
        let values = [-1.0e30f32, -3.5, -0.0, 0.0, 1.0e-6, 2.0, 7.5e20];
        let keys: Vec<u32> = values.iter().map(|v| ordered_key(*v)).collect();
        assert!(keys.windows(2).all(|w| w[0] <= w[1]), "{keys:?}");
        for v in values {
            assert_eq!(from_ordered_key(ordered_key(v)).to_bits(), v.to_bits());
        }
        // The cleared words decode as "no particle".
        assert_eq!(decode(&[u32::MAX, u32::MAX, u32::MAX, 0, 0, 0]), vec![None]);
    }

    /// The kernel's override defaults are the record layout (the pipeline sets them too).
    #[test]
    fn override_defaults_match_the_record_layout() {
        let at = |offset: usize| offset / 16;
        for (name, value) in [
            ("emitter_vec4s", size_of::<EmitterGpu>() / 16),
            ("curve_ids_vec4", at(std::mem::offset_of!(EmitterGpu, curve_ids))),
            ("render_vec4", at(std::mem::offset_of!(EmitterGpu, render))),
            ("stretch_vec4", at(std::mem::offset_of!(EmitterGpu, stretch))),
            ("records", MAX_EMITTERS as usize),
        ] {
            let line = format!("override {name}: u32 = {value}u;");
            assert!(BOUNDS_REDUCE.contains(&line), "{line}");
        }
    }

    #[test]
    fn bounds_kernel_has_no_function_layout_errors() {
        let (name, source) = bounds_source();
        let errors =
            regia_render::wgsl_function_layout_errors(&name, &source).expect("bounds kernel emits SPIR-V");
        assert!(errors.is_empty(), "{name}: {errors:?}");
    }

    #[test]
    fn bounds_kernel_passes_spirv_val() {
        let (name, source) = bounds_source();
        if let Err(err) = regia_render::validate_wgsl_modules(&[(&name, &source)]) {
            panic!("bounds kernel fails validation: {err}");
        }
    }
}
