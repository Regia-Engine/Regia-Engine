// SPDX-License-Identifier: MPL-2.0
//! The SSAO pipelines of ADR-1491, on the GPU (lavapipe in CI).
//!
//! `ssao.wgsl` is built with the `BENT_NORMALS` pipeline constant off and on. Off, the kernel
//! (`cs_main`) and the per-tap blur (`cs_blur`) neither sum the bent-normal lean nor write layer 1.
//! On, the kernel sums it and the blur is `cs_blur_tiled`, which loads each workgroup's 11×11 texels
//! (depth, AO and reflection visibility, decoded lean) once into workgroup memory instead of
//! once per tap. This test runs each pipeline `SsaoPass` uses next to the v0.2.2 kernel and blur
//! (kept here verbatim as `cs_main_reference` and `cs_blur_reference`, composed into the same
//! module) on the same depth buffer, raw AO texels and parameters, and compares the outputs byte
//! for byte (within one level on Metal, whose fast math may reassociate the sums, ADR-3287). Cases: the half-resolution AO target (the shift in `depth_texel_of`), a depth size
//! that is not twice the AO size (the integer path), partial workgroups at the right and bottom
//! edges, sky texels, a depth step (the bilateral weights), bent normals on and off, reflection
//! occlusion on and off. With bent normals off layer 1 is left alone (nothing reads it: the
//! forward pass tests the same switch); v0.2.2 wrote a zero lean there.
//!
//! No golden: the new and the old kernels are compared with each other.

use std::borrow::Cow;
use std::sync::{Mutex, MutexGuard, PoisonError};

use regia_render::RenderDevice;

const SSAO: &str = include_str!("../shaders/ssao.wgsl");

/// The per-tap blur `cs_blur` was before ADR-1491 (v0.2.2), with its two layer stores spelled
/// out (`store_texel` then always wrote layer 1).
const REFERENCE_BLUR: &str = r"
@compute @workgroup_size(8, 8)
fn cs_blur_reference(@builtin(global_invocation_id) gid: vec3<u32>) {
    let size = vec2<i32>(textureDimensions(out_tex));
    let pix = vec2<i32>(gid.xy);
    if (pix.x >= size.x || pix.y >= size.y) {
        return;
    }
    let depth_size = vec2<i32>(textureDimensions(depth_tex));
    let centre = view_position(depth_texel_of(pix, size, depth_size), depth_size).z;
    var sum = vec3<f32>(0.0);
    var lean_sum = vec3<f32>(0.0);
    var weight = 0.0;
    var refl_weight = 0.0;
    let bent_on = params.noise.z > 0.5;
    for (var y: i32 = -2; y <= 1; y = y + 1) {
        for (var x: i32 = -2; x <= 1; x = x + 1) {
            let q = clamp(pix + vec2<i32>(x, y), vec2<i32>(0), size - vec2<i32>(1));
            let z = view_position(depth_texel_of(q, size, depth_size), depth_size).z;
            let w = 1.0 / (1e-3 + abs(z - centre) / max(abs(centre) * 0.02, 1e-3));
            let t = textureLoad(raw_tex, q, 0, 0).rgb;
            let wr = select(0.0, w, traced(q) || params.noise.y < 0.5);
            sum += vec3<f32>(t.r * w, t.gb * wr);
            if (bent_on) {
                lean_sum += bent_decode(textureLoad(raw_tex, q, 1, 0).rgb) * w;
            }
            weight += w;
            refl_weight += wr;
        }
    }
    let ao = sum.r / max(weight, 1e-6);
    let refl = select(vec2<f32>(1.0), sum.gb / max(refl_weight, 1e-6), refl_weight > 0.0);
    textureStore(out_tex, pix, 0, vec4<f32>(ao, refl, 1.0));
    textureStore(out_tex, pix, 1, vec4<f32>(bent_encode(lean_sum / max(weight, 1e-6)), 1.0));
}
";

/// The SSAO kernel `cs_main` was before ADR-1491 (v0.2.2, verbatim but for its name, the edge rule of ADR-1626 and the
/// store helper: `store_texel` then always wrote both layers).
const REFERENCE_MAIN: &str = r"
fn store_both_reference(p: vec2<i32>, ao_refl: vec3<f32>, lean: vec3<f32>) {
    textureStore(out_tex, p, 0, vec4<f32>(ao_refl, 1.0));
    textureStore(out_tex, p, 1, vec4<f32>(bent_encode(lean), 1.0));
}

