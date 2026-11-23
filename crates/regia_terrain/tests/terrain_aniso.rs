// SPDX-License-Identifier: MPL-2.0
//! Anisotropic terrain filtering (`regia_render::TerrainBudget::anisotropy`, ADR-192):
//! grazing ground keeps the detail of its pixel's short axis.
//!
//! A flat photoreal terrain with one layer: stripes along the view direction (detail across
//! the pixel's short axis) over fine stripes across it (the long axis, where too sharp a
//! filter would alias). The eye is 1.7 m up (DO27 on foot), looking along the stripes, so
//! ground 20–120 m out is seen at 1–5° (footprints 10–60× longer than wide). Measured in that
//! far band, as the high-frequency energy of the luma (standard deviation of its Laplacian,
//! DO27's TERRAIN_RES metric):
//! - 16× anisotropic filtering keeps clearly more of it than the isotropic (v0.1.7) filter;
//! - it never shows more than a 4× supersampled frame of the same view (no aliasing gain);
//! - under TAAU upscaling from 0.67 with the camera moving, the same two hold for the
//!   accumulated frame; without TAA a 16x frame changes no more per frame than supersampled
//!   frames do, and under TAAU its frame-to-frame change per unit of detail kept stays at or
//!   under the isotropic frame's (no shimmer spike).
//!
//! The same bars hold on the shader's own footprint path (ADR-3957, forced on every adapter
//! here), which drivers that drop the sampler's anisotropy for explicit-gradient array samples
//! take; `the_sampler_path_matches_the_driver` probes the adapter for that and fails when the
//! automatic choice (`regia_terrain::anisotropy_footprint_for`) leaves such a driver on the
//! sampler path. `the_sampler_lod_offset_matches_the_driver` probes the 16x sampler's trilinear
//! blend and fails when the adapter's sampler-path LOD offset (`regia_terrain::anisotropy_lod_offset_for`,
//! ADR-5459) leaves it filtering sharper than the exact trilinear LOD (RADV without the offset).
//! `the_device_sampler_lod_offset_keeps_the_sampler_path_at_trilinear` holds the offset a terrain draw
//! starts from (`regia_terrain::anisotropy_lod_offset_for_device`, ADR-5797: the device's own
//! measurement) to the same.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SharedPassDraw, TerrainBudget,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerTextures, MaterialParams, Rgba8Image, TerrainLayers, TerrainMap,
    TerrainMaterial, TerrainSharedDraw,
};

const W: u32 = 256;
const H: u32 = 144;
const WORLD_M: f32 = 1024.0;
const EYE_Y: f32 = 1.7;
const FOV_Y: f32 = 0.8;
/// Texture repeats per metre (a 64-texel tile every 4 m: 6.25 cm texels).
const TILING: f32 = 0.25;
/// Far band, metres of ground distance.
const BAND: (f32, f32) = (20.0, 120.0);

fn map() -> TerrainMap {
    let n = 257u32;
    TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], WORLD_M, 1.0, 0.0).expect("map")
}

fn layers() -> TerrainLayers {
    TerrainLayers {
        layers: vec![LayerDesc {
            name: "stripes".into(),
            albedo: [1.0, 1.0, 1.0],
            roughness: 0.9,
            tiling: TILING,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            blend_sharpness: 0.3,
            triplanar: false,
            height_blend: 0.0,
            detail_normal_strength: 0.0,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    }
}

/// 64² albedo: stripes across U (16-texel period, 1 m on the ground; they run along the
/// view) plus fine stripes across V (4-texel period, 25 cm; they cross the view).
fn stripes() -> LayerTextures {
    let size = 64u32;
    let mut albedo = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let along = if (x / 8) % 2 == 0 { 0.75 } else { 0.1 };
            let across = if (y / 2) % 2 == 0 { 0.1 } else { -0.05 };
            let linear: f32 = along + across;
            let c = (linear.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0).round() as u8;
            albedo.extend_from_slice(&[c, c, c, 255]);
        }
    }
    LayerTextures {
        albedo: Rgba8Image {
            width: size,
            height: size,
            data: albedo,
        },
        normal: Rgba8Image::solid(size, size, [128, 128, 255, 255]),
        height: Rgba8Image::solid(size, size, [128, 128, 128, 255]),
        orm: Rgba8Image::solid(size, size, [255, 255, 0, 255]),
        use_orm: false,
    }
}

