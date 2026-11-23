// SPDX-License-Identifier: MPL-2.0
//! HDR headroom through the SGSR upscale.
//!
//! The post blit expands tonemapped highlights above the knee when the frame target is the
//! paper-white HDR intermediate. SGSR replaces that blit on a scaled mobile frame, so it has
//! to apply the same expansion. An SDR frame (no headroom) must stay the display-encoded image
//! the mobile-aa goldens blessed.

use regia_render::{
    DirectionalLightUniform, DisplayCalibration, FrameQuality, FrameRenderer, FrameTarget, LabMaterial,
    LabMesh, LabScene, LabVertex, PostSettings, RenderDevice, SpatialUpscaler,
    calibration::hdr_highlight_expand,
};

const W: u32 = 128;
const H: u32 = 96;

fn vert(position: [f32; 3]) -> LabVertex {
    LabVertex::from_pos_normal_uv(position, [0.0, 0.0, 1.0], [0.0, 0.0])
}

fn quad(c: [[f32; 2]; 4], z: f32, material: usize) -> LabMesh {
    LabMesh {
        vertices: c.iter().map(|p| vert([p[0], p[1], z])).collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

/// Dark wall filling the view, with a flat emissive panel in the middle. The panel tonemaps
/// to display white; the wall stays under the highlight knee.
fn highlight_scene(width: u32, height: u32) -> LabScene {
    LabScene {
        width,
        height,
        eye: [0.0, 0.0, 3.2],
        target: [0.0, 0.0, 0.0],
        fov_y: 1.0,
        near: 0.1,
        far: 20.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: Some(PostSettings::golden_deterministic()),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.02, 0.02, 0.02, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.0, 0.0, 0.0, 1.0],
                roughness: 1.0,
                emissive: [1.0, 1.0, 1.0],
                emissive_intensity: 80.0,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![
            quad([[-4.0, -3.0], [4.0, -3.0], [4.0, 3.0], [-4.0, 3.0]], -0.5, 0),
            quad([[-1.2, -0.9], [1.2, -0.9], [1.2, 0.9], [-1.2, 0.9]], 0.0, 1),
        ],
    }
}

fn quality(scale: f32, upscaler: SpatialUpscaler) -> FrameQuality {
    let mut q = FrameQuality::LAB;
    q.render_scale_permille = (scale * 1000.0).round() as u16;
    q.spatial_upscaler = upscaler;
    q
}

fn frame_scene(scene: &LabScene, q: FrameQuality) -> regia_render::FrameScene<'_> {
    scene.as_frame_scene_with_quality(q)
}

fn render_sdr(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    scene: &LabScene,
    q: FrameQuality,
) -> Vec<u8> {
    renderer.set_display_headroom(None);
    let fs = frame_scene(scene, q);
    renderer.render_offscreen(device, &fs).expect("sdr frame").pixels
}

fn render_hdr(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    scene: &LabScene,
    q: FrameQuality,
    headroom: f32,
) -> Vec<[f32; 3]> {
    renderer.set_display_headroom(Some(headroom));
    let format = wgpu::TextureFormat::Rgba16Float;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sgsr hdr"),
        size: wgpu::Extent3d {
            width: scene.width,
            height: scene.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let fs = frame_scene(scene, q);
    renderer
        .render_into(
            device,
            &fs,
            FrameTarget::full(&view, format, scene.width, scene.height).with_color(Some(&texture)),
        )
        .expect("hdr frame");
    read_rgba16f(device, &texture, scene.width, scene.height)
}

fn read_rgba16f(device: &RenderDevice, texture: &wgpu::Texture, width: u32, height: u32) -> Vec<[f32; 3]> {
    let row = width * 8;
    let padded = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("sgsr hdr readback"),
        size: u64::from(padded) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("sgsr hdr readback"),
        });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    device.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll readback");
    rx.recv().expect("map callback").expect("map readback");
    let mapped = buffer.slice(..).get_mapped_range().expect("mapped range");
    let mut rgb = Vec::with_capacity((width * height) as usize);
    for y in 0..height as usize {
        let row_bytes = &mapped[y * padded as usize..];
        for x in 0..width as usize {
            let o = x * 8;
            let h = |k: usize| {
                f16_to_f32(u16::from_le_bytes([
                    row_bytes[o + 2 * k],
                    row_bytes[o + 2 * k + 1],
                ]))
            };
            rgb.push([h(0), h(1), h(2)]);
        }
    }
    drop(mapped);
    buffer.unmap();
    rgb
}

