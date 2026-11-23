// SPDX-License-Identifier: MPL-2.0
//! The GPU resource inspector (`seeing.list_resources`, `seeing.dump_resource`): the registry
//! lists the renderer's persistent targets, a half-float texture round-trips bit-exact through
//! EXR, and the targets the GPU cannot copy answer with their documented error codes.

use regia_render::{
    DirectionalLightUniform, DumpFormat, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, box_mesh, dump_resource, list_resources,
    resource_inspector, set_debug_readback,
};

fn device() -> Option<RenderDevice> {
    match RenderDevice::shared_headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip resource_inspector: {err}");
            None
        }
    }
}

/// A texture of `format`, `w` x `h`, copyable out and writable, tracked under `label`.
fn tracked(
    device: &RenderDevice,
    label: &str,
    format: wgpu::TextureFormat,
    w: u32,
    h: u32,
    data: Option<(&[u8], u32)>,
) -> wgpu::Texture {
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    if let Some((bytes, row_bytes)) = data {
        device.queue.write_texture(
            texture.as_image_copy(),
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row_bytes),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
    }
    resource_inspector::track(label, &texture);
    texture
}

#[test]
fn list_contains_the_hdr_and_depth_targets_after_a_headless_frame() {
    let Some(device) = device() else { return };
    set_debug_readback(true);
    let (vertices, indices) = box_mesh([1.0, 1.0, 1.0]);
    let lab = LabScene {
        width: 96,
        height: 64,
        eye: [0.0, 1.5, 5.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: Some(PostSettings {
            auto_exposure: false,
            exposure_ev: 14.0,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([-0.4, -0.8, -0.3], [1.0; 3], 60_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: vec![LabMesh {
            vertices,
            indices,
            material: 0,
            ..LabMesh::unskinned()
        }],
    };
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    renderer.render_offscreen(&device, &frame).expect("frame");

    let listed = list_resources();
    let find = |label: &str| listed.iter().find(|r| r.label == label);
    let hdr = find("frame hdr").expect("the HDR target is listed");
    assert!(
        hdr.format.contains("Float") || hdr.format.contains("Ufloat"),
        "{}",
        hdr.format
    );
    assert_eq!(hdr.category.name(), "render_targets");
    assert!(hdr.bytes >= u64::from(hdr.width) * u64::from(hdr.height) * 4);
    assert!(hdr.copyable, "debug readback was on at creation");
    let depth = find("frame depth").expect("the depth target is listed");
    assert_eq!(depth.format, "Depth32Float");
    assert!(depth.copyable);
    assert_eq!(depth.dump_format, Some(DumpFormat::Exr));

    // The listed targets can be dumped as EXR files.
    for label in ["frame hdr", "frame depth"] {
        let dumped = dump_resource(&device, label, 0, 0, None).expect(label);
        assert_eq!(dumped.format, DumpFormat::Exr);
        assert_eq!(
            &dumped.bytes[..4],
            &20_000_630u32.to_le_bytes(),
            "{label}: EXR magic"
        );
    }
}

#[test]
fn rgba16float_pattern_round_trips_bit_exact_through_exr_half() {
    let Some(device) = device() else { return };
    set_debug_readback(true);
    const W: u32 = 8;
    const H: u32 = 4;
    // Finite halves only (exponent 1..=30): every texel and channel carries distinct bits.
    let half = |i: u32, k: u32| -> u16 {
        let exp = 1 + (i * 7 + k) % 30;
        let mant = (i * 131 + k * 17) % 1024;
        let sign = (i + k) & 1;
        ((sign << 15) | (exp << 10) | mant) as u16
    };
    let mut bytes = Vec::new();
    for i in 0..W * H {
        for k in 0..4 {
            bytes.extend_from_slice(&half(i, k).to_le_bytes());
        }
    }
    let label = "inspector test rgba16f pattern";
    tracked(
        &device,
        label,
        wgpu::TextureFormat::Rgba16Float,
        W,
        H,
        Some((&bytes, W * 8)),
    );
    let dumped = dump_resource(&device, label, 0, 0, Some(DumpFormat::Exr)).expect("dump");
    assert_eq!((dumped.width, dumped.height), (W, H));

    // Uncompressed scanlines at the end of the file: per row, 8 header bytes, then the planes
    // in name order A, B, G, R (texel channels 3, 2, 1, 0).
    let line_bytes = (4 * W * 2) as usize;
    for y in 0..H as usize {
        let start = dumped.bytes.len() - (H as usize - y) * (8 + line_bytes);
        let chunk = &dumped.bytes[start..start + 8 + line_bytes];
        assert_eq!(
            i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
            y as i32
        );
        let (pairs, rest) = chunk[8..].as_chunks::<2>();
        assert!(rest.is_empty());
        let mut at = 0;
        for k in [3u32, 2, 1, 0] {
            for x in 0..W {
                let want = half(y as u32 * W + x, k);
                assert_eq!(u16::from_le_bytes(pairs[at]), want, "row {y} channel {k} x {x}");
                at += 1;
            }
        }
    }
}

#[test]
fn rgba8_dumps_as_png_with_the_texels_unchanged() {
    let Some(device) = device() else { return };
    set_debug_readback(true);
    let pixels: Vec<u8> = (0..2 * 2 * 4).map(|v| (v * 11) as u8).collect();
    let label = "inspector test rgba8 pattern";
    tracked(
        &device,
        label,
        wgpu::TextureFormat::Rgba8Unorm,
        2,
        2,
        Some((&pixels, 2 * 4)),
    );
    let dumped = dump_resource(&device, label, 0, 0, None).expect("dump");
    assert_eq!(dumped.format, DumpFormat::Png);
    let decoder = png::Decoder::new(std::io::Cursor::new(dumped.bytes));
    let mut reader = decoder.read_info().expect("png header");
    let mut decoded = vec![0; reader.output_buffer_size().expect("png size")];
    reader.next_frame(&mut decoded).expect("png frame");
    assert_eq!(decoded, pixels);
}

#[test]
fn depth24plus_is_not_copyable_and_unknown_labels_are_not_found() {
    let Some(device) = device() else { return };
    set_debug_readback(true);
    let label = "inspector test depth24";
    let depth = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth24Plus,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    resource_inspector::track(label, &depth);
    let listed = list_resources();
    let entry = listed.iter().find(|r| r.label == label).expect("listed");
    assert!(!entry.copyable);
    assert_eq!(entry.dump_format, None);
    let err = dump_resource(&device, label, 0, 0, None).expect_err("depth24plus");
    assert_eq!(err.code(), "resource_not_copyable");

    let err = dump_resource(&device, "inspector test no such label", 0, 0, None).expect_err("missing");
    assert_eq!(err.code(), "resource_not_found");
}

#[test]
fn unsupported_formats_and_mismatched_requests_are_format_unsupported() {
    let Some(device) = device() else { return };
    set_debug_readback(true);
    let odd = "inspector test rgb10a2";
    tracked(&device, odd, wgpu::TextureFormat::Rgb10a2Unorm, 4, 4, None);
    let err = dump_resource(&device, odd, 0, 0, None).expect_err("no writer");
    assert_eq!(err.code(), "format_unsupported");

    let label = "inspector test r32f";
    tracked(&device, label, wgpu::TextureFormat::R32Float, 4, 4, None);
    let err = dump_resource(&device, label, 0, 0, Some(DumpFormat::Png)).expect_err("float as png");
    assert_eq!(err.code(), "format_unsupported");
    let err = dump_resource(&device, label, 3, 0, None).expect_err("mip out of range");
    assert_eq!(err.code(), "subresource_out_of_range");
    let err = dump_resource(&device, label, 0, 5, None).expect_err("layer out of range");
    assert_eq!(err.code(), "subresource_out_of_range");
    dump_resource(&device, label, 0, 0, Some(DumpFormat::Exr)).expect("r32float dumps as EXR");
}
