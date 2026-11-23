// SPDX-License-Identifier: MPL-2.0
//! Any-hit alpha test for a masked sun-shadow caster (ADR-4557).
//!
//! A card at y = 2 covers the central 32×32 texels of a 64² ground (x, z in [-2, 2]). The
//! software walk (`bvh_walk.wgsl`, dispatched as the sun-trace BVH shader) and, when the device
//! has ray queries, `sun_trace_ray_query.wgsl` both run. Coverage is the engine's masked-caster
//! proxy (`MASKED_COVERAGE`, one half): there is no mask texture on the trace, so the any-hit
//! does not fetch one. The lit fraction of the card's footprint is within
//! [`LIT_FRACTION_EPSILON`] of the open fraction 0.5. An opaque card's covered texel count is
//! pinned at 1024. A texel the card misses stays lit.
//!
//! Lavapipe advertises `VK_KHR_ray_query`. If this process's wgpu device refuses
//! `EXPERIMENTAL_RAY_QUERY`, the ray-query test fails with the ignore reason; mark it
//! `#[ignore = "wgpu refused EXPERIMENTAL_RAY_QUERY: …"]` and keep the software walk.

use regia_render::RenderDevice;
use regia_render::hwrt::casters::coverage_custom_data;
use regia_render::hwrt::sun_trace::{SUN_TRACE_FORMAT, SUN_TRACE_GROUP, sun_trace_groups};
use regia_render::hwrt::{
    AccelInstance, AccelMesh, GpuAccel, GpuAccelMesh, MASKED_COVERAGE, SUN_TRACE_BVH_WGSL,
    SUN_TRACE_RAY_QUERY_WGSL, ShadowBvh,
};
use wgpu::util::DeviceExt as _;

/// Ground resolution. 64×4 bytes per row is the copy alignment, so readback has no padding.
const N: u32 = 64;

/// Absolute tolerance on the lit fraction of the masked card.
///
/// Over the 1024 texels the card covers, a fair coin has standard deviation 16/1024 ≈ 0.0156.
/// 0.08 is about five of those: a solid card (lit fraction 0) or a missing card (lit fraction 1)
/// cannot pass. The holes are the coverage hash, not a stored checker image.
const LIT_FRACTION_EPSILON: f32 = 0.08;

/// Pinned covered texels of the opaque card (the central 32² of the 64² ground).
const OPAQUE_COVERED_TEXELS: usize = 1024;

fn card() -> Vec<[[f32; 3]; 3]> {
    vec![
        [[-1.0, 2.0, -1.0], [1.0, 2.0, -1.0], [1.0, 2.0, 1.0]],
        [[-1.0, 2.0, -1.0], [1.0, 2.0, 1.0], [-1.0, 2.0, 1.0]],
    ]
}