fn f16_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from((h >> 10) & 0x1f);
    let mant = f32::from(h & 0x3ff);
    match exp {
        0 => sign * mant * 2f32.powi(-24),
        31 => {
            if mant == 0.0 {
                sign * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        e => sign * (1.0 + mant / 1024.0) * 2f32.powi(e - 15),
    }
}

fn channel_max(rgb: &[[f32; 3]]) -> f32 {
    rgb.iter().flat_map(|p| p.iter().copied()).fold(0.0_f32, f32::max)
}

/// `utils.wgsl` `decode_srgb` is `pow(c, 2.2)`, the decode the blit and SGSR share.
fn shader_decode(encoded: f32) -> f32 {
    encoded.max(0.0).powf(2.2)
}

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip sgsr hdr: {err}");
            None
        }
    }
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn hdr_sgsr_keeps_highlights_above_paper_white() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let headroom = DisplayCalibration::NEUTRAL.hdr_headroom();
    assert!(
        headroom > 1.5,
        "the neutral HDR grade has highlight room, got {headroom}"
    );
    let scene = highlight_scene(W, H);
    let mut renderer = FrameRenderer::new();

    let bilinear = render_hdr(
        &device,
        &mut renderer,
        &scene,
        quality(0.75, SpatialUpscaler::Bilinear),
        headroom,
    );
    let sgsr = render_hdr(
        &device,
        &mut renderer,
        &scene,
        quality(0.75, SpatialUpscaler::Sgsr),
        headroom,
    );
    let native = render_hdr(
        &device,
        &mut renderer,
        &scene,
        quality(1.0, SpatialUpscaler::Bilinear),
        headroom,
    );

    let (b_max, s_max, n_max) = (channel_max(&bilinear), channel_max(&sgsr), channel_max(&native));
    assert!(
        b_max > 1.0 && n_max > 1.0,
        "non-SGSR HDR path must expand highlights: bilinear {b_max}, native {n_max}"
    );
    assert!(
        s_max > 1.0,
        "SGSR clipped the HDR highlight to SDR (max {s_max}); bilinear reached {b_max}"
    );

    // Interior of the emissive panel. The two upscalers reconstruct the silhouette
    // differently (HDR bilinear before the tonemap, SGSR after it), so the check is the
    // texels whose whole neighbourhood is already above paper white on the non-SGSR path.
    let above = |p: &[f32; 3]| p[1] > 1.0;
    let at = |img: &[[f32; 3]], x: i32, y: i32| img[(y as u32 * W + x as u32) as usize];
    let mut kept = 0usize;
    let mut lost = 0usize;
    let mut worst = f32::MAX;
    for y in 4..H as i32 - 4 {
        for x in 4..W as i32 - 4 {
            let solid = (-4..=4).all(|dy| (-4..=4).all(|dx| above(&at(&bilinear, x + dx, y + dy))));
            if !solid {
                continue;
            }
            let g = at(&sgsr, x, y)[1];
            if g > 1.0 {
                kept += 1;
            } else {
                lost += 1;
                worst = worst.min(g);
            }
        }
    }
    assert!(
        kept > (W * H / 10) as usize,
        "expected a solid expanded panel, kept {kept} pixels (bilinear max {b_max})"
    );
    assert_eq!(
        lost, 0,
        "SGSR dropped {lost} interior highlight pixels to {worst} (bilinear peak {b_max})"
    );
    assert!(
        (s_max - b_max).abs() / b_max < 0.05,
        "SGSR peak {s_max} diverges from the bilinear HDR peak {b_max}"
    );

    // The wall stays at or under paper white on both paths.
    let wall = |rgb: &[[f32; 3]]| rgb[(H as usize - 2) * W as usize + 2];
    assert!(
        wall(&bilinear).iter().all(|c| *c <= 1.0) && wall(&sgsr).iter().all(|c| *c <= 1.0),
        "the dark wall was expanded: bilinear {:?} sgsr {:?}",
        wall(&bilinear),
        wall(&sgsr)
    );

    // Flat panel centre: SGSR of a constant encoded field plus the shared expansion
    // matches the CPU mirror of the blit (decode pow 2.2, then `hdr_highlight_expand`).
    let sdr = render_sdr(
        &device,
        &mut renderer,
        &scene,
        quality(0.75, SpatialUpscaler::Sgsr),
    );
    let centre = ((H / 2) * W + W / 2) as usize;
    let encoded = f32::from(sdr[centre * 4]) / 255.0;
    let expect = hdr_highlight_expand(shader_decode(encoded), headroom);
    let got = sgsr[centre][0];
    assert!(
        (got - expect).abs() < 0.02,
        "panel center {got} != expanded SDR {expect} (encoded {encoded}, headroom {headroom})"
    );
    assert!(got > 1.0, "panel center stayed in SDR range ({got})");
}

