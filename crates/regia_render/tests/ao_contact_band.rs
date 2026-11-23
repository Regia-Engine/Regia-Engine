// SPDX-License-Identifier: MPL-2.0
//! Contact-scale calibration against the reference (ADR-3202, spec 27 § 2.2 rows `ao_contact` and
//! `sun_penumbra_1m`).
//!
//! **AO in the 10 cm band.** A 1 m box rests on a plane (the spec 27 `ao_contact` scene). The SSAO
//! kernels run on that depth buffer, written analytically by a ray cast in the fragment shader (no
//! mesh, no frame), and the AO term is read along the ground out of the box's +X face. The
//! reference is analytic: a ground point's cosine-weighted view of the face is the form factor of
//! a differential area to a perpendicular rectangle (Howell's catalogue, configuration B-3; Siegel
//! and Howell, "Thermal Radiation Heat Transfer"), so the unoccluded share is one minus it. The
//! horizon kernel (`cs_horizon`) must land within 15 % of the analytic occlusion in the band, seen
//! from straight above (the spec 27 view, where the face itself is hidden) and at an angle. The
//! point kernel's number is printed for the record (it is the 0.32× row).
//!
//! **Sun penumbra near contact.** A CPU mirror of the sun PCSS filter (`shadows.wgsl`
//! `shadow_pcss_sun`: 16 Vogel taps, bilinear, the one-texel radius floor) on a straight edge
//! shows where the 1 m row's floor comes from (the floor radius and each tap's bilinear tent, about
//! 1.7 texels however small the sun's disc is) and that shadow filter mode 4's steepening
//! (`sun_penumbra_gain`, constants read from the shader) brings the per-pixel 10-90 % width to the
//! sun disc's within 15 % from a 0.6-texel radius up, and leaves wide penumbrae untouched. On the GPU (the
//! lab frame, lavapipe) the spec 27 sun wall is measured at 1, 5 and 20 m from contact with the
//! mode off and on, against the disc's analytic width, and a slowly moving sun counts flicker
//! (frame-to-frame reversals of a pixel's visibility) with the mode off and on: no new flicker.
//!
//! No golden: every reference is analytic or the other mode on the same frame.

use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use regia_core::glam::Vec3;
use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, GiMode,
    LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset,
};
use regia_test_utils::spec27::edge_width;

const SSAO: &str = include_str!("../shaders/ssao.wgsl");
const SHADOWS: &str = include_str!("../shaders/shadows.wgsl");

/// `regia_render::shadow::SUN_PCSS_ANGULAR_RADIUS_TAN`.
const SUN_RADIUS: f32 = 0.004_675;

static GPU: Mutex<()> = Mutex::new(());

fn gpu() -> (MutexGuard<'static, ()>, &'static RenderDevice) {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    let guard = GPU.lock().unwrap_or_else(PoisonError::into_inner);
    (
        guard,
        D.get_or_init(|| RenderDevice::headless().expect("headless GPU (lavapipe in CI)")),
    )
}

/// A `const NAME: f32 = value;` of a shader source.
fn wgsl_const(src: &str, name: &str) -> f32 {
    let key = format!("const {name}: f32 = ");
    let start = src
        .find(&key)
        .unwrap_or_else(|| panic!("{name} is not in the shader"))
        + key.len();
    let end = start + src[start..].find(';').expect("const ends");
    src[start..end].trim().parse().expect("a float literal")
}

// --- The analytic AO reference ---------------------------------------------------------------

/// Form factor from a differential area to a rectangle perpendicular to it that shares the
/// line through its foot: width `b` along that line from the foot's projection, height `c`, at
/// distance `a` (Howell's B-3).
fn ff_corner(a: f64, b: f64, c: f64) -> f64 {
    let s = a.hypot(c);
    ((b / a).atan() - a / s * (b / s).atan()) / std::f64::consts::TAU
}

/// The unoccluded cosine-weighted share at ground point `(x, 0, z)` (x > 0) beside the box
/// x ∈ [-1, 0], y ∈ [0, 1], z ∈ [-0.5, 0.5]: only its +X face is in view.
fn analytic_visibility(x: f64, z: f64) -> f64 {
    let x = x.max(1e-4);
    let width = |w: f64| {
        if w > 0.0 {
            ff_corner(x, w, 1.0)
        } else {
            -ff_corner(x, -w, 1.0)
        }
    };
    1.0 - (width(0.5 - z) + width(0.5 + z))
}