fn inv_view_proj() -> [[f32; 4]; 4] {
    // Column-major. ndc (x, y, *, 1) maps to world (2x, 0, 2y): the ground, x and z in [-2, 2].
    [
        [2.0, 0.0, 0.0, 0.0],
        [0.0, 0.0, 2.0, 0.0],
        [0.0, 0.0, 0.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SunParams {
    inv_view_proj: [[f32; 4]; 4],
    view_proj: [[f32; 4]; 4],
    to_sun: [f32; 4],
    eye: [f32; 4],
    viewport: [f32; 4],
    tune: [f32; 4],
    counts: [u32; 4],
}

fn params(node_count: u32) -> SunParams {
    SunParams {
        inv_view_proj: inv_view_proj(),
        view_proj: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
        to_sun: [0.0, 1.0, 0.0, 100.0],
        eye: [0.0, 10.0, 0.0, 0.1],
        viewport: [0.0, 0.0, N as f32, N as f32],
        // Offset 0, so the ray origin is the reconstructed ground point.
        tune: [0.0, 0.0, 30.0, 0.0],
        counts: [node_count, 0, 0, 0],
    }
}

struct Ground {
    depth: wgpu::TextureView,
    out: wgpu::Texture,
    out_view: wgpu::TextureView,
}

fn ground(device: &wgpu::Device, queue: &wgpu::Queue) -> Ground {
    let size = wgpu::Extent3d {
        width: N,
        height: N,
        depth_or_array_layers: 1,
    };
    let depth = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("anyhit depth"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    // Depth formats are render attachments, not Queue::write_texture destinations in wgpu 30.
    let attachment = depth.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("anyhit depth clear"),
    });
    {
        let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("anyhit depth clear"),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &attachment,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(0.5),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
    }
    queue.submit([encoder.finish()]);
    let out = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("anyhit visibility"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: SUN_TRACE_FORMAT,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = |texture: &wgpu::Texture, usage: wgpu::TextureUsages| {
        texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("anyhit"),
            format: Some(if usage == wgpu::TextureUsages::TEXTURE_BINDING {
                wgpu::TextureFormat::Depth32Float
            } else {
                SUN_TRACE_FORMAT
            }),
            usage: Some(usage),
            ..Default::default()
        })
    };
    Ground {
        depth: view(&depth, wgpu::TextureUsages::TEXTURE_BINDING),
        out_view: view(&out, wgpu::TextureUsages::STORAGE_BINDING),
        out,
    }
}

fn read_rgba(device: &wgpu::Device, queue: &wgpu::Queue, texture: &wgpu::Texture) -> Vec<u8> {
    let padded = (N * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let bytes = u64::from(padded) * u64::from(N);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("anyhit readback"),
        size: bytes,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("anyhit readback"),
    });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(N),
            },
        },
        wgpu::Extent3d {
            width: N,
            height: N,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll readback");
    rx.recv().expect("map callback").expect("map readback");
    let mapped = readback.slice(..).get_mapped_range().expect("mapped range");
    let tight = (N * 4) as usize;
    let mut out = Vec::with_capacity(tight * N as usize);
    for row in 0..N as usize {
        let start = row * padded as usize;
        out.extend_from_slice(&mapped[start..start + tight]);
    }
    drop(mapped);
    readback.unmap();
    out
}

/// `(shadowed inside, lit inside, lit outside)`.
fn tally(rgba: &[u8]) -> (usize, usize, usize) {
    let mut shadowed = 0usize;
    let mut lit_in = 0usize;
    let mut lit_out = 0usize;
    for j in 0..N {
        for i in 0..N {
            let px = ((j * N + i) * 4) as usize;
            let (r, g, b, a) = (rgba[px], rgba[px + 1], rgba[px + 2], rgba[px + 3]);
            assert_eq!(
                (g, b, a),
                (r, r, 255),
                "pixel ({i}, {j}) is not a visibility texel"
            );
            assert!(
                r == 0 || r == 255,
                "pixel ({i}, {j}) visibility {r} is not 0 or 255"
            );
            let ndc_x = (i as f32 + 0.5) / N as f32 * 2.0 - 1.0;
            let ndc_y = 1.0 - (j as f32 + 0.5) / N as f32 * 2.0;
            let inside = (ndc_x * 2.0).abs() < 1.0 && (ndc_y * 2.0).abs() < 1.0;
            match (inside, r == 0) {
                (true, true) => shadowed += 1,
                (true, false) => lit_in += 1,
                (false, false) => lit_out += 1,
                (false, true) => panic!("a miss at ({i}, {j}) was shadowed"),
            }
        }
    }
    (shadowed, lit_in, lit_out)
}

fn assert_open_fraction(shadowed: usize, lit_in: usize) {
    let footprint = shadowed + lit_in;
    assert_eq!(footprint, OPAQUE_COVERED_TEXELS);
    let open = lit_in as f32 / footprint as f32;
    eprintln!("masked card: {shadowed} shadowed, {lit_in} lit, open fraction {open:.6}");
    assert!(
        (open - (1.0 - MASKED_COVERAGE)).abs() < LIT_FRACTION_EPSILON,
        "lit fraction {open} (open fraction {}), epsilon {LIT_FRACTION_EPSILON}, {shadowed} shadowed of {footprint}",
        1.0 - MASKED_COVERAGE
    );
}

