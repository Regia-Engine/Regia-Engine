// SPDX-License-Identifier: MPL-2.0
//! Forward shading integration tests on a real (or software) GPU — through
//! the same frame path games run.
//!
//! CI runs this on Mesa lavapipe (Linux) and WARP (Windows).

use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial, LabVertex,
    RenderDevice,
};

/// Helper: linear luminance from sRGB-encoded RGBA8.
fn luminance(rgba: &[u8; 4]) -> f32 {
    fn srgb_to_linear(c: u8) -> f32 {
        let c = c as f32 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * srgb_to_linear(rgba[0]) + 0.7152 * srgb_to_linear(rgba[1]) + 0.0722 * srgb_to_linear(rgba[2])
}

fn pixel_at(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let idx = ((y * width + x) * 4) as usize;
    pixels[idx..idx + 4]
        .try_into()
        .expect("pixel slice must be exactly 4 bytes")
}

fn avg_luminance_in_region(pixels: &[u8], width: u32, x0: u32, y0: u32, x1: u32, y1: u32) -> f32 {
    let mut sum = 0.0f32;
    let mut count = 0u32;
    for y in y0..y1 {
        for x in x0..x1 {
            sum += luminance(&pixel_at(pixels, width, x, y));
            count += 1;
        }
    }
    sum / count as f32
}

fn max_channel(pixels: &[u8]) -> u8 {
    pixels
        .chunks(4)
        .map(|p| p[0].max(p[1]).max(p[2]))
        .max()
        .unwrap_or(0)
}

fn ground_plane() -> Vec<LabVertex> {
    let corners = [
        [-4.0, 0.0, -4.0],
        [4.0, 0.0, -4.0],
        [4.0, 0.0, 4.0],
        [-4.0, 0.0, 4.0],
    ];
    let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    corners
        .into_iter()
        .zip(uvs)
        .map(|(position, uv)| LabVertex::from_pos_normal_uv(position, [0.0, 1.0, 0.0], uv))
        .collect()
}

fn elevated_disc() -> Vec<LabVertex> {
    // Flat disc (quad) above the origin that should cast a shadow onto the ground.
    let corners = [
        [-0.7, 1.2, -0.7],
        [0.7, 1.2, -0.7],
        [0.7, 1.2, 0.7],
        [-0.7, 1.2, 0.7],
    ];
    let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    corners
        .into_iter()
        .zip(uvs)
        .map(|(position, uv)| LabVertex::from_pos_normal_uv(position, [0.0, 1.0, 0.0], uv))
        .collect()
}

static GROUND: std::sync::LazyLock<Vec<LabVertex>> = std::sync::LazyLock::new(ground_plane);
static DISC: std::sync::LazyLock<Vec<LabVertex>> = std::sync::LazyLock::new(elevated_disc);
static MATERIAL: LabMaterial = LabMaterial {
    base_color: [0.75, 0.75, 0.75, 1.0],
    roughness: 0.6,
    ..LabMaterial::EMPTY
};

fn scene(shadows: bool, illuminance: f32) -> FrameScene<'static> {
    FrameScene {
        width: 256,
        height: 256,
        eye: [0.0, 6.0, 9.0],
        target: [0.0, 0.2, 0.0],
        fov_y: 1.0,
        near: 0.1,
        far: 40.0,
        depth_prepass: true,
        directional_shadows: shadows,
        ibl: false,
        exposure_ev: 15.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new([0.35, -0.85, 0.4], [1.0, 1.0, 1.0], illuminance),
        local_lights: &[],
        quality: FrameQuality::LAB,
        meshes: vec![
            FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 1,
                object_key: 1,
                vertices: &GROUND,
                indices: &[0, 2, 1, 0, 3, 2],
                material: &MATERIAL,
                model: regia_core::math::Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: regia_render::ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            },
            FrameMesh {
                is_static: false,
                mesh_key: 2,
                material_key: 1,
                object_key: 2,
                vertices: &DISC,
                indices: &[0, 2, 1, 0, 3, 2],
                material: &MATERIAL,
                model: regia_core::math::Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: regia_render::ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            },
        ],
    }
}