fn material() -> TerrainMaterial {
    TerrainMaterial {
        layers: vec![Some(stripes())],
        macro_set: None,
        params: MaterialParams {
            anti_tiling_cell: 0.0,
            ..MaterialParams::default()
        },
    }
}

fn eye(z: f32) -> [f32; 3] {
    [0.3, EYE_Y, z]
}

fn target(z: f32) -> [f32; 3] {
    [0.3, 0.0, z - 80.0]
}

struct Run {
    renderer: FrameRenderer,
}

impl Run {
    /// A renderer with the terrain draw; `footprint` forces the anisotropic tap path (`None`
    /// keeps the adapter's automatic choice).
    fn new(device: &RenderDevice, footprint: Option<bool>) -> Self {
        let mut draw =
            TerrainSharedDraw::new(device, map(), &layers(), CdlodConfig::default(), 5).expect("terrain");
        draw.set_material(&material(), &[]).expect("material");
        if let Some(on) = footprint {
            draw.gpu_mut().set_anisotropy_footprint(on);
        }
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
        Self { renderer }
    }

    /// One frame's luma at `scale`× the test size; `taau` upscales from 0.67 with TAA.
    fn frame(&mut self, device: &RenderDevice, z: f32, aniso: u8, scale: u32, taau: bool) -> Vec<f32> {
        let post = PostSettings {
            auto_exposure: false,
            exposure_ev: 14.0,
            bloom: false,
            taa: taau,
            ..PostSettings::golden_deterministic()
        };
        let lab = LabScene {
            width: W * scale,
            height: H * scale,
            eye: eye(z),
            target: target(z),
            fov_y: FOV_Y,
            near: 0.1,
            far: 4000.0,
            depth_prepass: true,
            directional_shadows: false,
            ibl: true,
            exposure_ev: 14.0,
            ambient_lux: 12_000.0,
            double_sided: false,
            post: Some(post.clone()),
            directional: DirectionalLightUniform::new([-0.3, -0.8, -0.5], [1.0, 0.95, 0.9], 60_000.0),
            local_lights: Vec::new(),
            materials: Vec::new(),
            meshes: Vec::new(),
        };
        let mut quality =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        quality.taau = taau;
        quality.render_scale_permille = if taau { 670 } else { 1000 };
        quality.screen_space_gi = false;
        quality.ssr = false;
        quality.volumetric_fog = false;
        quality.volumetric_clouds = false;
        quality.terrain_budget = TerrainBudget::with_anisotropy(aniso);
        let mut frame = lab.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        let image = self
            .renderer
            .render_offscreen(device, &frame)
            .unwrap_or_else(|err| panic!("frame: {err}"));
        image
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
            .collect()
    }
}

/// Box-downsamples a `scale`× luma image to the test size.
fn downsample(luma: &[f32], scale: u32) -> Vec<f32> {
    let w = (W * scale) as usize;
    let s = scale as usize;
    let mut out = Vec::with_capacity((W * H) as usize);
    for y in 0..H as usize {
        for x in 0..W as usize {
            let mut sum = 0.0;
            for dy in 0..s {
                for dx in 0..s {
                    sum += luma[(y * s + dy) * w + x * s + dx];
                }
            }
            out.push(sum / (s * s) as f32);
        }
    }
    out
}

/// Ground distance of each pixel center's ray on the flat map (infinite for sky), z = `z`.
fn distances(z: f32) -> Vec<f32> {
    let e = glam::Vec3::from_array(eye(z));
    let f = (glam::Vec3::from_array(target(z)) - e).normalize();
    let r = f.cross(glam::Vec3::Y).normalize();
    let u = r.cross(f);
    let t = (FOV_Y * 0.5).tan();
    let aspect = W as f32 / H as f32;
    let mut out = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        for x in 0..W {
            let sx = ((x as f32 + 0.5) / W as f32 * 2.0 - 1.0) * t * aspect;
            let sy = (1.0 - (y as f32 + 0.5) / H as f32 * 2.0) * t;
            let d = (f + r * sx + u * sy).normalize();
            out.push(if d.y < -1e-4 { -e.y / d.y } else { f32::INFINITY });
        }
    }
    out
}