fn storage_buffer(device: &wgpu::Device, label: &str, contents: &[u8]) -> wgpu::Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents,
        usage: wgpu::BufferUsages::STORAGE,
    })
}

fn bvh_pipeline(device: &wgpu::Device) -> (wgpu::BindGroupLayout, wgpu::ComputePipeline) {
    let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty,
        count: None,
    };
    let storage = |binding| {
        entry(
            binding,
            wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
        )
    };
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("anyhit bvh"),
        entries: &[
            entry(
                0,
                wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
            ),
            entry(
                1,
                wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
            ),
            entry(
                2,
                wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: SUN_TRACE_FORMAT,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
            ),
            storage(3),
            storage(4),
        ],
    });
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("anyhit bvh"),
        source: wgpu::ShaderSource::Wgsl(SUN_TRACE_BVH_WGSL.into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("anyhit bvh"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("anyhit bvh"),
        layout: Some(&pipeline_layout),
        module: &module,
        entry_point: Some("trace_sun"),
        compilation_options: Default::default(),
        cache: None,
    });
    (layout, pipeline)
}

fn dispatch_bvh(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    layout: &wgpu::BindGroupLayout,
    pipeline: &wgpu::ComputePipeline,
    ground: &Ground,
    bvh: &ShadowBvh,
) -> Vec<u8> {
    let nodes = storage_buffer(device, "anyhit nodes", bytemuck::cast_slice(bvh.nodes()));
    let tris = storage_buffer(device, "anyhit tris", bytemuck::cast_slice(bvh.triangles()));
    let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("anyhit params"),
        contents: bytemuck::bytes_of(&params(bvh.nodes().len() as u32)),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("anyhit bvh"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&ground.depth),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(&ground.out_view),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: nodes.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: tris.as_entire_binding(),
            },
        ],
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("anyhit bvh"),
    });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("anyhit bvh"),
            timestamp_writes: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &bind, &[]);
        let (x, y) = sun_trace_groups(N, N);
        pass.dispatch_workgroups(x, y, 1);
    }
    queue.submit([encoder.finish()]);
    read_rgba(device, queue, &ground.out)
}

#[test]
fn both_shaders_name_the_any_hit_and_the_no_texture_limit() {
    assert!(
        SUN_TRACE_BVH_WGSL.contains("fn any_hit("),
        "the software walk names any_hit"
    );
    assert!(
        SUN_TRACE_RAY_QUERY_WGSL.contains("fn any_hit("),
        "the ray-query tracer names any_hit"
    );
    assert!(
        SUN_TRACE_BVH_WGSL.contains("No mask texture")
            && SUN_TRACE_RAY_QUERY_WGSL.contains("no mask texture"),
        "both tracers document that a coverage-only mask is not a texture fetch"
    );
    assert_eq!(SUN_TRACE_GROUP, 8);
}