/// The forward pass's multiple-bounce fit (`forward_occlusion.wgsl` `multi_bounce_ao`).
fn multi_bounce(v: f64, albedo: f64) -> f64 {
    let a = 2.0404 * albedo - 0.3324;
    let b = -4.7951 * albedo + 0.6417;
    let c = 2.7552 * albedo + 0.6903;
    v.max(((v * a + b) * v + c) * v)
}

// --- The analytic depth buffer ---------------------------------------------------------------

/// A pinhole camera (aspect 1) over the box-on-plane scene.
#[derive(Clone, Copy)]
struct Camera {
    eye: Vec3,
    target: Vec3,
    fov_y: f32,
    near: f32,
    far: f32,
}

impl Camera {
    fn basis(self) -> (Vec3, Vec3, Vec3) {
        let f = (self.target - self.eye).normalize();
        let helper = if f.y.abs() > 0.99 { Vec3::Z } else { Vec3::Y };
        let r = f.cross(helper).normalize();
        let u = r.cross(f);
        (r, u, f)
    }

    /// The world ray through pixel centre `(px, py)` of a `size`² image (y down).
    fn ray(self, px: f32, py: f32, size: f32) -> Vec3 {
        let (r, u, f) = self.basis();
        let t = (0.5 * self.fov_y).tan();
        let nx = (px / size) * 2.0 - 1.0;
        let ny = 1.0 - (py / size) * 2.0;
        (f + r * (nx * t) + u * (ny * t)).normalize()
    }
}

/// The first hit of the ray `o + t d` with the ground (y = 0) or the box.
fn hit(o: Vec3, d: Vec3) -> Option<f32> {
    let mut best = f32::INFINITY;
    if d.y < 0.0 {
        best = -o.y / d.y;
    }
    let (lo, hi) = (Vec3::new(-1.0, 0.0, -0.5), Vec3::new(0.0, 1.0, 0.5));
    let inv = d.recip();
    let t0 = (lo - o) * inv;
    let t1 = (hi - o) * inv;
    let (tn, tf) = (t0.min(t1).max_element(), t0.max(t1).min_element());
    if tn <= tf && tn > 0.0 {
        best = best.min(tn);
    }
    best.is_finite().then_some(best)
}

/// The same scene and camera as WGSL: a fullscreen triangle whose fragments write the hit's
/// depth (`ssao.wgsl` `linear_depth` inverted) or 1 (sky).
fn depth_fill_shader(cam: Camera, size: u32) -> String {
    let (r, u, f) = cam.basis();
    let v = |v: Vec3| format!("vec3<f32>({:.9}, {:.9}, {:.9})", v.x, v.y, v.z);
    format!(
        r"
@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {{
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
}}

@fragment
fn fs_main(@builtin(position) p: vec4<f32>) -> @builtin(frag_depth) f32 {{
    let o = {eye};
    let t = tan({half:.9});
    let nx = (p.x / {size:.1}) * 2.0 - 1.0;
    let ny = 1.0 - (p.y / {size:.1}) * 2.0;
    let d = normalize({f} + {r} * (nx * t) + {u} * (ny * t));
    var best = 1e30;
    if (d.y < 0.0) {{
        best = -o.y / d.y;
    }}
    let inv = 1.0 / d;
    let t0 = (vec3<f32>(-1.0, 0.0, -0.5) - o) * inv;
    let t1 = (vec3<f32>(0.0, 1.0, 0.5) - o) * inv;
    let lo = min(t0, t1);
    let hi = max(t0, t1);
    let tn = max(max(lo.x, lo.y), lo.z);
    let tf = min(min(hi.x, hi.y), hi.z);
    if (tn <= tf && tn > 0.0) {{
        best = min(best, tn);
    }}
    if (best > 1e29) {{
        return 1.0;
    }}
    let z = best * dot(d, {f});
    let n = {near:.9};
    let fa = {far:.9};
    return fa * (z - n) / (z * (fa - n));
}}
",
        eye = v(cam.eye),
        half = 0.5 * cam.fov_y,
        size = size as f32,
        f = v(f),
        r = v(r),
        u = v(u),
        near = cam.near,
        far = cam.far,
    )
}

