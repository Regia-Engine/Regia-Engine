// SPDX-License-Identifier: MPL-2.0
//! Does this adapter fetch a texel repeatably on the path that breaks out of a loop? (ADR-5713)
//!
//! The linear SSR march used to fetch the hit's colour inside its loop, right before `break`, and
//! on an RX 7900 XTX (RADV, Mesa 26.2.6) the engine's still frame came back as two alternating
//! images. This probe takes the engine out of the picture: a ray-cast room (mirror floor, striped
//! walls with one very bright patch, two cubes) is rendered into a fresh colour and depth target,
//! and the march runs over them as the frame's second pass, 32 times. It does that with the march
//! as it was before ADR-5713 (`BASE_MARCH`, verbatim) and with the current `shaders/ssr.wgsl`.
//!
//! - the current march must render every frame bit-identically, and must be live (it changes the
//!   image where the march is switched off);
//! - where the old march is repeatable too, both must render the same frame;
//! - how the old march behaves is printed, not asserted: it is the adapter's answer.
//!
//! Measured 2026-11-11 with the old march: an RX 7900 XTX (RADV NAVI31, Mesa 26.2.6) 9 to 31 of
//! 32 frames differ from the first (six launches, at most 21 to 88 of 6144 pixels); an RTX 5090
//! (615.71.09), lavapipe and a Steam Deck (RADV VANGOGH, Mesa 26.0.0-devel) none. The current
//! march: none on all four.

use regia_core::glam::Vec3;
use regia_core::glam::camera::rh::proj::directx::perspective;
use regia_core::glam::camera::rh::view::look_at_mat4;
use regia_render::RenderDevice;

const W: u32 = 96;
const H: u32 = 64;
const FRAMES: u32 = 32;
/// `SsrParams` in bytes: two matrices, three vectors, eight probe rows of two vectors.
const PARAM_FLOATS: usize = 16 + 16 + 4 + 4 + 4 + 8 * 8;

/// The scene pass: a closed room seen from inside, ray-cast per pixel, written as lit colour
/// (alpha = perceptual roughness, as the forward pass writes it) and depth.
const SCENE: &str = r"
struct Params {
    inv_view_proj: mat4x4<f32>,
    view_proj: mat4x4<f32>,
    camera: vec4<f32>,
    screen: vec4<f32>,
}
@group(0) @binding(0) var<uniform> params: Params;
struct Out {
    @location(0) color: vec4<f32>,
    @builtin(frag_depth) depth: f32,
}
fn slab(o: vec3<f32>, d: vec3<f32>, bmin: vec3<f32>, bmax: vec3<f32>) -> vec2<f32> {
    let inv = vec3<f32>(1.0) / d;
    let t0 = (bmin - o) * inv;
    let t1 = (bmax - o) * inv;
    let tn = min(t0, t1);
    let tf = max(t0, t1);
    return vec2<f32>(max(max(tn.x, tn.y), tn.z), min(min(tf.x, tf.y), tf.z));
}
@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}
@fragment fn fs(@builtin(position) pos: vec4<f32>) -> Out {
    let uv = pos.xy / params.screen.xy;
    let ndc = vec2<f32>(uv.x * 2.0 - 1.0, (1.0 - uv.y) * 2.0 - 1.0);
    let far = params.inv_view_proj * vec4<f32>(ndc, 1.0, 1.0);
    let o = params.camera.xyz;
    let d = normalize(far.xyz / far.w - o);
    // The room [-3, 3] x [0, 3] x [-3, 3] from inside: the ray leaves it at the far slab distance.
    var t = slab(o, d, vec3<f32>(-3.0, 0.0, -3.0), vec3<f32>(3.0, 3.0, 3.0)).y;
    let wall = o + d * t;
    var color = vec3<f32>(40.0, 42.0, 45.0) + 20.0 * vec3<f32>(fract(wall.x), fract(wall.z), 0.5);
    var rough = 0.02;
    if (wall.y > 0.001) {
        let stripe = step(0.5, fract(wall.x * 1.5 + wall.z * 1.5 + wall.y));
        color = mix(vec3<f32>(30.0, 60.0, 90.0), vec3<f32>(400.0, 380.0, 350.0), stripe);
        if (length(wall - vec3<f32>(0.5, 2.5, -3.0)) < 0.6) {
            color = vec3<f32>(3600.0, 4100.0, 5200.0);
        }
        rough = 0.9;
    }
    let red = slab(o, d, vec3<f32>(-1.75, 0.55, -1.75), vec3<f32>(-1.05, 1.25, -1.05));
    if (red.x < red.y && red.x > 0.0 && red.x < t) {
        t = red.x;
        color = vec3<f32>(160.0, 20.0, 16.0);
        rough = 0.7;
    }
    let blue = slab(o, d, vec3<f32>(1.05, 0.55, -1.75), vec3<f32>(1.75, 1.25, -1.05));
    if (blue.x < blue.y && blue.x > 0.0 && blue.x < t) {
        t = blue.x;
        color = vec3<f32>(16.0, 30.0, 160.0);
        rough = 0.7;
    }
    let clip = params.view_proj * vec4<f32>(o + d * t, 1.0);
    return Out(vec4<f32>(color, rough), clip.z / clip.w);
}
";