#[test]
fn a_masked_card_on_the_software_walk_matches_its_open_fraction() {
    let device = RenderDevice::shared_headless().expect("lavapipe");
    let d = &device.device;
    let ground = ground(d, &device.queue);
    let (layout, pipeline) = bvh_pipeline(d);
    let tris = card();

    let opaque = ShadowBvh::build(&tris).expect("opaque bvh");
    let rgba = dispatch_bvh(d, &device.queue, &layout, &pipeline, &ground, &opaque);
    let (shadowed, lit_in, lit_out) = tally(&rgba);
    assert_eq!(lit_in, 0, "an opaque card leaves no hole in its footprint");
    assert_eq!(
        shadowed, OPAQUE_COVERED_TEXELS,
        "pinned covered texel count of the opaque card"
    );
    assert_eq!(lit_out, (N * N) as usize - OPAQUE_COVERED_TEXELS);

    let masked =
        ShadowBvh::build_with_coverage(&tris, &[MASKED_COVERAGE, MASKED_COVERAGE]).expect("masked bvh");
    let rgba = dispatch_bvh(d, &device.queue, &layout, &pipeline, &ground, &masked);
    let (shadowed, lit_in, lit_out) = tally(&rgba);
    assert_eq!(
        lit_out,
        (N * N) as usize - OPAQUE_COVERED_TEXELS,
        "a miss stays a miss"
    );
    assert_open_fraction(shadowed, lit_in);

    // The GPU walk and the CPU reference answer the same texels.
    let mut cpu_shadowed = 0usize;
    for j in 0..N {
        for i in 0..N {
            let ndc_x = (i as f32 + 0.5) / N as f32 * 2.0 - 1.0;
            let ndc_y = 1.0 - (j as f32 + 0.5) / N as f32 * 2.0;
            let origin = [ndc_x * 2.0, 0.0, ndc_y * 2.0];
            let px = ((j * N + i) * 4) as usize;
            let gpu_hit = rgba[px] == 0;
            let cpu_hit = masked.occluded(origin, [0.0, 1.0, 0.0], 0.0, 100.0);
            assert_eq!(gpu_hit, cpu_hit, "texel ({i}, {j}) origin {origin:?}");
            cpu_shadowed += usize::from(cpu_hit);
        }
    }
    assert_eq!(cpu_shadowed, shadowed);
}

/// Rejection must continue traversal, including when the next hit is in a different leaf.
#[test]
fn the_software_walk_keeps_searching_after_a_rejected_mask() {
    let device = RenderDevice::shared_headless().expect("lavapipe");
    let ground = ground(&device.device, &device.queue);
    let (layout, pipeline) = bvh_pipeline(&device.device);
    let empty = ShadowBvh::build(&[]).expect("empty bvh");
    let rgba = dispatch_bvh(&device.device, &device.queue, &layout, &pipeline, &ground, &empty);
    assert_eq!(tally(&rgba), (0, OPAQUE_COVERED_TEXELS, 3072));

    // A fraction that rounds to byte 255 is fully covered, like the CPU reference.
    let full = ShadowBvh::build_with_coverage(&card(), &[0.9999, 0.9999]).expect("full bvh");
    let rgba = dispatch_bvh(&device.device, &device.queue, &layout, &pipeline, &ground, &full);
    assert_eq!(tally(&rgba), (OPAQUE_COVERED_TEXELS, 0, 3072));

    // Eight masked triangles round to zero coverage, forcing every candidate to be rejected.
    // The ninth and tenth triangles form an opaque card farther toward the sun. More than
    // MAX_LEAF_TRIANGLES triangles forces the BVH to split: continuation crosses leaf nodes.
    let mut tris = card().repeat(4);
    let mut coverage = vec![0.001; tris.len()];
    tris.extend(card().into_iter().map(|tri| tri.map(|[x, y, z]| [x, y + 1.0, z])));
    coverage.extend([0.0, 0.0]);
    let stacked = ShadowBvh::build_with_coverage(&tris, &coverage).expect("stacked bvh");
    let rgba = dispatch_bvh(
        &device.device,
        &device.queue,
        &layout,
        &pipeline,
        &ground,
        &stacked,
    );
    assert_eq!(tally(&rgba), (OPAQUE_COVERED_TEXELS, 0, 3072));
}

fn ray_query_device() -> RenderDevice {
    let device = RenderDevice::headless_with_ray_queries().expect("headless ray-query device");
    eprintln!(
        "ray-query capability: adapter={:?}, enabled={}",
        device.adapter.get_info(),
        device
            .device
            .features()
            .contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY)
    );
    assert!(
        device
            .device
            .features()
            .contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY),
        "wgpu refused EXPERIMENTAL_RAY_QUERY on {}; mark this test \
         #[ignore = \"wgpu refused EXPERIMENTAL_RAY_QUERY\"]",
        device.adapter.get_info().name
    );
    device
}