/// Standard deviation of the 4-neighbour Laplacian over the band's interior pixels.
fn hf_energy(luma: &[f32], dist: &[f32]) -> f32 {
    let w = W as usize;
    let mut vals = Vec::new();
    for y in 1..H as usize - 1 {
        for x in 1..w - 1 {
            let i = y * w + x;
            let inside = [i, i - 1, i + 1, i - w, i + w]
                .iter()
                .all(|&j| dist[j] >= BAND.0 && dist[j] <= BAND.1);
            if inside {
                vals.push(4.0 * luma[i] - luma[i - 1] - luma[i + 1] - luma[i - w] - luma[i + w]);
            }
        }
    }
    assert!(
        vals.len() > 1000,
        "the far band must cover the frame ({} px)",
        vals.len()
    );
    let mean = vals.iter().sum::<f32>() / vals.len() as f32;
    (vals.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / vals.len() as f32).sqrt()
}

/// Mean |Δ luma| between two frames over the band.
fn flicker(a: &[f32], b: &[f32], dist: &[f32]) -> f32 {
    let (sum, n) = a
        .iter()
        .zip(b)
        .zip(dist)
        .filter(|(_, d)| **d >= BAND.0 && **d <= BAND.1)
        .fold((0.0, 0usize), |(s, n), ((p, q), _)| (s + (p - q).abs(), n + 1));
    sum / n.max(1) as f32
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn grazing_ground_keeps_its_short_axis_detail() {
    grazing(None);
}

#[test]
fn grazing_ground_keeps_its_short_axis_detail_on_the_footprint_path() {
    grazing(Some(true));
}

fn grazing(footprint: Option<bool>) {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let z = 0.0;
    let dist = distances(z);
    let mut run = Run::new(&device, footprint);
    let iso = hf_energy(&run.frame(&device, z, 0, 1, false), &dist);
    let a8 = hf_energy(&run.frame(&device, z, 8, 1, false), &dist);
    let a16 = hf_energy(&run.frame(&device, z, 16, 1, false), &dist);
    let reference = hf_energy(&downsample(&run.frame(&device, z, 16, 4, false), 4), &dist);
    eprintln!(
        "far band HF energy (footprint path {footprint:?}, sampler LOD offset {:?}): isotropic {iso:.2}, 8x {a8:.2}, 16x {a16:.2}, \
         4x supersampled {reference:.2}",
        regia_terrain::anisotropy_lod_offset()
    );
    assert!(a16 > iso * 1.3, "16x anisotropic {a16:.2} vs isotropic {iso:.2}");
    assert!(a8 > iso * 1.2, "8x anisotropic {a8:.2} vs isotropic {iso:.2}");
    assert!(a16 >= a8 * 0.98, "16x {a16:.2} under 8x {a8:.2}");
    assert!(
        a16 <= reference * 1.15,
        "16x anisotropic {a16:.2} over the supersampled reference {reference:.2} (aliasing)"
    );
}

#[test]
fn taau_motion_keeps_detail_without_shimmer() {
    taau_motion(None);
}

#[test]
fn taau_motion_keeps_detail_without_shimmer_on_the_footprint_path() {
    taau_motion(Some(true));
}

fn taau_motion(footprint: Option<bool>) {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    const FRAMES: usize = 12;
    // The camera dollies forward 5 cm a frame; the last frames are compared.
    let dolly = |aniso: u8| {
        let mut run = Run::new(&device, footprint);
        (0..FRAMES)
            .map(|i| run.frame(&device, -(i as f32) * 0.05, aniso, 1, true))
            .collect::<Vec<_>>()
    };
    let iso = dolly(0);
    let aniso = dolly(16);
    let z_last = -((FRAMES - 1) as f32) * 0.05;
    let dist = distances(z_last);
    let last = FRAMES - 1;
    let hf_iso = hf_energy(&iso[last], &dist);
    let hf_aniso = hf_energy(&aniso[last], &dist);
    let reference = hf_energy(
        &downsample(
            &Run::new(&device, footprint).frame(&device, z_last, 16, 4, false),
            4,
        ),
        &dist,
    );
    // Frame-to-frame change over the last four frames (motion plus any shimmer). The same
    // change in supersampled frames is the motion alone; un-accumulated 16x frames must not
    // exceed it (the filter itself does not alias in motion).
    let positions = last - 3..=last;
    let mut ss = Run::new(&device, footprint);
    let reference_frames: Vec<Vec<f32>> = positions
        .clone()
        .map(|i| downsample(&ss.frame(&device, -(i as f32) * 0.05, 16, 4, false), 4))
        .collect();
    let mut plain = Run::new(&device, footprint);
    let native_frames: Vec<Vec<f32>> = positions
        .map(|i| plain.frame(&device, -(i as f32) * 0.05, 16, 1, false))
        .collect();
    let mean_flicker = |frames: &[Vec<f32>]| {
        (0..3)
            .map(|k| {
                flicker(
                    &frames[k],
                    &frames[k + 1],
                    &distances(-((last - 3 + k + 1) as f32) * 0.05),
                )
            })
            .sum::<f32>()
            / 3.0
    };
    let fl_iso = mean_flicker(&iso[last - 3..]);
    let fl_aniso = mean_flicker(&aniso[last - 3..]);
    let fl_ref = mean_flicker(&reference_frames);
    let fl_native = mean_flicker(&native_frames);
    eprintln!(
        "TAAU 0.67 dolly (footprint path {footprint:?}, sampler LOD offset {:?}): HF isotropic {hf_iso:.2}, 16x {hf_aniso:.2}, reference {reference:.2}; \
         flicker isotropic {fl_iso:.3}, 16x {fl_aniso:.3}, supersampled {fl_ref:.3}, 16x without TAA {fl_native:.3}",
        regia_terrain::anisotropy_lod_offset()
    );
    assert!(
        hf_aniso > hf_iso * 2.0,
        "TAAU: 16x {hf_aniso:.2} vs isotropic {hf_iso:.2}"
    );
    assert!(
        hf_aniso <= reference * 1.15,
        "TAAU: 16x {hf_aniso:.2} over the supersampled reference {reference:.2} (aliasing)"
    );
    assert!(
        fl_native <= fl_ref,
        "16x frames change {fl_native:.3} a frame, the supersampled ones {fl_ref:.3} (aliasing in motion)"
    );
    // TAAU's jitter residue grows with the detail it keeps; per unit of detail the 16x frame
    // must not change more than the isotropic one (no shimmer spike).
    assert!(
        fl_aniso / hf_aniso <= fl_iso / hf_iso,
        "TAAU: 16x flicker {fl_aniso:.3} per HF {hf_aniso:.2} vs isotropic {fl_iso:.3} per {hf_iso:.2} (shimmer)"
    );
}

/// Compute samples of a 2D array texture whose mip `k` is red `32 k`, through `textureSampleGrad`
/// with each query's gradients (normalized) and either the isotropic or the 16x sampler; one
/// `vec4` per query: `g` = (gx, gy), `aniso.x` > 0.5 picks the 16x sampler.
const ARRAY_GRAD_PROBE: &str = r"
struct Q { g: vec4<f32>, aniso: vec4<f32> };
@group(0) @binding(0) var tex: texture_2d_array<f32>;
@group(0) @binding(1) var s_iso: sampler;
@group(0) @binding(2) var s_aniso: sampler;
@group(0) @binding(3) var<storage, read> qs: array<Q>;
@group(0) @binding(4) var<storage, read_write> out: array<vec4<f32>>;
@compute @workgroup_size(1) fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let q = qs[id.x];
    let uv = vec2<f32>(0.37, 0.61);
    if (q.aniso.x > 0.5) {
        out[id.x] = textureSampleGrad(tex, s_aniso, uv, 0, q.g.xy, q.g.zw);
    } else {
        out[id.x] = textureSampleGrad(tex, s_iso, uv, 0, q.g.xy, q.g.zw);
    }
}
";