fn render(scene: &FrameScene<'_>) -> regia_render::ForwardResult {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let mut renderer = FrameRenderer::new();
    renderer.render_offscreen(&device, scene).expect("frame render")
}

#[test]
fn lit_ground_clearly_brighter_than_ambient() {
    let lit = render(&scene(true, 100_000.0));
    assert_eq!(lit.pixels.len(), (256 * 256 * 4) as usize);

    let max_c = max_channel(&lit.pixels);
    // Ambient-only scenes top out near channel 15; direct sun on a grey plane
    // must exceed that by a wide margin.
    assert!(
        max_c > 80,
        "expected direct lighting (max channel > 80), got max={max_c} — still ambient-only?"
    );

    // The same scene with the sun switched off must be dramatically darker:
    // physically 100 klx of sun dominates any residual ambient.
    let ambient = render(&scene(false, 0.0));
    let lit_ground = avg_luminance_in_region(&lit.pixels, 256, 40, 160, 70, 185);
    let ambient_ground = avg_luminance_in_region(&ambient.pixels, 256, 40, 160, 70, 185);
    assert!(
        lit_ground > ambient_ground * 5.0,
        "sun-lit ground {lit_ground:.4} must dwarf the sun-off frame {ambient_ground:.4}"
    );
}

/// An elevated disc must darken the ground it shadows: the shadow-on frame has
/// a detectably darker umbra than nearby open ground, and is darker overall
/// than the shadows-off frame.
#[test]
fn elevated_disc_casts_shadow_on_ground() {
    let shaded = render(&scene(true, 100_000.0));
    let unshaded = render(&scene(false, 100_000.0));

    let mean = |pixels: &[u8]| -> f32 {
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p as &[u8; 4])
            .map(luminance)
            .sum::<f32>()
            / pixels.len() as f32
            / 4.0
    };
    let shaded_mean = mean(&shaded.pixels);
    let unshaded_mean = mean(&unshaded.pixels);
    assert!(
        shaded_mean < unshaded_mean * 0.98,
        "the disc's shadow must darken the frame (shaded {shaded_mean:.5} vs unshaded {unshaded_mean:.5})"
    );

    // The umbra: the 16×16 block that changes most between the two frames is
    // the disc's shadow; it must be distinctly darker with shadows on.
    let block = 16u32;
    let mut best = (0.0f32, 0u32, 0u32);
    for y in (0..256 - block).step_by(8) {
        for x in (0..256 - block).step_by(8) {
            let on = avg_luminance_in_region(&shaded.pixels, 256, x, y, x + block, y + block);
            let off = avg_luminance_in_region(&unshaded.pixels, 256, x, y, x + block, y + block);
            if off - on > best.0 {
                best = (off - on, x, y);
            }
        }
    }
    let (_, bx, by) = best;
    let best_delta = best.0;
    assert!(
        best_delta > 0.02,
        "some block must darken noticeably when shadows turn on (best delta {best_delta:.4})"
    );
    let umbra = avg_luminance_in_region(&shaded.pixels, 256, bx, by, bx + block, by + block);
    let same_unshaded = avg_luminance_in_region(&unshaded.pixels, 256, bx, by, bx + block, by + block);
    assert!(
        umbra < same_unshaded * 0.9,
        "umbra block ({bx},{by}) with shadows ({umbra:.4}) must beat the shadows-off frame ({same_unshaded:.4})"
    );
}