/// `shaders/ssr.wgsl` before ADR-5713 (commit 37d8d39db), verbatim: the hit's colour is fetched
/// inside the loop, on the path that breaks out of it.
const BASE_MARCH: &str = r#"// Screen-space reflection plus box-projected probe specular (M15b).
// McGuire and Mara, JCGT 2014, simplified to a linear screen march (no Hi-Z).
// Lagarde, SIGGRAPH 2012, for the probe parallax correction.

struct SsrProbe {
    center: vec4<f32>,
    // xyz = half extent, w = blend weight
    half: vec4<f32>,
}

struct SsrParams {
    inv_view_proj: mat4x4<f32>,
    view_proj: mat4x4<f32>,
    camera: vec4<f32>,
    // x = width, y = height, z = thickness, w = step length
    screen: vec4<f32>,
    // x = probe count, y = LDR-to-scene scale, z = SSR enabled, w = floor normal.y
    misc: vec4<f32>,
    probes: array<SsrProbe, 4>,
    // The local reflection captures' boxes (ADR-2860): center.w = blend weight (0 = none),
    // half.w = the fade distance outside the box.
    captures: array<SsrProbe, 4>,
}

struct VsOut {
    @builtin(position) clip: vec4<f32>,
}

@group(0) @binding(0) var<uniform> params: SsrParams;
@group(0) @binding(1) var hdr_tex: texture_2d<f32>;
@group(0) @binding(2) var depth_tex: texture_depth_2d;
@group(0) @binding(3) var probe_tex: texture_2d_array<f32>;
@group(0) @binding(4) var probe_samp: sampler;

fn reconstruct(pix: vec2<i32>) -> vec3<f32> {
    let w = max(params.screen.x, 1.0);
    let h = max(params.screen.y, 1.0);
    let uv = (vec2<f32>(pix) + vec2<f32>(0.5)) / vec2<f32>(w, h);
    let depth = textureLoad(depth_tex, pix, 0);
    let ndc = vec3<f32>(uv.x * 2.0 - 1.0, (1.0 - uv.y) * 2.0 - 1.0, depth);
    let world = params.inv_view_proj * vec4<f32>(ndc, 1.0);
    return world.xyz / select(world.w, 1e-6, abs(world.w) < 1e-6);
}

fn equirect_uv(dir: vec3<f32>) -> vec2<f32> {
    let d = normalize(dir);
    let phi = atan2(d.z, d.x);
    let theta = acos(clamp(d.y, -1.0, 1.0));
    return vec2<f32>(phi * (0.5 / 3.14159265) + 0.5, theta / 3.14159265);
}

fn box_lookup(origin: vec3<f32>, dir: vec3<f32>, bmin: vec3<f32>, bmax: vec3<f32>) -> vec3<f32> {
    let rd = normalize(dir + vec3<f32>(1e-6));
    let rbmax = (bmax - origin) / rd;
    let rbmin = (bmin - origin) / rd;
    let rx = select(rbmin.x, rbmax.x, rd.x > 0.0);
    let ry = select(rbmin.y, rbmax.y, rd.y > 0.0);
    let rz = select(rbmin.z, rbmax.z, rd.z > 0.0);
    var fa = min(min(rx, ry), rz);
    if (fa < 0.0) {
        fa = 0.0;
    }
    let hit = origin + rd * fa;
    return hit - (bmin + bmax) * 0.5;
}

