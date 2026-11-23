// SPDX-License-Identifier: MIT-0
//! GPU heater for cost runs (`look_r2 --perf` with `LOOK_R2_PERF_HEAT=<iterations>`).
//!
//! A lookdev frame spends most of its wall time on the CPU (scene build, readback), so the GPU
//! idles between frames and drops to its low power state: on the farm's bench RTX 5090 the SM
//! clock sat at 225–300 MHz (P8) through a whole `--perf` run, and every pass timing scaled
//! with it. A game keeps the GPU busy and its clocks up. Before each timed frame the heater
//! submits a throw-away ALU loop on the same queue, so the frame's passes run right after it at
//! the clocks the loop raised; it shares no resources with the frame and, on the same queue,
//! never overlaps the timed passes.

const SHADER: &str = r"
struct Params { iterations: u32, _a: u32, _b: u32, _c: u32 }
@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read_write> sink: array<f32>;
@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    var x = f32(g.x) * 1e-6;
    for (var i = 0u; i < params.iterations; i = i + 1u) {
        x = fma(x, 0.9999999, 1e-7);
    }
    if (x == 123.456) {
        sink[g.x & 1023u] = x;
    }
}
";

/// A compute pipeline that keeps the GPU busy for a while.
pub struct GpuHeater {
    pipeline: wgpu::ComputePipeline,
    bind_group: wgpu::BindGroup,
}

impl GpuHeater {
    /// The heater with `iterations` ALU steps per thread (about 4 ms at full clocks per
    /// 200 000 on an RTX 5090), or `None` when `LOOK_R2_PERF_HEAT` is unset or not a number.
    #[must_use]
    pub fn from_env(device: &wgpu::Device) -> Option<Self> {
        let iterations: u32 = std::env::var("LOOK_R2_PERF_HEAT").ok()?.parse().ok()?;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("perf heater"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("perf heater"),
            layout: None,
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        let params = wgpu::util::DeviceExt::create_buffer_init(
            device,
            &wgpu::util::BufferInitDescriptor {
                label: Some("perf heater params"),
                contents: &[iterations.to_le_bytes(), [0; 4], [0; 4], [0; 4]].concat(),
                usage: wgpu::BufferUsages::UNIFORM,
            },
        );
        let sink = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("perf heater sink"),
            size: 4096,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("perf heater"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: sink.as_entire_binding(),
                },
            ],
        });
        Some(Self { pipeline, bind_group })
    }

    /// Submits one heater dispatch (1 M threads).
    pub fn heat(&self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("perf heater"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("perf heater"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.dispatch_workgroups(4096, 1, 1);
        }
        queue.submit([encoder.finish()]);
    }
}
