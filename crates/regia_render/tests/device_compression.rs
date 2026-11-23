// SPDX-License-Identifier: MPL-2.0
//! Engine devices enable the block-compressed texture features their adapter has, and the
//! frame then draws a material's pre-compressed map instead of falling back to RGBA8.
//!
//! Regression: `RenderDevice::new` never requested `TEXTURE_COMPRESSION_BC` (nor ASTC or
//! ETC2), so every engine-created device (the runtime's, DO27's shell) uploaded uncompressed
//! maps; only harnesses that requested BC themselves got the compressed path.

use std::sync::Arc;

use regia_render::{
    CompressedMaps, CompressedTexture, DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality,
    FrameRenderer, FrameScene, LabMaterial, LabVertex, RenderDevice,
};

const SIZE: u32 = 32;

fn device() -> RenderDevice {
    RenderDevice::headless().unwrap_or_else(|err| panic!("no headless GPU device (lavapipe?): {err}"))
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn the_device_enables_every_compression_feature_the_adapter_has() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let adapter = device.adapter.features();
    let enabled = device.device.features();
    for feature in [
        wgpu::Features::TEXTURE_COMPRESSION_BC,
        wgpu::Features::TEXTURE_COMPRESSION_ASTC,
        wgpu::Features::TEXTURE_COMPRESSION_ETC2,
    ] {
        assert_eq!(
            enabled.contains(feature),
            adapter.contains(feature),
            "{feature:?}: adapter has it = {}, device enabled it = {}",
            adapter.contains(feature),
            enabled.contains(feature)
        );
    }
    assert_eq!(
        device.capabilities.texture_compression_bc,
        adapter.contains(wgpu::Features::TEXTURE_COMPRESSION_BC)
    );
}

fn quad() -> Vec<LabVertex> {
    [[-2.0f32, -2.0], [2.0, -2.0], [2.0, 2.0], [-2.0, 2.0]]
        .into_iter()
        .map(|[x, z]| LabVertex {
            position: [x, 0.0, z],
            normal: [0.0, 1.0, 0.0],
            uv: [(x + 2.0) / 4.0, (z + 2.0) / 4.0],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect()
}

fn render(device: &RenderDevice, material: &LabMaterial) -> Vec<u8> {
    let vertices = quad();
    let indices = [0u32, 2, 1, 0, 3, 2];
    let scene = FrameScene {
        width: SIZE,
        height: SIZE,
        eye: [0.0, 3.0, 0.01],
        target: [0.0, 0.0, 0.0],
        fov_y: 1.1,
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
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0, 1.0, 1.0], 1.0),
        local_lights: &[],
        quality: FrameQuality::LAB,
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 1,
            object_key: 1,
            vertices: &vertices,
            indices: &indices,
            material,
            model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }],
    };
    FrameRenderer::new()
        .render_offscreen(device, &scene)
        .unwrap_or_else(|err| panic!("frame: {err}"))
        .pixels
}

/// Mean (r, g, b) of the frame.
fn mean_rgb(pixels: &[u8]) -> [f64; 3] {
    let mut sum = [0.0f64; 3];
    let n = pixels.len() / 4;
    for p in pixels.as_chunks::<4>().0 {
        for c in 0..3 {
            sum[c] += f64::from(p[c]);
        }
    }
    sum.map(|s| s / n.max(1) as f64)
}

#[test]
fn an_engine_device_draws_the_bc_map_not_the_rgba8_fallback() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    if !device
        .adapter
        .features()
        .contains(wgpu::Features::TEXTURE_COMPRESSION_BC)
    {
        eprintln!("skip: the adapter has no BC texture compression");
        return;
    }
    // One 4×4 BC1 block, solid pure red: colour0 = RGB565 0xF800 > colour1 = 0, every index 0.
    let red_block = vec![0x00, 0xF8, 0x00, 0x00, 0, 0, 0, 0];
    let map = CompressedTexture {
        format: wgpu::TextureFormat::Bc1RgbaUnormSrgb,
        width: 4,
        height: 4,
        levels: vec![red_block.into()],
        normal_layout: regia_render::NormalLayout::Xyz,
        mip_stream: None,
    };
    let material = LabMaterial {
        albedo: None,
        roughness: 0.8,
        metallic: 0.0,
        compressed: Some(Arc::new(CompressedMaps {
            albedo: Some(map),
            ..CompressedMaps::default()
        })),
        ..LabMaterial::default()
    };
    let [r, g, b] = mean_rgb(&render(&device, &material));
    // The fallback (no BC on the device) samples the white default and prints grey.
    assert!(
        r > 2.0 * g.max(b) + 10.0,
        "the frame drew the BC1 red map (mean rgb {r:.0} {g:.0} {b:.0}); a gray frame is the RGBA8 fallback"
    );
}