@compute @workgroup_size(8, 8)
fn cs_main_reference(@builtin(global_invocation_id) gid: vec3<u32>) {
    let size = vec2<u32>(textureDimensions(out_tex));
    if (gid.x >= size.x || gid.y >= size.y) {
        return;
    }
    let depth_size = vec2<i32>(textureDimensions(depth_tex));
    let coord = depth_texel_of(vec2<i32>(gid.xy), vec2<i32>(size), depth_size);
    let d = textureLoad(depth_tex, coord, 0);
    if (d >= 1.0) {
        store_both_reference(vec2<i32>(gid.xy), vec3<f32>(1.0), vec3<f32>(0.0));
        return;
    }
    // ADR-1626: the last depth column and row take no occlusion by coordinate (the old kernel got the
    // same from an exact-zero difference, which not every compiler rounds to).
    if (any(coord + vec2<i32>(1) >= depth_size)) {
        store_both_reference(vec2<i32>(gid.xy), vec3<f32>(1.0), vec3<f32>(0.0));
        return;
    }
    let p = view_position(coord, depth_size);
    // Normal from the smaller of the two one-sided differences per axis, so a
    // silhouette edge does not tilt the normal of the surface in front of it.
    let px0 = view_position(coord - vec2<i32>(1, 0), depth_size);
    let px1 = view_position(coord + vec2<i32>(1, 0), depth_size);
    let py0 = view_position(coord - vec2<i32>(0, 1), depth_size);
    let py1 = view_position(coord + vec2<i32>(0, 1), depth_size);
    let dx = select(px1 - p, p - px0, abs(p.z - px0.z) < abs(px1.z - p.z));
    let dy = select(py1 - p, p - py0, abs(p.z - py0.z) < abs(py1.z - p.z));
    let c = cross(dy, dx);
    let c_len2 = dot(c, c);
    if (!(c_len2 > 1e-24)) {
        // Degenerate depth neighbourhood (far-plane precision): no occlusion.
        store_both_reference(vec2<i32>(gid.xy), vec3<f32>(1.0), vec3<f32>(0.0));
        return;
    }
    var n = c * inverseSqrt(c_len2);
    if (dot(n, -p) < 0.0) {
        n = -n;
    }

    let radius = params.screen.z;
    let samples = u32(clamp(params.screen.w, 4.0, 32.0));
    // World radius projected into AO-target pixels at this depth.
    let radius_px = clamp(radius * params.proj.y * 0.5 * f32(size.y) / max(-p.z, 1e-3), 1.5, params.mode.w);
    let to_depth = vec2<f32>(depth_size) / vec2<f32>(size);
    let rotation = ign(vec2<f32>(gid.xy) + 5.588238 * params.noise.x) * 6.2831853;
    let bias = params.mode.y;
    let split = probe_split_at(coord, depth_size);
    var occlusion = 0.0;
    // The same obscurance weighted for the brick's and the terrain layer's share of the light.
    var occ_brick = 0.0;
    var occ_terrain = 0.0;
    // Without a split (rest = 1, shares 0) the whole kernel.
    let rest = max(1.0 - split.x - split.y, 0.0);
    // Σ obscurance × the share no probe holds, and the same along each sample's direction.
    var lean_share = 0.0;
    var lean_dir = vec3<f32>(0.0);
    for (var i: u32 = 0u; i < samples; i = i + 1u) {
        let t = (f32(i) + 0.5) / f32(samples);
        let angle = f32(i) * 2.399963 + rotation; // golden angle spiral
        let offset = vec2<f32>(cos(angle), sin(angle)) * radius_px * t;
        let s_coord = vec2<i32>((vec2<f32>(gid.xy) + vec2<f32>(0.5) + offset) * to_depth);
        let s = view_position(s_coord, depth_size);
        let v = s - p;
        let vv = dot(v, v);
        let falloff = clamp(1.0 - vv / (radius * radius), 0.0, 1.0);
        let cos_rise = dot(v, n) * inverseSqrt(max(vv, 1e-8));
        let o = max(cos_rise - bias, 0.0) * falloff;
        occlusion += o;
        let height = max(dot(v, n), 0.0);
        let keep_brick = probe_ao_keep(height, split.z);
        let keep_terrain = probe_ao_keep(height, split.w);
        occ_brick += o * keep_brick;
        occ_terrain += o * keep_terrain;
        let kept = o * (rest + split.x * keep_brick + split.y * keep_terrain);
        lean_share += kept;
        lean_dir += v * (kept * inverseSqrt(max(vv, 1e-8)));
    }
    // Without a split this is the whole kernel's value exactly.
    let n_samples = f32(samples);
    let ao = rest * clamp(1.0 - occlusion / n_samples * params.mode.z, 0.0, 1.0)
        + split.x * clamp(1.0 - occ_brick / n_samples * params.mode.z, 0.0, 1.0)
        + split.y * clamp(1.0 - occ_terrain / n_samples * params.mode.z, 0.0, 1.0);
    var refl = vec2<f32>(1.0);
    // Traced on a fixed checkerboard of AO texels (`traced`); the blur fills the others.
    if (params.noise.y > 0.5 && traced(vec2<i32>(gid.xy))) {
        refl = reflection_visibility(p, n, rotation, depth_size);
    }
    var lean = vec3<f32>(0.0);
    if (params.noise.z > 0.5) {
        let k = params.mode.z / n_samples;
        lean = to_world(bent_normal_lean(n, lean_dir * k, lean_share * k));
    }
    store_both_reference(vec2<i32>(gid.xy), vec3<f32>(ao, refl), lean);
}
";