fn trace_ray_query(device: &RenderDevice, coverage: Option<f32>) -> Vec<u8> {
    let opaque = coverage.is_none();
    let d = &device.device;
    let tris = card();
    let flat: Vec<[f32; 3]> = tris.iter().flatten().copied().collect();
    let vertices = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("anyhit blas"),
        contents: bytemuck::cast_slice(&flat),
        usage: wgpu::BufferUsages::BLAS_INPUT,
    });
    let meshes = [AccelMesh {
        key: 1,
        generation: 1,
        vertex_count: flat.len() as u32,
        index_count: 0,
    }];
    let buffers = [GpuAccelMesh {
        key: 1,
        vertex_buffer: &vertices,
        first_vertex: 0,
        vertex_stride: 12,
        index_buffer: None,
        first_index: 0,
        opaque,
    }];
    let identity = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let instances = [AccelInstance {
        mesh: 1,
        transform: identity,
        custom_data: coverage.map_or(0, coverage_custom_data),
        mask: 0xFF,
    }];
    let mut accel = GpuAccel::new(d).expect("ray-query device");
    let ground = ground(d, &device.queue);
    let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty,
        count: None,
    };
    let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("anyhit ray query"),
        entries: &[
            entry(
                0,
                wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
            ),
            entry(
                1,
                wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
            ),
            entry(
                2,
                wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: SUN_TRACE_FORMAT,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
            ),
            entry(
                3,
                wgpu::BindingType::AccelerationStructure { vertex_return: false },
            ),
        ],
    });
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("anyhit ray query"),
        source: wgpu::ShaderSource::Wgsl(SUN_TRACE_RAY_QUERY_WGSL.into()),
    });
    let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("anyhit ray query"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("anyhit ray query"),
        layout: Some(&pipeline_layout),
        module: &module,
        entry_point: Some("trace_sun"),
        compilation_options: Default::default(),
        cache: None,
    });
    let uniform = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("anyhit params"),
        contents: bytemuck::bytes_of(&params(0)),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("anyhit ray query"),
    });
    accel
        .build(d, &mut encoder, &meshes, &buffers, &instances)
        .expect("blas");
    let tlas = accel.tlas().expect("tlas");
    let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("anyhit ray query"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&ground.depth),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(&ground.out_view),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: tlas.as_binding(),
            },
        ],
    });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("anyhit ray query"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind, &[]);
        let (x, y) = sun_trace_groups(N, N);
        pass.dispatch_workgroups(x, y, 1);
    }
    device.queue.submit([encoder.finish()]);
    read_rgba(d, &device.queue, &ground.out)
}

#[test]
fn a_masked_card_on_the_ray_query_path_matches_its_open_fraction() {
    let device = ray_query_device();
    let rgba = trace_ray_query(&device, None);
    let (shadowed, lit_in, lit_out) = tally(&rgba);
    assert_eq!(lit_in, 0, "an opaque card leaves no hole in its footprint");
    assert_eq!(
        shadowed, OPAQUE_COVERED_TEXELS,
        "pinned covered texel count of the opaque card"
    );
    assert_eq!(lit_out, (N * N) as usize - OPAQUE_COVERED_TEXELS);

    let rgba = trace_ray_query(&device, Some(MASKED_COVERAGE));
    let (shadowed, lit_in, lit_out) = tally(&rgba);
    assert_eq!(
        lit_out,
        (N * N) as usize - OPAQUE_COVERED_TEXELS,
        "a miss stays a miss"
    );
    assert_open_fraction(shadowed, lit_in);

    let rgba = trace_ray_query(&device, Some(1.0));
    assert_eq!(tally(&rgba), (OPAQUE_COVERED_TEXELS, 0, 3072));
    let rgba = trace_ray_query(&device, Some(0.0));
    assert_eq!(tally(&rgba), (0, OPAQUE_COVERED_TEXELS, 3072));
}