fn sample_probes(world: vec3<f32>, refl: vec3<f32>) -> vec3<f32> {
    let count = u32(params.misc.x + 0.5);
    var acc = vec3<f32>(0.0);
    var wsum = 0.0;
    let n = min(count, 4u);
    for (var i: u32 = 0u; i < n; i = i + 1u) {
        let weight = params.probes[i].half.w;
        if (weight <= 0.0) {
            continue;
        }
        let center = params.probes[i].center.xyz;
        let half = max(params.probes[i].half.xyz, vec3<f32>(0.05));
        let lookup = box_lookup(world, refl, center - half, center + half);
        let uv = equirect_uv(lookup);
        acc += textureSampleLevel(probe_tex, probe_samp, uv, i32(i), 0.0).rgb * weight;
        wsum += weight;
    }
    if (wsum <= 1e-5) {
        return vec3<f32>(0.0);
    }
    return acc / wsum * params.misc.y;
}

// The share of `world` the local reflection captures cover (ADR-2860), weighed as the forward
// pass weighs them; there the forward pass already reflects them in HDR. 0 without captures.
// Depth reconstruction tolerance on a capture box's faces, metres (ADR-2860).
const CAPTURE_FACE_MARGIN_M: f32 = 0.02;

fn capture_cover(world: vec3<f32>) -> f32 {
    var cover = 0.0;
    for (var i: u32 = 0u; i < 4u; i = i + 1u) {
        let weight = params.captures[i].center.w;
        if (weight <= 0.0) {
            continue;
        }
        // `world` comes from the depth buffer: on a box face (a room's box fitted to its walls) it
        // scatters a few millimetres either side, so the face's margin counts as inside.
        let outside = max(length(max(abs(world - params.captures[i].center.xyz) - params.captures[i].half.xyz, vec3<f32>(0.0))) - CAPTURE_FACE_MARGIN_M, 0.0);
        cover += weight * (1.0 - smoothstep(0.0, max(params.captures[i].half.w, 1e-3), outside));
    }
    return clamp(cover, 0.0, 1.0);
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VsOut {
    // Corners (-1,-1), (3,-1), (-1,3), without a function-local array or struct.
    let x = select(-1.0, 3.0, vertex_index == 1u);
    let y = select(-1.0, 3.0, vertex_index == 2u);
    return VsOut(vec4<f32>(x, y, 0.0, 1.0));
}

@fragment
fn fs_main(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let w = i32(params.screen.x);
    let h = i32(params.screen.y);
    let pix = vec2<i32>(i32(frag.x), i32(frag.y));
    let base = textureLoad(hdr_tex, pix, 0);
    // `base.a` is perceptual roughness from the forward pass. Same thresholds
    // as `ssr_roughness_weight`: the 0.35 golden floor stays a mirror, and a
    // surface at 0.7 or rougher (outdoor ground, sky dome) keeps its colour.
    let fade = clamp((0.7 - clamp(base.a, 0.0, 1.0)) / 0.35, 0.0, 1.0);
    if (fade <= 0.0 || pix.x >= w - 1 || pix.y >= h - 1) {
        return base;
    }
    // A depth crease is a silhouette, not a surface. Shading across it (McGuire
    // & Mara) reconstructs a normal that points at the floor and mixes a dark
    // probe into the edge. A flat floor moves about 0.001 in NDC per pixel.
    let depth = textureLoad(depth_tex, pix, 0);
    let dpx = abs(textureLoad(depth_tex, clamp(pix + vec2<i32>(1, 0), vec2<i32>(0), vec2<i32>(w - 1, h - 1)), 0) - depth);
    let dmx = abs(textureLoad(depth_tex, clamp(pix + vec2<i32>(-1, 0), vec2<i32>(0), vec2<i32>(w - 1, h - 1)), 0) - depth);
    let dpy = abs(textureLoad(depth_tex, clamp(pix + vec2<i32>(0, 1), vec2<i32>(0), vec2<i32>(w - 1, h - 1)), 0) - depth);
    let dmy = abs(textureLoad(depth_tex, clamp(pix + vec2<i32>(0, -1), vec2<i32>(0), vec2<i32>(w - 1, h - 1)), 0) - depth);
    if (dpx > 0.05 || dmx > 0.05 || dpy > 0.05 || dmy > 0.05) {
        return base;
    }
    let world = reconstruct(pix);
    let dx = reconstruct(pix + vec2<i32>(1, 0)) - world;
    let dy = reconstruct(pix + vec2<i32>(0, 1)) - world;
    var n = cross(dx, dy);
    let nlen = length(n);
    if (nlen < 1e-5) {
        return base;
    }
    n = n / nlen;
    let view_dir = normalize(params.camera.xyz - world);
    if (dot(n, view_dir) < 0.0) {
        n = -n;
    }
    if (n.y < params.misc.w) {
        return base;
    }
    let refl = reflect(-view_dir, n);
    let probe = sample_probes(world, refl);
    var hit = false;
    var hit_color = base.rgb;
    if (params.misc.z > 0.5) {
        let step_len = max(params.screen.w, 0.02);
        let thickness = params.screen.z;
        for (var s: i32 = 2; s < 18; s = s + 1) {
            let p = world + refl * (f32(s) * step_len);
            let clip = params.view_proj * vec4<f32>(p, 1.0);
            if (abs(clip.w) < 1e-4) {
                continue;
            }
            let ndc = clip.xyz / clip.w;
            let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
            if (uv.x < 0.0 || uv.y < 0.0 || uv.x > 1.0 || uv.y > 1.0 || ndc.z < 0.0 || ndc.z > 1.0) {
                break;
            }
            let sp = vec2<i32>(i32(uv.x * params.screen.x), i32(uv.y * params.screen.y));
            let scene_z = textureLoad(depth_tex, sp, 0);
            if (ndc.z > scene_z && ndc.z < scene_z + thickness) {
                hit = true;
                hit_color = textureLoad(hdr_tex, sp, 0).rgb;
                break;
            }
        }
    }
    var refl_color = base.rgb;
    var apply = false;
    if (params.misc.z > 0.5 && hit) {
        refl_color = hit_color;
        apply = true;
    } else if (params.misc.x > 0.5) {
        refl_color = probe;
        // A miss inside a local capture keeps the forward pass's HDR capture (ADR-2860).
        let cover = capture_cover(world);
        if (cover >= 1.0) {
            // Fully covered: the forward pass's pixel as it is (`mix(base, base, k)` is not
            // always `base` to the last bit).
            return base;
        }
        if (cover > 0.0) {
            refl_color = mix(probe, base.rgb, cover);
        }
        apply = true;
    }
    if (!apply) {
        return base;
    }
    return vec4<f32>(mix(base.rgb, refl_color, 0.72 * fade), base.a);
}
"#;

/// The march the engine ships.
const MARCH: &str = include_str!("../shaders/ssr.wgsl");

/// A fullscreen pipeline of `source`, with a depth target when `depth`.
fn pipeline(
    d: &wgpu::Device,
    label: &str,
    source: &str,
    entries: (&str, &str),
    depth: bool,
) -> wgpu::RenderPipeline {
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    });
    d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: None,
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some(entries.0),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: depth.then(|| wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::Always),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some(entries.1),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba16Float,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// A cleared colour attachment on `view`.
fn color(view: &wgpu::TextureView) -> wgpu::RenderPassColorAttachment<'_> {
    wgpu::RenderPassColorAttachment {
        view,
        depth_slice: None,
        resolve_target: None,
        ops: wgpu::Operations {
            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
            store: wgpu::StoreOp::Store,
        },
    }
}