/// Fills the depth buffer: a fullscreen triangle whose fragments write a hashed depth (sky
/// texels at 1, two depth planes with a step at x = 30, a small per-texel jitter).
const DEPTH_FILL: &str = r"
@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) p: vec4<f32>) -> @builtin(frag_depth) f32 {
    let c = vec2<u32>(p.xy);
    let h = (c.x * 73856093u) ^ (c.y * 19349663u);
    if (h % 13u == 0u) {
        return 1.0;
    }
    let base = select(0.93, 0.985, c.x > 30u);
    return base + f32(h % 997u) * 1e-5;
}
";

/// The rest value written into both outputs before a blur: a layer the blur skips keeps it.
const SENTINEL: u8 = 7;

/// One test at a time: each opens its own device (ADR-1438: concurrent device churn
/// live-locks the NVIDIA driver).
static GPU: Mutex<()> = Mutex::new(());

fn device() -> (MutexGuard<'static, ()>, RenderDevice) {
    let guard = GPU.lock().unwrap_or_else(PoisonError::into_inner);
    let device = RenderDevice::headless().expect("headless GPU (lavapipe in CI)");
    (guard, device)
}

/// One blur case.
#[derive(Clone, Copy, Debug)]
struct Case {
    /// The AO target (both blur outputs and the raw input).
    size: (u32, u32),
    /// The depth buffer.
    depth: (u32, u32),
    /// `noise.y`: reflection occlusion (G, B average only the traced texels).
    reflection: bool,
    /// `noise.z`: bent normals (the lean in layer 1).
    bent: bool,
}

/// `SsaoParams` (`ssao.wgsl`): eight 16-byte rows.
fn params(case: Case) -> Vec<u8> {
    let (w, h) = case.size;
    let rows: [[f32; 4]; 8] = [
        [w as f32, h as f32, 0.75, 12.0],
        [1.0, 0.1, 1.0, (h as f32 * 0.1).max(4.0)],
        [1.1, 1.6, 0.1, 300.0],
        [
            0.0,
            if case.reflection { 1.0 } else { 0.0 },
            if case.bent { 1.0 } else { 0.0 },
            0.0,
        ],
        // probe: u32 zeros (no split) have the same bits as f32 zeros.
        [0.0; 4],
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
    ];
    rows.iter().flatten().flat_map(|v| v.to_le_bytes()).collect()
}

/// Deterministic raw AO texels for both layers (a 32-bit LCG; codes 1..255 so the lean decodes
/// to every value the kernel can write).
fn raw_texels(case: Case) -> Vec<u8> {
    let (w, h) = case.size;
    let mut state = 0x2545_f491_u32 ^ w.wrapping_mul(31) ^ h.wrapping_mul(17);
    (0..w * h * 4 * 2)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((state >> 24) as u8).max(1)
        })
        .collect()
}

fn two_layer_texture(
    device: &RenderDevice,
    label: &str,
    size: (u32, u32),
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
    device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 2,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage,
        view_formats: &[],
    })
}

fn write_layers(device: &RenderDevice, texture: &wgpu::Texture, size: (u32, u32), bytes: &[u8]) {
    device.queue.write_texture(
        texture.as_image_copy(),
        bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size.0 * 4),
            rows_per_image: Some(size.1),
        },
        wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 2,
        },
    );
}

fn array_view(texture: &wgpu::Texture, usage: wgpu::TextureUsages) -> wgpu::TextureView {
    texture.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        usage: Some(usage),
        ..Default::default()
    })
}