/// The phone path (ADR-1749): the Android cook writes ASTC, and the arena APK has no textures, so
/// `cargo xtask android-pass` cannot see a map drawn. Built for aarch64 and run on the phone
/// (`cargo ndk -t arm64-v8a test -p regia_render --test device_compression --no-run`, then the
/// binary from `/data/local/tmp`); it skips on an adapter without ASTC.
#[test]
fn an_engine_device_draws_the_astc_map_not_the_rgba8_fallback() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    if !device
        .adapter
        .features()
        .contains(wgpu::Features::TEXTURE_COMPRESSION_ASTC)
    {
        eprintln!("skip: the adapter has no ASTC texture compression");
        return;
    }
    // One ASTC 4×4 void-extent block (Khronos Data Format Specification, ASTC "void-extent
    // blocks"): bits 0-8 = 0x1FC, bit 9 = 0 (LDR), bits 10-11 = 1, the extent all ones (no
    // extent), then R, G, B, A as UNORM16: pure red, opaque.
    let red_block = vec![
        0xFC, 0xFD, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF,
    ];
    let map = CompressedTexture {
        format: wgpu::TextureFormat::Astc {
            block: wgpu::AstcBlock::B4x4,
            channel: wgpu::AstcChannel::UnormSrgb,
        },
        width: 4,
        height: 4,
        levels: vec![red_block.into()],
        normal_layout: regia_render::NormalLayout::Xyz,
        mip_stream: None,
    };
    let material = LabMaterial {
        albedo: None,
        roughness: 0.8,
        metallic: 0.0,
        compressed: Some(Arc::new(CompressedMaps {
            albedo: Some(map),
            ..CompressedMaps::default()
        })),
        ..LabMaterial::default()
    };
    let [r, g, b] = mean_rgb(&render(&device, &material));
    assert!(
        r > 2.0 * g.max(b) + 10.0,
        "the frame drew the ASTC red map (mean rgb {r:.0} {g:.0} {b:.0}); a gray frame is the RGBA8 fallback"
    );
}

/// A material whose only map is one solid 4×4 block of `format`.
fn solid_block_material(format: wgpu::TextureFormat, block: Vec<u8>) -> LabMaterial {
    LabMaterial {
        albedo: None,
        roughness: 0.8,
        metallic: 0.0,
        compressed: Some(Arc::new(CompressedMaps {
            albedo: Some(CompressedTexture {
                format,
                width: 4,
                height: 4,
                levels: vec![block.into()],
                normal_layout: regia_render::NormalLayout::Xyz,
                mip_stream: None,
            }),
            ..CompressedMaps::default()
        })),
        ..LabMaterial::default()
    }
}

/// The three families' red blocks: BC1, an ASTC void-extent, an ETC2 individual-mode block
/// (base colours 0xF, 0, 0; modifier +2).
fn red_blocks() -> [(wgpu::TextureFormat, Vec<u8>); 3] {
    [
        (
            wgpu::TextureFormat::Bc1RgbaUnormSrgb,
            vec![0x00, 0xF8, 0x00, 0x00, 0, 0, 0, 0],
        ),
        (
            wgpu::TextureFormat::Astc {
                block: wgpu::AstcBlock::B4x4,
                channel: wgpu::AstcChannel::UnormSrgb,
            },
            vec![
                0xFC, 0xFD, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0xFF,
                0xFF,
            ],
        ),
        (
            wgpu::TextureFormat::Etc2Rgb8UnormSrgb,
            vec![0xFF, 0, 0, 0, 0, 0, 0, 0],
        ),
    ]
}

/// The loader's choice end to end (ADR-2063): on a device clamped to a profile that lacks a family
/// (Pascal: no ASTC or ETC2; Mobile: no BC) the frame draws the map through the CPU decode, on one
/// that has it through the blocks, and `texture_path` names which. Either way the frame shows the
/// map's red, not the white default a missing map samples, and the two paths agree within a few
/// levels. On lavapipe (BC only) BC takes both paths and ASTC and ETC2 always decode; on a phone
/// (built as the ASTC test above says) ASTC and ETC2 take both paths and BC always decodes.
#[test]
fn a_frame_draws_a_map_natively_or_decoded_by_the_adapter_features() {
    use regia_render::compressed_texture::{TexturePath, texture_path};
    use regia_render::minspec::MinspecProfile;
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut frames: Vec<(wgpu::TextureFormat, TexturePath, [f64; 3])> = Vec::new();
    for profile in [MinspecProfile::Pascal, MinspecProfile::Mobile] {
        let device = RenderDevice::headless_with_profile(profile)
            .unwrap_or_else(|err| panic!("{profile:?} device: {err}"));
        for (format, block) in red_blocks() {
            let path = texture_path(format, device.device.features());
            assert_ne!(path, TexturePath::Unbound, "{format:?} has a decoder");
            let [r, g, b] = mean_rgb(&render(&device, &solid_block_material(format, block)));
            assert!(
                r > 2.0 * g.max(b) + 10.0,
                "{profile:?} {format:?} ({path:?}) drew the red map (mean rgb {r:.0} {g:.0} {b:.0})"
            );
            frames.push((format, path, [r, g, b]));
        }
    }
    // Pascal lacks ASTC and ETC2 and Mobile lacks BC whatever the adapter has, so each family took
    // the decode at least once.
    for (format, _) in red_blocks() {
        assert!(
            frames
                .iter()
                .any(|(f, path, _)| *f == format && *path == TexturePath::CpuDecode),
            "{format:?} never decoded: {frames:?}"
        );
    }
    // Where the adapter has the family too, both paths drew it: the same red.
    for (format, path, rgb) in &frames {
        for (_, _, other) in frames.iter().filter(|(f, p, _)| f == format && p != path) {
            let delta = rgb
                .iter()
                .zip(other)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f64, f64::max);
            assert!(delta < 4.0, "{format:?}: decoded and native differ by {delta:.1}");
        }
    }
}
