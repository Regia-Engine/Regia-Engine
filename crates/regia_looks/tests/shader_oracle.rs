// SPDX-License-Identifier: MPL-2.0
//! The shared WGSL helpers evaluated on the GPU (lavapipe on CI hosts) and checked as numbers. Both
//! renderers include these files verbatim (`regia_looks::viewer::hook_source` and `LOOKS_SHADER`),
//! so a helper's range is a property of every look that calls it. No image, no golden.
#![cfg(feature = "gpu")]

const METAL_GRAIN: &str = include_str!("../src/shaders/shared/metal_grain.wgsl");

/// Samples evaluated per helper: the input runs from 0 to 1 in `SAMPLES - 1` steps.
const SAMPLES: u32 = 101;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::yield_now();
    }
}

fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::from_env()
        .unwrap_or(wgpu::Backends::VULKAN | wgpu::Backends::METAL | wgpu::Backends::DX12);
    let instance = wgpu::Instance::new(descriptor);
    let opened = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        force_fallback_adapter: false,
        compatible_surface: None,
        ..Default::default()
    }))
    .map_err(|e| e.to_string())
    .and_then(|adapter| {
        block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("regia_looks shader oracle"),
            ..Default::default()
        }))
        .map_err(|e| e.to_string())
    });
    match opened {
        Ok(pair) => Some(pair),
        Err(e) => {
            assert!(
                std::env::var_os("REGIA_LOOKS_REQUIRE_GPU").is_none(),
                "no GPU: {e}"
            );
            eprintln!("skipped: {e}");
            None
        }
    }
}

/// Runs `expr` (WGSL in terms of `x`, from 0 to 1) after `helpers` and returns one value per sample.
fn evaluate(device: &wgpu::Device, queue: &wgpu::Queue, helpers: &str, expr: &str) -> Vec<f32> {
    let source = format!(
        "{helpers}\n\
         @group(0) @binding(0) var<storage, read_write> oracle_out: array<f32>;\n\
         @compute @workgroup_size(64)\n\
         fn oracle_main(@builtin(global_invocation_id) id: vec3<u32>) {{\n\
             if (id.x >= {SAMPLES}u) {{ return; }}\n\
             let x = f32(id.x) / f32({SAMPLES}u - 1u);\n\
             oracle_out[id.x] = {expr};\n\
         }}\n"
    );
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("oracle"),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("oracle"),
        layout: None,
        module: &module,
        entry_point: Some("oracle_main"),
        compilation_options: Default::default(),
        cache: None,
    });
    let bytes = u64::from(SAMPLES) * 4;
    let out = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("oracle out"),
        size: bytes,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let read = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("oracle read"),
        size: bytes,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("oracle"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: out.as_entire_binding(),
        }],
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("oracle"),
    });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("oracle"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(SAMPLES.div_ceil(64), 1, 1);
    }
    encoder.copy_buffer_to_buffer(&out, 0, &read, 0, bytes);
    queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    read.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    rx.recv().expect("map callback").expect("map");
    let data = read.slice(..).get_mapped_range().expect("mapped range");
    let values = bytemuck::cast_slice::<u8, f32>(&data).to_vec();
    drop(data);
    read.unmap();
    values
}

/// The pit shadow is the share of the studio a blasted surface sees: in 0..1 for every roughness,
/// 1 when smooth, falling as the finish gets rougher. Before the clamp a roughness above about 0.83
/// gave a negative share, which subtracted the studio and drew a black metal.
#[test]
fn the_pit_shadow_is_a_share_for_every_roughness() {
    let Some((device, queue)) = device() else {
        return;
    };
    let pits = evaluate(&device, &queue, METAL_GRAIN, "looks_pit_shadow(x)");
    assert_eq!(pits.len(), SAMPLES as usize);
    for (i, p) in pits.iter().enumerate() {
        let roughness = i as f32 / (SAMPLES - 1) as f32;
        assert!((0.0..=1.0).contains(p), "roughness {roughness}: pit shadow {p}");
    }
    assert!((pits[0] - 1.0).abs() < 1e-6, "smooth: {}", pits[0]);
    // The built-in sandblast (roughness 0.55) keeps the share it had.
    assert!((pits[55] - (1.0 - 1.2 * 0.55)).abs() < 1e-5, "0.55: {}", pits[55]);
    assert!(
        pits.windows(2).all(|w| w[1] <= w[0]),
        "never brighter when rougher: {pits:?}"
    );
}

/// The grain fade is a weight in 0..1 for every footprint: 1 for grains that cover pixels, 0 for
/// grains far below one.
#[test]
fn the_grain_fade_is_a_weight() {
    let Some((device, queue)) = device() else {
        return;
    };
    // Footprints from 0 to 4 pixels per grain.
    let fade = evaluate(&device, &queue, METAL_GRAIN, "looks_grain_fade(x * 4.0)");
    assert!(fade.iter().all(|f| (0.0..=1.0).contains(f)), "{fade:?}");
    assert!((fade[0] - 1.0).abs() < 1e-6);
    assert!(fade[SAMPLES as usize - 1].abs() < 1e-6);
}