/// Size of the probe texture's level 0 (texels).
const PROBE_SIZE: u32 = 256;

/// One probe query: gradients in level-0 texels (x, y axes) and whether the 16x sampler filters.
struct LodQuery {
    gx: [f32; 2],
    gy: [f32; 2],
    aniso: bool,
}

/// The mip level the adapter filters each query's footprint at (the blend of mip `k`'s red `32 k`).
fn array_grad_lods(device: &RenderDevice, queries: &[LodQuery]) -> Vec<f32> {
    let (d, q) = (&device.device, &device.queue);
    const LEVELS: u32 = 9;
    let tex = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("array grad probe"),
        size: wgpu::Extent3d {
            width: PROBE_SIZE,
            height: PROBE_SIZE,
            depth_or_array_layers: 2,
        },
        mip_level_count: LEVELS,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for k in 0..LEVELS {
        let s = (PROBE_SIZE >> k).max(1);
        let red = u8::try_from(32 * k).unwrap_or(u8::MAX);
        let px: Vec<u8> = (0..s * s * 2).flat_map(|_| [red, 0, 0, 255]).collect();
        q.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: k,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &px,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * s),
                rows_per_image: Some(s),
            },
            wgpu::Extent3d {
                width: s,
                height: s,
                depth_or_array_layers: 2,
            },
        );
    }
    let sampler = |anisotropy_clamp: u16| {
        d.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            anisotropy_clamp,
            ..Default::default()
        })
    };
    let (s_iso, s_aniso) = (sampler(1), sampler(16));
    let n = PROBE_SIZE as f32;
    let query_bytes: Vec<u8> = queries
        .iter()
        .flat_map(|c| {
            let a = if c.aniso { 1.0f32 } else { 0.0 };
            [
                c.gx[0] / n,
                c.gx[1] / n,
                c.gy[0] / n,
                c.gy[1] / n,
                a,
                0.0,
                0.0,
                0.0,
            ]
        })
        .flat_map(f32::to_le_bytes)
        .collect();
    let qs = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("array grad probe queries"),
        size: query_bytes.len() as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    q.write_buffer(&qs, 0, &query_bytes);
    let size = (queries.len() * 16) as u64;
    let out = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("array grad probe out"),
        size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let read = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("array grad probe read"),
        size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("array grad probe"),
        source: wgpu::ShaderSource::Wgsl(ARRAY_GRAD_PROBE.into()),
    });
    let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("array grad probe"),
        layout: None,
        module: &module,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    });
    let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("array grad probe"),
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
    let mut enc = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(u32::try_from(queries.len()).expect("query count"), 1, 1);
    }
    enc.copy_buffer_to_buffer(&out, 0, &read, 0, size);
    q.submit([enc.finish()]);
    read.slice(..)
        .map_async(wgpu::MapMode::Read, |r| r.expect("map the probe"));
    d.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = read.slice(..).get_mapped_range().expect("mapped probe");
    data.as_chunks::<16>()
        .0
        .iter()
        .map(|px| {
            let red = f32::from_le_bytes([px[0], px[1], px[2], px[3]]);
            red * 255.0 / 32.0
        })
        .collect()
}