#[test]
fn write_lighting_fix_after_png() {
    // Review-only artifact: never write into `docs/`. Gated behind
    // `REGIA_WRITE_EVIDENCE=1` and written to `target/evidence/` instead.
    if regia_core::brand::env_var_os("REGIA_WRITE_EVIDENCE").as_deref() != Some(std::ffi::OsStr::new("1")) {
        eprintln!("skip lighting-fix evidence: set REGIA_WRITE_EVIDENCE=1");
        return;
    }
    let result = render(&scene(true, 100_000.0));
    let out = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/evidence/m3/lighting-fix-after.png");
    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    write_png(&out, result.width, result.height, &result.pixels).expect("write after png");
    assert!(out.is_file(), "missing {}", out.display());
    eprintln!(
        "wrote {} (max channel {})",
        out.display(),
        max_channel(&result.pixels)
    );
}

fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) -> std::io::Result<()> {
    // regia_render has no `image`/`png` dep; encode via Python's stdlib zlib (same as before PNG).
    let tmp = path.with_extension("rgba");
    std::fs::write(&tmp, rgba)?;
    let status = std::process::Command::new("python3")
        .arg("-c")
        .arg(format!(
            "import struct,zlib,pathlib; w,h={width},{height}; rgba=pathlib.Path({tmp:?}).read_bytes();\n\
             def chunk(tag,data): return struct.pack('>I',len(data))+tag+data+struct.pack('>I',zlib.crc32(tag+data)&0xffffffff);\n\
             raw=b''.join(b'\\x00'+rgba[y*w*4:(y+1)*w*4] for y in range(h));\n\
             ihdr=struct.pack('>IIBBBBB',w,h,8,6,0,0,0);\n\
             pathlib.Path({path:?}).write_bytes(b'\\x89PNG\\r\\n\\x1a\\n'+chunk(b'IHDR',ihdr)+chunk(b'IDAT',zlib.compress(raw,9))+chunk(b'IEND',b''))"
        ))
        .status()?;
    let _ = std::fs::remove_file(&tmp);
    if !status.success() {
        return Err(std::io::Error::other("python png encode failed"));
    }
    Ok(())
}

/// Base-colour texture bytes are sRGB (glTF 2.0 § 3.9.2): a texel of byte 60 must
/// shade like an untextured material whose linear base colour is that byte
/// decoded (≈ 0.045), not like byte 60 read as linear (≈ 0.235, 5× too bright).
/// This is the "bleached textures" bug DO27 measured: charcoal rendered tan.
#[test]
fn albedo_texture_bytes_are_decoded_as_srgb() {
    const BYTE: u8 = 60;
    let decoded = {
        let c = f32::from(BYTE) / 255.0;
        ((c + 0.055) / 1.055).powf(2.4)
    };
    static TEXTURED: std::sync::LazyLock<LabMaterial> = std::sync::LazyLock::new(|| LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        roughness: 0.6,
        albedo: Some(regia_render::LabTexture {
            width: 1,
            height: 1,
            rgba: vec![BYTE, BYTE, BYTE, 255],
        }),
        ..LabMaterial::EMPTY
    });
    static FLAT: std::sync::OnceLock<LabMaterial> = std::sync::OnceLock::new();
    let flat = FLAT.get_or_init(|| LabMaterial {
        base_color: [decoded, decoded, decoded, 1.0],
        roughness: 0.6,
        ..LabMaterial::EMPTY
    });
    let ground_only = |material: &'static LabMaterial| {
        let mut s = scene(false, 100_000.0);
        s.meshes.truncate(1);
        s.meshes[0].material = material;
        s
    };
    let textured = render(&ground_only(&TEXTURED));
    let reference = render(&ground_only(flat));
    let t = avg_luminance_in_region(&textured.pixels, 256, 40, 160, 70, 185);
    let r = avg_luminance_in_region(&reference.pixels, 256, 40, 160, 70, 185);
    assert!(r > 0.0005, "reference ground must be lit, got {r:.5}");
    assert!(
        (t / r - 1.0).abs() < 0.08,
        "textured byte {BYTE} shades {t:.5} vs decoded flat {r:.5} (ratio {:.2}); \
         a ratio well above 1 (≈ 2.5 after tonemapping) means the albedo texture is sampled as linear",
        t / r
    );
}