/// `FRAMES` frames of the scene pass followed by `march` (with the march's loop on or off), each
/// frame's RGBA16F bytes.
fn frames(device: &RenderDevice, march: &str, march_on: bool) -> Vec<Vec<u8>> {
    let (d, q) = (&device.device, &device.queue);
    let target = |label: &str, format: wgpu::TextureFormat, usage: wgpu::TextureUsages| {
        d.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: W,
                height: H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        })
    };
    let attach_and_sample = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
    let hdr = target("probe hdr", wgpu::TextureFormat::Rgba16Float, attach_and_sample);
    let depth = target(
        "probe depth",
        wgpu::TextureFormat::Depth32Float,
        attach_and_sample,
    );
    let out = target(
        "probe out",
        wgpu::TextureFormat::Rgba16Float,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
    );
    // The march's probe array and sampler: bound, never read (the probe count is 0).
    let probes = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe array"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let probe_view = probes.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    });
    let sampler = d.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });

    // The room test's camera (`ssr_linear_repeatable`), one buffer for both passes: the scene's
    // `Params` is the head of the march's `SsrParams`.
    let eye = Vec3::new(0.0, 1.5, 2.6);
    let view_proj = perspective(std::f32::consts::FRAC_PI_3, W as f32 / H as f32, 0.05, 20.0)
        * look_at_mat4(eye, Vec3::new(0.0, 0.2, -1.0), Vec3::Y);
    let mut params = Vec::with_capacity(PARAM_FLOATS);
    params.extend_from_slice(&view_proj.inverse().to_cols_array());
    params.extend_from_slice(&view_proj.to_cols_array());
    params.extend_from_slice(&[eye.x, eye.y, eye.z, 1.0]);
    // Width, height, thickness and step length; probe count, scale, march on, floor normal.y.
    params.extend_from_slice(&[W as f32, H as f32, 0.06, 0.22]);
    params.extend_from_slice(&[0.0, 1.0, if march_on { 1.0 } else { 0.0 }, 0.72]);
    params.resize(PARAM_FLOATS, 0.0);
    let ubuf = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("probe params"),
        size: (PARAM_FLOATS * 4) as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    q.write_buffer(&ubuf, 0, bytemuck::cast_slice(&params));

    let scene_pipe = pipeline(d, "probe scene", SCENE, ("vs", "fs"), true);
    let march_pipe = pipeline(d, "probe march", march, ("vs_main", "fs_main"), false);
    let scene_bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("probe scene"),
        layout: &scene_pipe.get_bind_group_layout(0),
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: ubuf.as_entire_binding(),
        }],
    });
    let hdr_view = hdr.create_view(&wgpu::TextureViewDescriptor::default());
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
    let depth_sampled = depth.create_view(&wgpu::TextureViewDescriptor {
        aspect: wgpu::TextureAspect::DepthOnly,
        ..Default::default()
    });
    let out_view = out.create_view(&wgpu::TextureViewDescriptor::default());
    let march_bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("probe march"),
        layout: &march_pipe.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: ubuf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&hdr_view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(&depth_sampled),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(&probe_view),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
        ],
    });
    let row = W * 8;
    let read = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("probe read"),
        size: u64::from(row * H),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    (0..FRAMES)
        .map(|_| {
            let mut enc = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            {
                let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("probe scene"),
                    color_attachments: &[Some(color(&hdr_view))],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &depth_view,
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
                pass.set_pipeline(&scene_pipe);
                pass.set_bind_group(0, &scene_bind, &[]);
                pass.draw(0..3, 0..1);
            }
            {
                let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("probe march"),
                    color_attachments: &[Some(color(&out_view))],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&march_pipe);
                pass.set_bind_group(0, &march_bind, &[]);
                pass.draw(0..3, 0..1);
            }
            enc.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &out,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &read,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(row),
                        rows_per_image: Some(H),
                    },
                },
                wgpu::Extent3d {
                    width: W,
                    height: H,
                    depth_or_array_layers: 1,
                },
            );
            q.submit([enc.finish()]);
            read.slice(..)
                .map_async(wgpu::MapMode::Read, |r| r.expect("map the probe"));
            d.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
            let bytes = read.slice(..).get_mapped_range().expect("mapped probe").to_vec();
            read.unmap();
            bytes
        })
        .collect()
}

