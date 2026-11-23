// SPDX-License-Identifier: MPL-2.0
//! Material texture streaming (`regia_render::texture_stream`): low mips first, the rest
//! inside the per-frame byte budget, a fade, and — once settled under the desktop policy —
//! exactly the frame an unstreamed renderer draws. The tier cap keeps the top levels off the
//! GPU. Every allocation is visible in `memory_report`.

use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabTexture, LabVertex, MemoryCategory, RenderDevice, TextureStreaming,
};

/// A 4 m ground quad, UV 0..4 (1 m per UV unit), looked at from 1.5 m above.
fn quad() -> Vec<LabVertex> {
    [[-2.0f32, -2.0], [2.0, -2.0], [2.0, 2.0], [-2.0, 2.0]]
        .into_iter()
        .map(|[x, z]| LabVertex {
            position: [x, 0.0, z],
            normal: [0.0, 1.0, 0.0],
            uv: [x + 2.0, z + 2.0],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect()
}

/// A `size`² pattern with detail at every scale (each mip level differs from the next).
fn pattern(size: u32, seed: u32) -> LabTexture {
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let h =
                (x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663) ^ seed).wrapping_mul(2_654_435_761);
            let checker = if ((x / 8) + (y / 8)) % 2 == 0 { 200 } else { 60 };
            rgba.extend_from_slice(&[checker, (h >> 24) as u8, ((x * 255) / size) as u8, 255]);
        }
    }
    LabTexture {
        width: size,
        height: size,
        rgba,
    }
}

fn material() -> LabMaterial {
    LabMaterial {
        albedo: Some(pattern(512, 1)),
        occlusion_map: Some(pattern(256, 2)),
        roughness: 0.6,
        metallic: 0.0,
        ..LabMaterial::default()
    }
}

fn scene<'a>(
    vertices: &'a [LabVertex],
    indices: &'a [u32],
    material: &'a LabMaterial,
    streaming: TextureStreaming,
) -> FrameScene<'a> {
    FrameScene {
        width: 96,
        height: 96,
        eye: [0.3, 1.5, 0.4],
        target: [0.0, 0.0, -0.4],
        fov_y: 1.0,
        near: 0.05,
        far: 50.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.3, -1.0, 0.2], [1.0, 1.0, 1.0], 1.0),
        local_lights: &[],
        quality: FrameQuality {
            texture_streaming: streaming,
            ..FrameQuality::LAB
        },
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 1,
            object_key: 1,
            vertices,
            indices,
            material,
            model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }],
    }
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// Small numbers so the test streams over several frames: 32 px at first use, 96 KiB a
/// frame (the 512² level alone is 1 MiB), a 4-frame fade.
fn policy(resident_all: bool, max_px: u32) -> TextureStreaming {
    TextureStreaming {
        enabled: true,
        initial_max_px: 32,
        max_px,
        upload_kib_per_frame: 96,
        upload_us_per_frame: 1_000_000_000,
        fade_frames: 4,
        need_margin_percent: 200,
        resident_all,
        budget_mib: 0,
        eager: false,
    }
}

fn max_diff(a: &[u8], b: &[u8]) -> u8 {
    a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn settled_desktop_streaming_draws_the_unstreamed_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (vertices, indices) = (quad(), [0u32, 2, 1, 0, 3, 2]);
    let material = material();
    let reference = FrameRenderer::new()
        .render_offscreen(
            &device,
            &scene(&vertices, &indices, &material, TextureStreaming::OFF),
        )
        .expect("unstreamed frame")
        .pixels;

    let streamed = scene(&vertices, &indices, &material, policy(true, 0));
    let mut renderer = FrameRenderer::new();
    let first = renderer
        .render_offscreen(&device, &streamed)
        .expect("frame 0")
        .pixels;
    assert!(
        max_diff(&first, &reference) > 8,
        "the first frame draws the 32 px low mips (blurrier than the reference)"
    );
    let mut frames = 1;
    let mut saw_fade = false;
    let mut last = first;
    while frames < 200 {
        last = renderer
            .render_offscreen(&device, &streamed)
            .expect("frame")
            .pixels;
        frames += 1;
        let report = renderer.stream_report();
        assert_eq!(report.materials, 1);
        assert!(
            report.uploaded_bytes <= 96 * 1024,
            "frame {frames} streamed {} bytes over the 96 KiB budget",
            report.uploaded_bytes
        );
        saw_fade |= report.fading > 0;
        if report.settled == 1 {
            break;
        }
    }
    assert!(
        frames > 5,
        "1.4 MiB at 96 KiB a frame takes more than a few frames ({frames})"
    );
    assert!(saw_fade, "a completed set fades in");
    let settled = renderer
        .render_offscreen(&device, &streamed)
        .expect("settled")
        .pixels;
    assert_eq!(
        max_diff(&settled, &reference),
        0,
        "settled desktop streaming must draw the unstreamed frame exactly"
    );
    assert_eq!(max_diff(&last, &settled), 0);
}