#[test]
fn the_sampler_path_matches_the_driver() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let info = device.adapter.get_info();
    // An 8:1 footprint (8 texels along x, 1 along y).
    let lods = array_grad_lods(
        &device,
        &[
            LodQuery {
                gx: [8.0, 0.0],
                gy: [0.0, 1.0],
                aniso: false,
            },
            LodQuery {
                gx: [8.0, 0.0],
                gy: [0.0, 1.0],
                aniso: true,
            },
        ],
    );
    let (iso, aniso) = (lods[0], lods[1]);
    let auto = regia_terrain::anisotropy_footprint_for(
        info.vendor,
        info.backend == wgpu::Backend::Vulkan,
        &info.driver,
        &info.driver_info,
        None,
    );
    eprintln!(
        "{} ({:?}, {} {}): 8:1 array footprint at LOD {iso:.2} isotropic, {aniso:.2} with 16x; footprint path {auto}",
        info.name, info.backend, info.driver, info.driver_info
    );
    // The control: the isotropic sampler filters at the long axis' level (8 texels = LOD 3).
    assert!(
        (iso - 3.0).abs() < 0.5,
        "the isotropic probe read LOD {iso:.2}, not 3: the probe is wrong on this adapter"
    );
    // A driver that drops the 16x sampler's anisotropy here (LOD near 3, not near 0) must take
    // the footprint path, or its terrain is isotropic (ADR-3957).
    assert!(
        aniso < 1.5 || auto,
        "{} ({} {}) filters an explicit-gradient array sample at LOD {aniso:.2} with a 16x sampler \
         (isotropic {iso:.2}): add it to regia_terrain::anisotropy_footprint_for",
        info.name,
        info.driver,
        info.driver_info
    );
}