fn depth_texture(device: &RenderDevice, cam: Camera, size: u32) -> wgpu::Texture {
    let d = &device.device;
    let texture = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("ao band depth"),
        size: wgpu::Extent3d {
            width: size,
            height: size,
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
        label: Some("ao band depth fill"),
        source: wgpu::ShaderSource::Wgsl(depth_fill_shader(cam, size).into()),
    });
    let pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("ao band depth fill"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::Always),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[],
        }),
        multiview_mask: None,
        cache: None,
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("ao band depth fill"),
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

/// Runs `entry` of `ssao.wgsl` over the depth buffer of `cam` (`size`², AO at half resolution as
/// the GTAO half-resolution row) with `SsaoPass`'s parameters for that row, and returns the raw
/// AO (layer 0, R) per AO texel.
fn run_kernel(device: &RenderDevice, cam: Camera, size: u32, entry: &str) -> Vec<f32> {
    let d = &device.device;
    let ao = size / 2;
    let depth = depth_texture(device, cam, size);
    // Composed, as `SsaoPass` builds the kernel (`ssao.wgsl` holds a shader-def variant).
    let composed = regia_render::compose_source(SSAO, "ssao.wgsl", std::collections::HashMap::new())
        .expect("ssao composes");
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("ssao under test"),
        source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(composed)),
    });
    let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(entry),
        layout: None,
        module: &module,
        entry_point: Some(entry),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });
    let horizon = entry == "cs_horizon";
    let proj_y = 1.0 / (0.5 * cam.fov_y).tan();
    // `SsaoPass::dispatch` on the GTAO half-resolution row: radius 1.2 m, 12 samples, bias 0.08,
    // strength 1.6 (point kernel) or 1 (horizon kernel), max radius a tenth (point kernel) or a
    // quarter (horizon kernel) of the AO height.
    let mut params: Vec<f32> = vec![
        ao as f32,
        ao as f32,
        1.2,
        12.0,
        1.0,
        0.08,
        if horizon { 1.0 } else { 1.6 },
        (ao as f32 * if horizon { 0.25 } else { 0.1 }).max(4.0),
        proj_y,
        proj_y,
        cam.near,
        cam.far,
        0.0,
        0.0,
        0.0,
        0.0,
    ];
    params.extend([0.0f32; 4]);
    params.extend([1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    let bytes: Vec<u8> = params.iter().flat_map(|v| v.to_le_bytes()).collect();
    use wgpu::util::DeviceExt;
    let uniform = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("ssao params"),
        contents: &bytes,
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let split = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("no split"),
        size: 16,
        usage: wgpu::BufferUsages::STORAGE,
        mapped_at_creation: false,
    });
    let out = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("ao out"),
        size: wgpu::Extent3d {
            width: ao,
            height: ao,
            depth_or_array_layers: 2,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let out_view = out.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    });
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
    let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
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
            wgpu::BindGroupEntry {
                binding: 4,
                resource: split.as_entire_binding(),
            },
        ],
    });
    let row = (ao * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let readback = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("ao readback"),
        size: u64::from(row * ao),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(ao.div_ceil(8), ao.div_ceil(8), 1);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &out,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(ao),
            },
        },
        wgpu::Extent3d {
            width: ao,
            height: ao,
            depth_or_array_layers: 1,
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
    let data = readback.slice(..).get_mapped_range().expect("mapped range");
    let mut values = Vec::with_capacity((ao * ao) as usize);
    for y in 0..ao {
        for x in 0..ao {
            values.push(f32::from(data[(y * row + x * 4) as usize]) / 255.0);
        }
    }
    values
}