#[test]
fn the_tier_cap_keeps_top_levels_off_the_gpu() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (vertices, indices) = (quad(), [0u32, 2, 1, 0, 3, 2]);
    let material = material();
    let capped = scene(&vertices, &indices, &material, policy(true, 128));
    let mut renderer = FrameRenderer::new();
    for _ in 0..60 {
        renderer.render_offscreen(&device, &capped).expect("frame");
        if renderer.stream_report().settled == 1 {
            break;
        }
    }
    assert_eq!(renderer.stream_report().settled, 1);
    let report = regia_render::memory_report(&device);
    if !report.available {
        return; // GL / Metal: no allocator report.
    }
    let streamed: u64 = report
        .labels
        .iter()
        .filter(|l| l.label == "material texture (streamed)")
        .map(|l| l.bytes)
        .sum();
    // 128² + 128² RGBA8 with mips ≈ 171 KiB (allocator alignment adds a little); the
    // uncapped 512² + 256² set is ≈ 1.7 MiB.
    assert!(streamed > 0, "streamed textures are labeled and categorized");
    assert!(
        streamed < 512 * 1024,
        "capped at 128 px: {streamed} bytes resident"
    );
    assert!(
        report.bytes(MemoryCategory::Meshes) >= streamed,
        "streamed material textures count as meshes"
    );
}

#[test]
fn need_based_streaming_stops_at_what_the_view_needs() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (vertices, indices) = (quad(), [0u32, 2, 1, 0, 3, 2]);
    let material = material();
    // The quad seen from 400 m away covers a few pixels: the 32 px set is already enough.
    let mut far = scene(&vertices, &indices, &material, policy(false, 0));
    far.eye = [0.0, 400.0, 0.1];
    far.far = 1000.0;
    let mut renderer = FrameRenderer::new();
    for _ in 0..20 {
        renderer.render_offscreen(&device, &far).expect("frame");
    }
    let report = renderer.stream_report();
    assert_eq!(report.pending + report.fading, 0, "nothing to stream far away");
    assert!(
        report.levels_below_cap > 0,
        "the far quad keeps its low mips only"
    );
}

/// A `size`² BC1 (sRGB) chain down to 1×1 with pseudo-random blocks: any 8 bytes are a valid
/// BC1 block, and every level differs from the next.
fn bc1_chain(size: u32) -> regia_render::CompressedTexture {
    let mut levels = Vec::new();
    let mut state = 0x9e37_79b9u32;
    let mut s = size;
    loop {
        let blocks = s.div_ceil(4) * s.div_ceil(4);
        let level: Vec<u8> = (0..blocks * 8)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state >> 24) as u8
            })
            .collect();
        levels.push(level.into());
        if s == 1 {
            break;
        }
        s /= 2;
    }
    regia_render::CompressedTexture {
        format: wgpu::TextureFormat::Bc1RgbaUnormSrgb,
        width: size,
        height: size,
        levels,
        normal_layout: regia_render::NormalLayout::Xyz,
        mip_stream: None,
    }
}