/// With the adapter's sampler-path LOD offset applied (`regia_terrain::anisotropy_lod_offset_for`,
/// ADR-5459), the 16x sampler never filters a footprint sharper than the isotropic sampler's exact
/// trilinear LOD. RADV blends toward the sharper mip once anisotropy is on (an isotropic
/// 1.25-texel footprint read LOD 0.15 with 16x, 0.32 isotropic, on the Steam Deck), which made its
/// terrain shimmer under TAAU; NVIDIA reads 0.64, lavapipe 0.28 (8-bit steps).
#[test]
fn the_sampler_lod_offset_matches_the_driver() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let info = device.adapter.get_info();
    let offset = regia_terrain::anisotropy_lod_offset_for(
        info.vendor,
        info.backend == wgpu::Backend::Vulkan,
        &info.driver,
        &info.driver_info,
        None,
    );
    let k = offset.exp2();
    // Isotropic footprints whose LOD falls between two mips (fractions 0.14, 0.32, 0.58), and the
    // terrain's 16:1 floor of the same short axes.
    let mut queries = Vec::new();
    let mut cases = Vec::new();
    for minor in [1.1f32, 1.25, 1.5] {
        for ratio in [1.0f32, 16.0] {
            queries.push(LodQuery {
                gx: [minor * ratio, 0.0],
                gy: [0.0, minor],
                aniso: false,
            });
            queries.push(LodQuery {
                gx: [minor * ratio, 0.0],
                gy: [0.0, minor],
                aniso: true,
            });
            queries.push(LodQuery {
                gx: [minor * ratio * k, 0.0],
                gy: [0.0, minor * k],
                aniso: true,
            });
            cases.push((minor, ratio));
        }
    }
    let lods = array_grad_lods(&device, &queries);
    for (i, &(minor, ratio)) in cases.iter().enumerate() {
        let (iso, raw, engine) = (lods[3 * i], lods[3 * i + 1], lods[3 * i + 2]);
        // The exact trilinear LOD of the short axis, which the isotropic sampler reads for ratio 1
        // and the anisotropic one should read for both.
        let exact = minor.log2();
        eprintln!(
            "{} ({} {}): {ratio}:1 footprint, short axis {minor} texels (LOD {exact:.2}): isotropic sampler \
             {iso:.2}, 16x {raw:.2}, 16x with the terrain's offset {offset:.2}: {engine:.2}",
            info.name, info.driver, info.driver_info
        );
        if ratio == 1.0 {
            assert!(
                (iso - exact).abs() < 0.1,
                "the isotropic probe read LOD {iso:.2}, not {exact:.2}: the probe is wrong on this adapter"
            );
        }
        // Readback steps are 1/32 LOD (8-bit red); lavapipe reads 0.28 for 0.32.
        assert!(
            engine >= exact - 0.08,
            "{} ({} {}) filters a {ratio}:1 footprint with a {minor}-texel short axis at LOD {engine:.2} \
             through the terrain's 16x sampler path (offset {offset:.2}), sharper than its trilinear LOD \
             {exact:.2}: give it a sampler LOD offset in regia_terrain::anisotropy_lod_offset_for",
            info.name,
            info.driver,
            info.driver_info
        );
    }
}