/// Mean AO, mean analytic visibility and texel count over the ground texels within 10 cm of the
/// box's +X face (|z| < 0.25: away from its corners), and the mean AO over open ground (more than
/// 1.5 m from the box's footprint, beyond the 1.2 m radius). Each AO texel reads depth texel 2p + 1 (`depth_texel_of` at half resolution).
fn band(values: &[f32], cam: Camera, size: u32) -> (f64, f64, usize, f64) {
    let ao = size / 2;
    let (mut s, mut a, mut n) = (0.0f64, 0.0f64, 0usize);
    let (mut open, mut n_open) = (0.0f64, 0usize);
    for y in 0..ao {
        for x in 0..ao {
            let (dx, dy) = (2 * x + 1, 2 * y + 1);
            let dir = cam.ray(dx as f32 + 0.5, dy as f32 + 0.5, size as f32);
            let Some(t) = hit(cam.eye, dir) else { continue };
            let p = cam.eye + dir * t;
            if p.y.abs() > 1e-3 {
                continue;
            }
            let v = f64::from(values[(y * ao + x) as usize]);
            if p.x > 0.0 && p.x <= 0.10 && p.z.abs() < 0.25 {
                s += v;
                a += analytic_visibility(f64::from(p.x), f64::from(p.z));
                n += 1;
            } else if (-1.0 - p.x).max(p.x).max(0.0).hypot((p.z.abs() - 0.5).max(0.0)) > 1.5 {
                open += v;
                n_open += 1;
            }
        }
    }
    (
        s / n.max(1) as f64,
        a / n.max(1) as f64,
        n,
        open / n_open.max(1) as f64,
    )
}