/// The depth buffer of `case`, filled by `DEPTH_FILL`.
fn depth_texture(device: &RenderDevice, case: Case) -> wgpu::Texture {
    let d = &device.device;
    let texture = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("ssao blur tile depth"),
        size: wgpu::Extent3d {
            width: case.depth.0,
            height: case.depth.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth32Float,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("ssao blur tile depth fill"),
        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(DEPTH_FILL)),
    });
    let pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("ssao blur tile depth fill"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs_main"),
            targets: &[],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::Always),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("ssao blur tile depth fill"),
    });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("ssao blur tile depth fill"),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&pipeline);
        pass.draw(0..3, 0..1);
    }
    device.queue.submit([encoder.finish()]);
    texture
}

/// Both layers of `texture` (`size`), tightly packed: layer 0 then layer 1, rows of RGBA8.
fn read_layers(device: &RenderDevice, texture: &wgpu::Texture, size: (u32, u32)) -> Vec<u8> {
    let d = &device.device;
    let row = size.0 * 4;
    let padded = row.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let bytes = u64::from(padded) * u64::from(size.1) * 2;
    let readback = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("ssao blur tile readback"),
        size: bytes,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("ssao blur tile readback"),
    });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(size.1),
            },
        },
        wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 2,
        },
    );
    device.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    d.poll(wgpu::PollType::wait_indefinitely())
        .expect("poll readback");
    rx.recv().expect("map callback").expect("map readback");
    let mapped = readback.slice(..).get_mapped_range().expect("mapped range");
    let out: Vec<u8> = mapped
        .chunks(padded as usize)
        .flat_map(|line| &line[..row as usize])
        .copied()
        .collect();
    drop(mapped);
    readback.unmap();
    out
}

/// Runs `entry` of the composed module (`ssao.wgsl` + `REFERENCE_BLUR` + `REFERENCE_MAIN`) over
/// `case`, with the `BENT_NORMALS` pipeline constant set when `bent_constant` (as `SsaoPass` builds
/// its bent-normal pipelines), and returns both output layers. A kernel entry (`cs_main*`) binds
/// an empty probe split; a blur binds the raw texels.
fn run(
    device: &RenderDevice,
    module: &wgpu::ShaderModule,
    entry: &str,
    case: Case,
    depth: &wgpu::Texture,
    bent_constant: bool,
) -> Vec<u8> {
    const BENT: &[(&str, f64)] = &[("BENT_NORMALS", 1.0)];
    let d = &device.device;
    let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(entry),
        layout: None,
        module,
        entry_point: Some(entry),
        compilation_options: wgpu::PipelineCompilationOptions {
            constants: if bent_constant { BENT } else { &[] },
            ..Default::default()
        },
        cache: None,
    });
    let uniform = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("ssao blur tile params"),
        size: 128,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    device.queue.write_buffer(&uniform, 0, &params(case));
    let raw = two_layer_texture(
        device,
        "ssao blur tile raw",
        case.size,
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
    );
    write_layers(device, &raw, case.size, &raw_texels(case));
    let out = two_layer_texture(
        device,
        "ssao blur tile out",
        case.size,
        wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
    );
    let texels = (case.size.0 * case.size.1 * 4 * 2) as usize;
    write_layers(device, &out, case.size, &vec![SENTINEL; texels]);
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
    let out_view = array_view(&out, wgpu::TextureUsages::STORAGE_BINDING);
    let raw_view = array_view(&raw, wgpu::TextureUsages::TEXTURE_BINDING);
    // No probe split (`probe.y` = 0 keeps it unread), as `SsaoPass`'s placeholder.
    let no_split = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("ssao blur tile no split"),
        size: 16,
        usage: wgpu::BufferUsages::STORAGE,
        mapped_at_creation: false,
    });
    let input = if entry.starts_with("cs_main") {
        wgpu::BindGroupEntry {
            binding: 4,
            resource: no_split.as_entire_binding(),
        }
    } else {
        wgpu::BindGroupEntry {
            binding: 3,
            resource: wgpu::BindingResource::TextureView(&raw_view),
        }
    };
    let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("ssao blur tile"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&depth_view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(&out_view),
            },
            input,
        ],
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("ssao blur tile"),
    });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some(entry),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(case.size.0.div_ceil(8), case.size.1.div_ceil(8), 1);
    }
    device.queue.submit([encoder.finish()]);
    read_layers(device, &out, case.size)
}