/// ADR-5797: with the offset a terrain draw starts from on this device
/// (`regia_terrain::anisotropy_lod_offset_for_device`: the device's own 16x sampler measured over a
/// 2D array, the listed value where it cannot be measured), the 16x sampler never filters a
/// footprint sharper than the isotropic sampler's exact trilinear LOD. The listed value alone
/// (`the_sampler_lod_offset_matches_the_driver`) names Mesa RADV; a Pixel 9 Pro's Mali-G715 blends
/// the same way (an isotropic 1.25-texel footprint at LOD 0.14 for an exact 0.32) and had no entry.
/// A new terrain draw carries that offset.
#[test]
fn the_device_sampler_lod_offset_keeps_the_sampler_path_at_trilinear() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let info = device.adapter.get_info();
    let offset = regia_terrain::anisotropy_lod_offset_for_device(&device);
    assert!(
        (0.0..=regia_terrain::MAX_ANISO_LOD_OFFSET).contains(&offset),
        "offset {offset}"
    );
    // The same twice: one measurement per adapter and process.
    assert_eq!(regia_terrain::anisotropy_lod_offset_for_device(&device), offset);
    let k = offset.exp2();
    let mut queries = Vec::new();
    let mut cases = Vec::new();
    for minor in [1.1f32, 1.19, 1.25, 1.35, 1.5] {
        for ratio in [1.0f32, 16.0] {
            queries.push(LodQuery {
                gx: [minor * ratio, 0.0],
                gy: [0.0, minor],
                aniso: false,
            });
            queries.push(LodQuery {
                gx: [minor * ratio * k, 0.0],
                gy: [0.0, minor * k],
                aniso: true,
            });
            cases.push((minor, ratio));
        }
    }
    let lods = array_grad_lods(&device, &queries);
    for (i, &(minor, ratio)) in cases.iter().enumerate() {
        let (iso, engine) = (lods[2 * i], lods[2 * i + 1]);
        let exact = minor.log2();
        eprintln!(
            "{} ({} {}): {ratio}:1 footprint, short axis {minor} texels (LOD {exact:.2}): isotropic sampler \
             {iso:.2}, 16x with the device's offset {offset:.3}: {engine:.2}",
            info.name, info.driver, info.driver_info
        );
        if ratio == 1.0 {
            assert!(
                (iso - exact).abs() < 0.1,
                "the isotropic probe read LOD {iso:.2}, not {exact:.2}: the probe is wrong on this adapter"
            );
        }
        // Readback steps are 1/32 LOD (8-bit red); lavapipe reads 0.28 for 0.32.
        assert!(
            engine >= exact - 0.08,
            "{} ({} {}) filters a {ratio}:1 footprint with a {minor}-texel short axis at LOD {engine:.2} \
             through the terrain's 16x sampler path with the device's offset {offset:.3}, sharper than its \
             trilinear LOD {exact:.2}",
            info.name,
            info.driver,
            info.driver_info
        );
    }
    let draw = TerrainSharedDraw::new(&device, map(), &layers(), CdlodConfig::default(), 5).expect("terrain");
    assert_eq!(draw.gpu().anisotropy_lod_offset(), offset);
}

impl Run {
    /// A renderer whose terrain takes the sampler path (`footprint` false) or the footprint path
    /// whatever the adapter would pick, with the sampler path's LOD offset set to `mips`
    /// (`TerrainGpu::set_anisotropy_lod_offset`, ADR-5459).
    fn with_lod_offset(device: &RenderDevice, footprint: bool, mips: f32) -> Self {
        let mut draw =
            TerrainSharedDraw::new(device, map(), &layers(), CdlodConfig::default(), 5).expect("terrain");
        draw.set_material(&material(), &[]).expect("material");
        draw.gpu_mut().set_anisotropy_footprint(footprint);
        draw.gpu_mut().set_anisotropy_lod_offset(mips);
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
        Self { renderer }
    }
}