/// The spec 27 `ao_contact` view (straight down from 40 m over a 3 m window) and a three-quarter
/// view that shows the face.
fn ao_views() -> [(&'static str, Camera); 2] {
    [
        (
            "top-down 40 m (spec 27)",
            Camera {
                eye: Vec3::new(0.6, 40.0, 0.8),
                target: Vec3::new(0.6, 0.0, 0.0),
                fov_y: 2.0 * (1.5f32 / 40.0).atan(),
                near: 0.05,
                far: 400.0,
            },
        ),
        (
            "three-quarter 3.5 m",
            Camera {
                eye: Vec3::new(2.6, 2.0, 1.8),
                target: Vec3::new(0.2, 0.0, 0.0),
                fov_y: 45f32.to_radians(),
                near: 0.05,
                far: 400.0,
            },
        ),
    ]
}

/// The horizon kernel's occlusion in the 10 cm band is within 15 % of the analytic occlusion,
/// from above (the face hidden: the spec 27 view) and at an angle, and open ground stays open.
#[test]
fn horizon_ao_matches_the_analytic_contact_band() {
    let (_guard, device) = gpu();
    let size = 256;
    for (name, cam) in ao_views() {
        let point = run_kernel(device, cam, size, "cs_main");
        let horizon = run_kernel(device, cam, size, "cs_horizon");
        let (p_ao, analytic, n, p_open) = band(&point, cam, size);
        let (h_ao, _, _, h_open) = band(&horizon, cam, size);
        assert!(n > 20, "{name}: band texels {n}");
        let occ = 1.0 - analytic;
        let (k_point, k_horizon) = ((1.0 - p_ao) / occ, (1.0 - h_ao) / occ);
        println!(
            "{name}: {n} band texels; analytic visibility {analytic:.3}; point kernel {p_ao:.3} ({k_point:.2}× the occlusion), horizon {h_ao:.3} ({k_horizon:.2}×); open ground point {p_open:.3}, horizon {h_open:.3}; band darkening after the multi-bounce fit (albedo 0.5): analytic {:.1} %, point {:.1} %, horizon {:.1} % (the tracer: 29.9 % on lavapipe, ADR-2660)",
            100.0 * (1.0 - multi_bounce(analytic, 0.5)),
            100.0 * (1.0 - multi_bounce(p_ao, 0.5)),
            100.0 * (1.0 - multi_bounce(h_ao, 0.5)),
        );
        assert!(
            (k_horizon - 1.0).abs() <= 0.15,
            "{name}: horizon occlusion {:.3} vs analytic {occ:.3} ({k_horizon:.2}×)",
            1.0 - h_ao
        );
        assert!(h_open > 0.97, "{name}: open ground darkened to {h_open:.3}");
    }
}

/// The analytic reference itself: the face blocks half the hemisphere at its foot (an infinite
/// wall's limit as the point reaches it) and fades with distance.
#[test]
fn analytic_contact_reference_is_sane() {
    let at_foot = analytic_visibility(1e-4, 0.0);
    assert!((at_foot - 0.5).abs() < 0.01, "{at_foot}");
    let mut last = at_foot;
    for x in [0.02, 0.05, 0.1, 0.3, 1.0, 3.0] {
        let v = analytic_visibility(x, 0.0);
        assert!(v > last && v < 1.0, "x {x}: {v}");
        last = v;
    }
    // At 5 cm the 1 m × 1 m face takes 0.461 of the cosine-weighted hemisphere.
    assert!((analytic_visibility(0.05, 0.0) - 0.539).abs() < 0.002);
}

// --- The sun PCSS filter on the CPU ----------------------------------------------------------

/// Vogel disc sample `i` of `n` (`shadows.wgsl` `vogel_disc`).
fn vogel(i: u32, n: u32, rot: f32) -> (f32, f32) {
    let r = ((i as f32 + 0.5) / n as f32).sqrt();
    let a = i as f32 * 2.399_963 + rot;
    (a.cos() * r, a.sin() * r)
}

/// `shadow_pcss_sun`'s filter on a straight shadow edge at texel-space x = `edge` (blockers left
/// of it, a flat receiver): the visibility at receiver x for true penumbra radius `r_true`
/// texels, with the mode-4 steepening (`gain` from the shader's constants) or without.
fn pcss_mirror(x: f32, edge: f32, r_true: f32, rot: f32, gain: Option<&dyn Fn(f32, f32) -> f32>) -> f32 {
    let radius = r_true.clamp(1.0, 9.0);
    let lit = |texel: f32| if texel + 0.5 < edge { 0.0 } else { 1.0 };
    let mut sum = 0.0;
    for i in 0..16 {
        let (ox, _) = vogel(i, 16, rot + 1.3);
        let q = x + ox * radius - 0.5;
        let b = q.floor();
        let f = q - b;
        sum += lit(b) * (1.0 - f) + lit(b + 1.0) * f;
    }
    let v = sum / 16.0;
    match gain {
        Some(g) => (0.5 + (v - 0.5) * g(r_true, radius)).clamp(0.0, 1.0),
        None => v,
    }
}

/// The 10-90 % width (texels) of the mirror's edge, per pixel: the profile averaged over 64
/// kernel rotations (the strip's rows), measured at each of 8 sub-texel edge positions and the
/// widths averaged. (An edge along a texel axis, as the spec 27 wall's, has one sub-texel position
/// along its whole length: averaging the profiles over positions would add the grid's stair as
/// blur that such an edge does not show.)
fn mirror_width(r_true: f32, gain: Option<&dyn Fn(f32, f32) -> f32>) -> f32 {
    // ±14 texels: the widest edge measured (a 9-texel filter radius) spreads over about 13.
    let step = 0.02;
    let xs: Vec<f32> = (0..1400).map(|i| -14.0 + i as f32 * step).collect();
    let mut total = 0.0;
    for e in 0..8 {
        let edge = 100.0 + e as f32 / 8.0;
        let mut profile = vec![0.0f32; xs.len()];
        for r in 0..64 {
            let rot = r as f32 / 64.0 * std::f32::consts::TAU;
            for (p, x) in profile.iter_mut().zip(&xs) {
                *p += pcss_mirror(edge + x, edge, r_true, rot, gain) / 64.0;
            }
        }
        total += edge_width(&profile, 0.1, 0.9).expect("the edge resolves") * step;
    }
    total / 8.0
}

/// The 10-90 % width of a uniform disc of radius `r` (the sun's penumbra): numerically, from the
/// disc's marginal.
fn disc_width(r: f32) -> f32 {
    let n = 4000;
    let cdf: Vec<f32> = (0..=n)
        .scan(0.0f64, |acc, i| {
            let x = -1.0 + 2.0 * f64::from(i) / f64::from(n);
            *acc += (1.0 - x * x).max(0.0).sqrt() * 2.0 / f64::from(n) * 2.0 / std::f64::consts::PI;
            Some(*acc as f32)
        })
        .collect();
    edge_width(&cdf, 0.1, 0.9).expect("disc") * 2.0 / n as f32 * r
}

/// Where the 1 m row's floor comes from, and that mode 4 follows the sun's disc from a 0.6-texel
/// radius up (below it, down to the 0.4-texel floor) and leaves wide penumbrae alone.
#[test]
fn angular_penumbra_mirror_follows_the_sun_disc() {
    let disc = wgsl_const(SHADOWS, "SUN_PENUMBRA_DISC_WIDTH");
    let tent = wgsl_const(SHADOWS, "SUN_PENUMBRA_TENT_WIDTH");
    let fit = wgsl_const(SHADOWS, "SUN_PENUMBRA_RAMP_FIT");
    let min_width = wgsl_const(SHADOWS, "SUN_PENUMBRA_MIN_WIDTH_TEXELS");
    assert_eq!(wgsl_const(SHADOWS, "SUN_PCSS_MIN_RADIUS_TEXELS"), 1.0);
    assert!(
        (disc_width(1.0) - disc).abs() < 0.01,
        "disc width {}",
        disc_width(1.0)
    );
    // `sun_penumbra_gain`.
    let gain = move |r_true: f32, radius: f32| {
        let filter = (disc * radius).hypot(tent);
        (fit * filter / (disc * r_true.max(0.0)).max(min_width)).max(1.0)
    };
    // The spec 27 1 m row: the frame measured 14.1 mm, which the mirror's 1.7-texel floor puts at
    // 8.3 mm texels on cascade 0, so the sun's disc is 4.675 / 8.3 = 0.56 texels in radius.
    let r_1m = 4.675 / 8.3;
    let base = mirror_width(r_1m, None) / disc_width(r_1m);
    let calibrated = mirror_width(r_1m, Some(&gain)) / disc_width(r_1m);
    println!(
        "1 m row: mode 3 {base:.2}× the sun disc's width (measured 2.07× on lavapipe), mode 4 {calibrated:.2}×"
    );
    assert!(base > 1.2, "the mirror reproduces the failing row ({base:.2}×)");
    for r in [0.6f32, 0.75, 1.0, 1.5, 2.0, 2.6, 4.0, 8.0] {
        let plain = mirror_width(r, None) / disc_width(r);
        let steep = mirror_width(r, Some(&gain)) / disc_width(r);
        println!("r_true {r:.2} texels: mode 3 {plain:.2}×, mode 4 {steep:.2}×");
        assert!(
            (0.85..=1.15).contains(&steep),
            "r {r}: mode 4 width {steep:.2}× the disc's"
        );
        assert!(
            steep <= plain + 1e-4,
            "r {r}: mode 4 never widens ({steep:.2} vs {plain:.2})"
        );
    }
    // At contact the ramp is the floor's width.
    let contact = mirror_width(0.05, Some(&gain));
    assert!(
        (0.25..=0.5).contains(&contact),
        "contact ramp {contact:.2} texels (floor {min_width})"
    );
    // A wide penumbra (the filter's own width under the disc's): mode 4 is mode 3 exactly.
    assert_eq!(gain(12.0, 9.0), 1.0);
    for x in [-1.0f32, -0.3, 0.0, 0.4, 1.7] {
        assert_eq!(
            pcss_mirror(100.0 + x, 100.0, 12.0, 0.7, None),
            pcss_mirror(100.0 + x, 100.0, 12.0, 0.7, Some(&gain))
        );
    }
}

// --- The sun penumbra in the frame -----------------------------------------------------------

fn quad(c: Vec3, u: Vec3, v: Vec3, material: usize) -> LabMesh {
    let n = u.cross(v).normalize_or(Vec3::Y).to_array();
    let corners = [c - u - v, c + u - v, c + u + v, c - u + v];
    let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    LabMesh {
        vertices: corners
            .iter()
            .zip(uvs)
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(p.to_array(), n, uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

fn grey(albedo: f32) -> LabMaterial {
    LabMaterial {
        base_color: [albedo, albedo, albedo, 1.0],
        roughness: 1.0,
        metallic: 0.0,
        ..LabMaterial::default()
    }
}

/// Desktop High as the spec 27 suite runs it (native scale, no TAAU or MSAA, no fog, clouds,
/// sky, reflections or GI beyond the flat ambient), with the angular penumbra `angular`.
fn desktop_high(angular: bool) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.msaa_samples = 1;
    q.render_scale_permille = 1000;
    q.horizontal_plus = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.physical_atmosphere = false;
    q.ssr = false;
    q.reflection_probes = false;
    q.gi = GiMode::Ambient;
    q.screen_space_gi = false;
    q.gi_probes = false;
    q.baked_gi = false;
    q.angular_sun_penumbra = angular;
    q
}

/// The spec 27 sun wall (a 30 m × 12 m double-sided wall on the x = 0 plane, its vertical edge at
/// z = 0) under a sun `elevation_deg` high travelling toward +X turned `azimuth_deg` toward +Z,
/// seen top-down from 4 m above ground point `(x, 0, 0)` with a 20° lens.
fn sun_wall(n: u32, x: f32, elevation_deg: f32, azimuth_deg: f32) -> LabScene {
    let mut materials = vec![grey(0.5), grey(0.5)];
    materials[1].double_sided = true;
    let (e, a) = (elevation_deg.to_radians(), azimuth_deg.to_radians());
    LabScene {
        width: n,
        height: n,
        eye: [x, 4.0, 0.08],
        target: [x, 0.0, 0.0],
        fov_y: 20f32.to_radians(),
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.4,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 14.4,
            use_physical_camera: false,
            exposure_compensation: 0.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new(
            [e.cos() * a.cos(), -e.sin(), e.cos() * a.sin()],
            [1.0; 3],
            100_000.0,
        ),
        local_lights: Vec::new(),
        materials,
        meshes: vec![
            quad(Vec3::ZERO, Vec3::X * 60.0, -Vec3::Z * 60.0, 0),
            quad(Vec3::new(0.0, 15.0, -6.0), Vec3::Z * 6.0, Vec3::Y * 15.0, 1),
        ],
    }
}

fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Reuse pipeline/resource allocations across measurements. Each capture starts with a camera
/// cut, which resets the histories and cached shadows just as a fresh renderer would. Keep the
/// lit and open controls separate and retain all three settle frames (ADR-4760).
struct SunFrames {
    lit: FrameRenderer,
    open: FrameRenderer,
}

impl SunFrames {
    fn new() -> Self {
        let renderer = || {
            let mut renderer = FrameRenderer::new();
            renderer.set_ddgi_enabled(Some(false));
            renderer.set_outdoor(OutdoorParams {
                planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
                ..OutdoorParams::default()
            });
            renderer
        };
        Self {
            lit: renderer(),
            open: renderer(),
        }
    }

    /// Sun visibility per pixel: the lit frame over the frame without sun shadows (the last of 3).
    fn visibility(&mut self, device: &RenderDevice, scene: &LabScene, quality: FrameQuality) -> Vec<f32> {
        let frame = |renderer: &mut FrameRenderer, scene: &LabScene| {
            renderer.camera_cut();
            let frame = scene.as_frame_scene_with_quality(quality);
            for i in 0..3 {
                if i == 2 {
                    renderer.request_hdr_capture();
                }
                renderer.render_offscreen(device, &frame).expect("frame");
            }
            renderer
                .take_hdr_capture(device)
                .expect("read back")
                .expect("the HDR copy exists")
                .rgb
        };
        let lit = frame(&mut self.lit, scene);
        let mut open = scene.clone();
        open.directional_shadows = false;
        let unshadowed = frame(&mut self.open, &open);
        lit.iter()
            .zip(&unshadowed)
            .map(|(l, u)| {
                let u = luminance(*u);
                if u > 1e-4 {
                    (luminance(*l) / u).clamp(0.0, 1.5)
                } else {
                    1.0
                }
            })
            .collect()
    }
}

/// The 10-90 % width (pixels) of the edge across the image centre: a strip of rows averaged,
/// read along the axis with the larger range (`spec27_calibration.rs` `penumbra_px`).
fn penumbra_px(vis: &[f32], n: usize) -> Option<f32> {
    let strip = (n / 20).max(2);
    let profile = |along_x: bool| -> Vec<f32> {
        (0..n)
            .map(|i| {
                let s: f32 = (n / 2 - strip..n / 2 + strip)
                    .map(|j| if along_x { vis[j * n + i] } else { vis[i * n + j] })
                    .sum();
                s / (2 * strip) as f32
            })
            .collect()
    };
    let range =
        |p: &[f32]| p.iter().copied().fold(f32::MIN, f32::max) - p.iter().copied().fold(f32::MAX, f32::min);
    let (px, py) = (profile(true), profile(false));
    edge_width(if range(&px) > range(&py) { &px } else { &py }, 0.1, 0.9)
}

/// The spec 27 penumbra rows in the frame: at 1 m the mode-3 width is the failing 2× and mode 4
/// is within 20 % of the sun disc's analytic width; 5 m stays within 20 % in both modes, and at
/// 20 m (a filter radius the floor never touches) the two modes draw the same visibility.
#[test]
fn angular_penumbra_rows_in_the_frame() {
    let (_guard, device) = gpu();
    let mut frames = SunFrames::new();
    let n = 512u32;
    let pitch_mm = 2.0 * 4.0 * (10f32.to_radians()).tan() / n as f32 * 1000.0;
    let e = 45f32.to_radians();
    for d in [1.0f32, 5.0, 20.0] {
        let scene = sun_wall(n, d * e.cos(), 45.0, 0.0);
        let plain = frames.visibility(device, &scene, desktop_high(false));
        let angular = frames.visibility(device, &scene, desktop_high(true));
        if d == 1.0 {
            assert!(
                plain == frames.visibility(device, &scene, desktop_high(false)),
                "resetting a cached capture must match the fresh renderer"
            );
        }
        let disc_mm = disc_width(SUN_RADIUS * d * 1000.0);
        let (w3, w4) = (
            penumbra_px(&plain, n as usize).expect("mode 3 edge") * pitch_mm,
            penumbra_px(&angular, n as usize).expect("mode 4 edge") * pitch_mm,
        );
        println!(
            "{d} m: sun disc {disc_mm:.1} mm; mode 3 {w3:.1} mm ({:.2}×), mode 4 {w4:.1} mm ({:.2}×)",
            w3 / disc_mm,
            w4 / disc_mm
        );
        if d < 2.0 {
            assert!(
                w3 / disc_mm > 1.2,
                "the base row fails as measured ({:.2}×)",
                w3 / disc_mm
            );
        }
        if d < 10.0 {
            assert!(
                (0.8..=1.2).contains(&(w4 / disc_mm)),
                "{d} m: mode 4 {w4:.1} mm vs disc {disc_mm:.1} mm"
            );
        } else {
            let worst = plain
                .iter()
                .zip(&angular)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max);
            assert!(worst < 1e-6, "{d} m: the wide penumbra moved by {worst}");
        }
    }
}

/// Flicker under a slowly moving sun: 16 frames over the 1 m row, the sun turning 0.05° a frame
/// in azimuth, so the wall edge's shadow sweeps about 1.2 cascade texels across the ground (the
/// texel steps crawl). A pixel flickers when its visibility changes by more than 0.02 one way and
/// then the other between consecutive frames (a steadily moving edge moves each pixel one way
/// only). Mode 4 adds no flicker events over mode 3.
#[test]
fn angular_penumbra_adds_no_flicker_under_a_moving_sun() {
    let (_guard, device) = gpu();
    let mut captures = SunFrames::new();
    let n = 256u32;
    let x = 45f32.to_radians().cos();
    let mut count = |angular: bool| -> usize {
        let frames: Vec<Vec<f32>> = (0..16)
            .map(|i| {
                captures.visibility(
                    device,
                    &sun_wall(n, x, 45.0, 0.05 * i as f32),
                    desktop_high(angular),
                )
            })
            .collect();
        let mut events = 0;
        for p in 0..(n * n) as usize {
            let mut last_sign = 0i32;
            for w in frames.windows(2) {
                let delta = w[1][p] - w[0][p];
                if delta.abs() <= 0.02 {
                    continue;
                }
                let sign = if delta > 0.0 { 1 } else { -1 };
                if last_sign != 0 && sign != last_sign {
                    events += 1;
                }
                last_sign = sign;
            }
        }
        events
    };
    let (plain, angular) = (count(false), count(true));
    println!("flicker events over 16 frames: mode 3 {plain}, mode 4 {angular}");
    assert!(angular <= plain, "mode 4 flickers more: {angular} vs {plain}");
}