/// Frames that differ from the first, and the most pixels one of them differs in.
fn unstable(frames: &[Vec<u8>]) -> (usize, usize) {
    let first = &frames[0];
    frames[1..].iter().fold((0, 0), |(count, worst), frame| {
        let pixels = pixels_differing(first, frame);
        (count + usize::from(pixels > 0), worst.max(pixels))
    })
}

/// Pixels (8 bytes each) in which two frames differ.
fn pixels_differing(a: &[u8], b: &[u8]) -> usize {
    a.as_chunks::<8>()
        .0
        .iter()
        .zip(b.as_chunks::<8>().0)
        .filter(|(x, y)| x != y)
        .count()
}

#[test]
fn the_march_is_repeatable_over_two_rendered_textures() {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip loop_break_fetch_probe: {err}");
            return;
        }
    };
    let info = device.adapter.get_info();
    let name = format!("{} ({} {})", info.name, info.driver, info.driver_info);
    let base = frames(&device, BASE_MARCH, true);
    let current = frames(&device, MARCH, true);
    let plain = frames(&device, MARCH, false);
    let (base_frames, base_pixels) = unstable(&base);
    let (current_frames, current_pixels) = unstable(&current);
    let live = pixels_differing(&current[0], &plain[0]);
    eprintln!(
        "{name}: colour fetched inside the loop before `break`: {base_frames} of {FRAMES} frames differ \
         from the first (at most {base_pixels} of {} pixels); fetched after the loop: {current_frames} \
         ({current_pixels}); the march changes {live} pixels of the scene",
        W * H
    );
    assert!(
        live > (W * H / 20) as usize,
        "the march is live in the probe scene: it changes {live} pixels"
    );
    assert_eq!(
        current_frames, 0,
        "the march is not repeatable: {current_frames} of {FRAMES} frames differ from the first (at most \
         {current_pixels} pixels)"
    );
    if base_frames == 0 {
        assert_eq!(
            pixels_differing(&base[0], &current[0]),
            0,
            "the march before and after ADR-5713 renders the same frame"
        );
    }
}