/// The sampler path's LOD offset (ADR-5459) through the terrain's own shader, on whatever adapter
/// runs this (only RADV gets an offset by default, so no lavapipe or NVIDIA frame took that path):
/// the far band of a 16x frame is softer for each larger offset, by about what a quarter mip is
/// worth, while the isotropic frame and the footprint path do not change at all. The offset is
/// added where the gradients are made (`aniso_grads`, once per projection), not at each tap
/// (ADR-5943); this holds the look ADR-5459 measured on the Steam Deck to that change.
#[test]
fn the_sampler_lod_offset_softens_the_sampler_path() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let info = device.adapter.get_info();
    let z = 0.0;
    let dist = distances(z);
    let frame = |footprint: bool, mips: f32, aniso: u8| {
        Run::with_lod_offset(&device, footprint, mips).frame(&device, z, aniso, 1, false)
    };
    const OFFSETS: [f32; 4] = [0.0, 0.25, 0.5, 1.0];
    let frames: Vec<Vec<f32>> = OFFSETS.iter().map(|&mips| frame(false, mips, 16)).collect();
    let hf: Vec<f32> = frames.iter().map(|luma| hf_energy(luma, &dist)).collect();
    let iso = frame(false, 0.0, 0);
    let hf_iso = hf_energy(&iso, &dist);
    // The hashes let two builds' frames be compared from their logs (a frame that must not move
    // prints the same hash before and after a shader change).
    let hashes: Vec<String> = frames
        .iter()
        .map(|luma| format!("{:016x}", frame_hash(luma)))
        .collect();
    eprintln!(
        "{} ({} {}): far band HF energy on the sampler path at 16x with LOD offsets {OFFSETS:?}: {hf:.2?}; \
         isotropic {hf_iso:.2}; frame hashes {hashes:?}, isotropic {:016x}",
        info.name,
        info.driver,
        info.driver_info,
        frame_hash(&iso)
    );
    // The same frame twice is the same bytes: the comparisons below are exact.
    assert!(
        frame(false, 0.25, 16) == frame(false, 0.25, 16),
        "two renders of one sampler-path frame differ: the exact comparisons below cannot hold"
    );
    // A driver that drops the sampler's anisotropy for array textures (the footprint path's
    // adapters, ADR-3957) filters the sampler path at the long axis' level, where the far band
    // has nothing left to soften: only the exact comparisons apply there.
    let drops_anisotropy = regia_terrain::anisotropy_footprint_for(
        info.vendor,
        info.backend == wgpu::Backend::Vulkan,
        &info.driver,
        &info.driver_info,
        None,
    );
    if !drops_anisotropy {
        for pair in hf.windows(2).zip(OFFSETS.windows(2)) {
            let ((before, after), (from, to)) = ((pair.0[0], pair.0[1]), (pair.1[0], pair.1[1]));
            assert!(
                after < before * SOFTER_PER_STEP,
                "LOD offset {to} keeps {after:.2} of the far band's detail, offset {from} {before:.2}: \
                 the sampler path must filter softer for a larger offset (ADR-5459)"
            );
        }
        // A quarter mip is a small step, not a different filter: it keeps most of the detail
        // (`QUARTER_MIP_KEEPS`), and a full mip still keeps more than the isotropic filter does.
        assert!(
            hf[1] > hf[0] * QUARTER_MIP_KEEPS,
            "LOD offset 0.25 keeps {:.2} of {:.2}: more than a quarter mip was taken",
            hf[1],
            hf[0]
        );
        assert!(
            hf[3] > hf_iso,
            "LOD offset 1 keeps {:.2}, under the isotropic frame's {hf_iso:.2}",
            hf[3]
        );
    }
    // The offset is the anisotropic sampler tap's alone.
    assert!(
        frame(false, 1.0, 0) == iso,
        "the isotropic frame changed with the sampler path's LOD offset"
    );
    assert!(
        frame(true, 1.0, 16) == frame(true, 0.0, 16),
        "the footprint path changed with the sampler path's LOD offset"
    );
}

/// FNV-1a over a frame's luma bits.
fn frame_hash(luma: &[f32]) -> u64 {
    luma.iter()
        .flat_map(|v| v.to_bits().to_le_bytes())
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

/// Each offset step of `the_sampler_lod_offset_softens_the_sampler_path` (0 to 0.25, to 0.5, to 1)
/// keeps at most this share of the step before's far-band detail. Measured: 0.96, 0.95, 0.88 on
/// lavapipe (Mesa 26.1.6), 0.76, 0.76, 0.73 on an RTX 5090 (NVIDIA 615.71), 0.83, 0.78, 0.63 on the
/// Steam Deck (RADV, Mesa 26.0).
const SOFTER_PER_STEP: f32 = 0.98;

/// LOD offset 0.25 keeps at least this share of offset 0's far-band detail. Measured: 0.96 on
/// lavapipe, 0.76 on an RTX 5090, 0.83 on the Steam Deck (48.86 to 40.40, the two values of
/// ADR-5459's sweep). An offset applied twice would keep what offset 0.5 keeps: 0.57 on the RTX
/// 5090 and 0.65 on the Deck, both under this bar.
const QUARTER_MIP_KEEPS: f32 = 0.7;