/// The levels a channel may differ by on `device`'s backend: none, except on Metal (ADR-3287).
/// wgpu builds Metal libraries with the default compile options, whose fast math lets the
/// compiler reassociate a sum; the tiled blur and its per-tap reference add the same taps in
/// two code shapes, and on the Apple M5 one channel of one texel rounds a level apart (blue 16
/// against 15 since v0.2.4). Vulkan adapters (lavapipe, NVIDIA, Intel) keep the exact check.
fn tolerance(device: &RenderDevice) -> u8 {
    u8::from(device.capabilities.backend == wgpu::Backend::Metal)
}

/// The first texel (layer, x, y) where a channel of `a` and `b` differs by more than `tol`
/// levels, with both values.
fn first_difference(a: &[u8], b: &[u8], size: (u32, u32), tol: u8) -> Option<String> {
    let (texels, _) = a.as_chunks::<4>();
    let (other, _) = b.as_chunks::<4>();
    let per_layer = (size.0 * size.1) as usize;
    let apart = |x: &[u8; 4], y: &[u8; 4]| x.iter().zip(y).any(|(p, q)| p.abs_diff(*q) > tol);
    texels.iter().zip(other).position(|(x, y)| apart(x, y)).map(|i| {
        let layer = i / per_layer;
        let at = i % per_layer;
        format!(
            "layer {layer} texel ({}, {}): {:?} vs {:?}",
            at % size.0 as usize,
            at / size.0 as usize,
            texels[i],
            other[i]
        )
    })
}

#[test]
fn the_kernel_and_blur_pipelines_write_the_reference_bytes() {
    let (_gpu, device) = device();
    let tol = tolerance(&device);
    // Composed, as `SsaoPass` builds the kernel: `ssao.wgsl` holds a shader-def variant (ADR-2075).
    let composed = regia_render::compose_source(
        &format!("{SSAO}\n{REFERENCE_BLUR}\n{REFERENCE_MAIN}"),
        "ssao.wgsl",
        std::collections::HashMap::new(),
    )
    .expect("ssao + reference compose");
    let module = device.device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("ssao + reference kernel and blur"),
        source: wgpu::ShaderSource::Naga(Cow::Owned(composed)),
    });
    let mut cases = Vec::new();
    // Half resolution (the shift), a depth size that is not twice the AO size (the integer
    // path), and exact workgroups; partial workgroups on the first two (37 × 23). ADR-1560: a
    // single partial row of workgroups (129 × 7), and a depth that is neither twice nor the same
    // ratio on both axes (53 × 41 over 160 × 90); the RTX 5090 and Mesa's Intel compiler wrote
    // other bytes there too while the blurs' arithmetic differed.
    for (size, depth) in [
        ((37, 23), (74, 46)),
        ((37, 23), (75, 47)),
        ((16, 16), (32, 32)),
        ((129, 7), (258, 14)),
        ((53, 41), (160, 90)),
    ] {
        for reflection in [false, true] {
            for bent in [false, true] {
                cases.push(Case {
                    size,
                    depth,
                    reflection,
                    bent,
                });
            }
        }
    }
    for case in cases {
        let depth = depth_texture(&device, case);
        // The pipelines `SsaoPass::dispatch` runs for this case, each against its v0.2.2 form.
        let blur = if case.bent { "cs_blur_tiled" } else { "cs_blur" };
        for (entry, reference) in [("cs_main", "cs_main_reference"), (blur, "cs_blur_reference")] {
            let new = run(&device, &module, entry, case, &depth, case.bent);
            let old = run(&device, &module, reference, case, &depth, false);
            let layer = (case.size.0 * case.size.1 * 4) as usize;
            assert_eq!(
                first_difference(&new[..layer], &old[..layer], case.size, tol),
                None,
                "layer 0, {entry} {case:?}"
            );
            if case.bent {
                assert_eq!(
                    first_difference(&new, &old, case.size, tol),
                    None,
                    "{entry} {case:?}"
                );
            } else {
                // Bent normals off: layer 1 untouched (v0.2.2 wrote the zero lean, 128).
                assert!(
                    new[layer..].iter().all(|&b| b == SENTINEL),
                    "layer 1 written, {entry} {case:?}"
                );
                let (lean, _) = old[layer..].as_chunks::<4>();
                assert!(
                    lean.iter().all(|t| t[..3] == [128, 128, 128]),
                    "v0.2.2 writes a zero lean, {entry} {case:?}"
                );
            }
            // Every texel was written: layer 0's alpha is 1 everywhere (the sentinel is 7).
            let (written, _) = new[..layer].as_chunks::<4>();
            assert!(written.iter().all(|t| t[3] == 255), "{entry} {case:?}");
        }
    }
}