#[test]
fn cooked_block_compressed_maps_stream_by_block_rows() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    if !device
        .device
        .features()
        .contains(wgpu::Features::TEXTURE_COMPRESSION_BC)
    {
        eprintln!("skipped: the adapter has no BC texture support");
        return;
    }
    let (vertices, indices) = (quad(), [0u32, 2, 1, 0, 3, 2]);
    // The cooked chain replaces the base colour; the material has no RGBA8 map there (a pak).
    let material = LabMaterial {
        compressed: Some(std::sync::Arc::new(regia_render::CompressedMaps {
            albedo: Some(bc1_chain(1024)),
            ..Default::default()
        })),
        roughness: 0.6,
        metallic: 0.0,
        ..LabMaterial::default()
    };
    let reference = FrameRenderer::new()
        .render_offscreen(
            &device,
            &scene(&vertices, &indices, &material, TextureStreaming::OFF),
        )
        .expect("unstreamed frame")
        .pixels;
    // 64 KiB a frame: the smallest budget a policy allows.
    let streaming = TextureStreaming {
        upload_kib_per_frame: 64,
        ..policy(true, 0)
    };
    let streamed = scene(&vertices, &indices, &material, streaming);
    let mut renderer = FrameRenderer::new();
    let first = renderer
        .render_offscreen(&device, &streamed)
        .expect("frame 0")
        .pixels;
    assert!(
        max_diff(&first, &reference) > 8,
        "the first frame draws the 32 px low mips of the cooked chain"
    );
    let mut frames = 1;
    while frames < 200 {
        renderer.render_offscreen(&device, &streamed).expect("frame");
        frames += 1;
        let report = renderer.stream_report();
        assert_eq!(report.materials, 1, "the cooked chain is streamed");
        assert!(
            report.uploaded_bytes <= 64 * 1024,
            "frame {frames} streamed {} bytes over the 64 KiB budget",
            report.uploaded_bytes
        );
        if report.settled == 1 {
            break;
        }
    }
    // 1024² BC1 is 512 KiB at level 0 (≈ 680 KiB with mips) at 64 KiB a frame.
    assert!(frames > 8, "the chain streams over many frames ({frames})");
    let settled = renderer
        .render_offscreen(&device, &streamed)
        .expect("settled")
        .pixels;
    assert_eq!(
        max_diff(&settled, &reference),
        0,
        "a settled cooked chain must draw the whole-chain upload exactly"
    );
}

/// Need-driven growth (the budgeted presets' policy) from the floor to the authored size takes
/// several one-level steps after the first frame dropped the CPU chain. Every step must upload
/// every level of its set: the settled frame is the unstreamed frame exactly. (hero-regress:
/// the second step read a level the first had freed, uploaded nothing for it, and swapped in
/// uninitialised — black — mips; a DO27 hero's dome rendered black after a camera change.)
#[test]
fn need_driven_growth_over_several_levels_uploads_every_level() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (vertices, indices) = (quad(), [0u32, 2, 1, 0, 3, 2]);
    let material = material();
    let reference = FrameRenderer::new()
        .render_offscreen(
            &device,
            &scene(&vertices, &indices, &material, TextureStreaming::OFF),
        )
        .expect("unstreamed frame")
        .pixels;
    // The near view needs the full 512 px: growth runs 32 → 64 → 128 → 256 → 512.
    let streamed = scene(&vertices, &indices, &material, policy(false, 0));
    let mut renderer = FrameRenderer::new();
    let mut frames = 0;
    let mut swaps = 0;
    while frames < 200 {
        renderer.render_offscreen(&device, &streamed).expect("frame");
        frames += 1;
        let report = renderer.stream_report();
        swaps += report.swapped;
        if report.settled == 1 {
            break;
        }
    }
    assert_eq!(
        renderer.stream_report().settled,
        1,
        "the near view streams the whole chain in ({frames} frames)"
    );
    assert!(swaps >= 4, "growth took one level a step ({swaps} swaps)");
    let settled = renderer
        .render_offscreen(&device, &streamed)
        .expect("settled")
        .pixels;
    assert_eq!(
        max_diff(&settled, &reference),
        0,
        "need-driven streaming settled at the authored size must draw the unstreamed frame exactly"
    );
}