#[test]
fn sdr_sgsr_is_byte_identical_with_headroom_cleared() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let scene = highlight_scene(W, H);
    let q = quality(0.75, SpatialUpscaler::Sgsr);
    let mut renderer = FrameRenderer::new();
    let first = render_sdr(&device, &mut renderer, &scene, q);
    let _hdr = render_hdr(
        &device,
        &mut renderer,
        &scene,
        q,
        DisplayCalibration::NEUTRAL.hdr_headroom(),
    );
    let second = render_sdr(&device, &mut renderer, &scene, q);
    assert_eq!(
        first,
        second,
        "an SDR frame after an HDR one changed ({} bytes)",
        first.iter().zip(&second).filter(|(a, b)| a != b).count()
    );

    // The blessed mobile-aa frames (SMAA native, and Mobile High's SMAA + SGSR) are the
    // SDR images from before this change. SGSR's filter is untouched when headroom is absent.
    // They are lavapipe frames (ADR-025): the farm gate's real-GPU pass compares no goldens.
    // (`regia_test_utils::golden::SKIP_GOLDENS_ENV`; regia_render cannot depend on that crate.)
    if regia_core::brand::env_var("REGIA_SKIP_GOLDENS").as_deref() == Ok("1") {
        eprintln!("REGIA_SKIP_GOLDENS: blessed mobile-aa frames not compared on this adapter");
        return;
    }
    // Same rule without the env var: a byte compare against a lavapipe frame only holds on a
    // software rasterizer. The SDR-after-HDR identity above already ran on this adapter.
    let info = device.adapter.get_info();
    if info.device_type != wgpu::DeviceType::Cpu {
        eprintln!(
            "blessed mobile-aa frames not compared on {} (hardware GPU; goldens are lavapipe-blessed, ADR-025)",
            info.name
        );
        return;
    }
    for (name, scale, smaa, upscaler) in [
        ("spatial_aa_smaa_native", 1.0, true, SpatialUpscaler::Bilinear),
        ("spatial_aa_mobile_high", 0.85, true, SpatialUpscaler::Sgsr),
    ] {
        let mut q = quality(scale, upscaler);
        q.smaa = smaa;
        let scene = spatial_aa_scene();
        // A fresh renderer: the blessed frames were captured that way, with no leftover
        // post state from the highlight scene above.
        let pixels = render_sdr(&device, &mut FrameRenderer::new(), &scene, q);
        let blessed = read_golden_png(name);
        let differ = pixels.iter().zip(&blessed).filter(|(a, b)| a != b).count();
        assert_eq!(pixels.len(), blessed.len(), "{name} size");
        assert_eq!(differ, 0, "{name} SDR frame changed by {differ} bytes");
    }
}

/// The mobile-aa golden scene (`regia_test_utils` `spatial_aa_golden`). Kept here so the
/// blessed PNGs can be compared byte for byte without a second crate.
fn spatial_aa_scene() -> LabScene {
    let mut meshes = vec![quad(
        [[-4.0, -3.0], [4.0, -3.0], [4.0, 3.0], [-4.0, 3.0]],
        -0.5,
        0,
    )];
    meshes.push(quad(
        [[-1.9, -1.3], [-0.2, -1.1], [-0.2, -0.9], [-1.9, -1.2]],
        0.0,
        1,
    ));
    for k in 0..6 {
        let x = 0.2 + 0.22 * k as f32;
        let lean = 0.04 + 0.03 * k as f32;
        let w = 0.025;
        meshes.push(quad(
            [
                [x - w, -1.3],
                [x + w, -1.3],
                [x + w + lean, 1.3],
                [x - w + lean, 1.3],
            ],
            0.0,
            1,
        ));
    }
    meshes.push(quad(
        [[-1.6, 0.0], [-0.6, 0.35], [-0.95, 1.3], [-1.95, 0.95]],
        0.0,
        2,
    ));
    let material = |c: f32| LabMaterial {
        base_color: [c, c, c, 1.0],
        unlit: true,
        ..LabMaterial::default()
    };
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 3.2],
        target: [0.0, 0.0, 0.0],
        fov_y: 1.0,
        near: 0.1,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: Some(PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            exposure_ev: 0.0,
            tonemap: regia_render::TonemapOperator::AgX,
            fxaa: false,
            taa: false,
            film_grain: 0.0,
            vignette: 0.0,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![material(0.35), material(0.02), material(0.9)],
        meshes,
    }
}

fn read_golden_png(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../regia_test_utils/goldens")
        .join(format!("{name}.png"));
    let file = std::fs::File::open(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().expect("png info");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("png size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    buf.truncate(info.buffer_size());
    buf
}
