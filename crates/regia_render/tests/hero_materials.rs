// SPDX-License-Identifier: MPL-2.0
//! Hero-asset material channels (lane hero-lookdev): DO27's buildings and vehicles are glTF
//! metal-roughness materials with a packed ORM image, transmissive glass and blended parts.
//!
//! - A packed ORM's G and B scale the roughness and metallic factors exactly as the glTF
//!   metallic-roughness texture does (glTF 2.0 § 3.9.2); the renderer used R only.
//! - Glass (`KHR_materials_transmission`, `KHR_materials_ior`) shows the frame behind it
//!   head-on and turns mirror-like at grazing angles (Fresnel).
//! - A blended material on the HDR frame blends with its own alpha (the HDR target's alpha
//!   carried the roughness for SSR, so a blended draw used its roughness as coverage).
//! - Metal under a sky is never black: the specular IBL lights it.

use regia_render::{
    ALPHA_BLEND, ALPHA_PREMULTIPLIED, AtmosphereParameters, AtmospherePreset, DirectionalLightUniform,
    FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabTexture, OutdoorParams, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset,
};

const W: u32 = 96;
const H: u32 = 64;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn srgb_to_linear(c: u8) -> f64 {
    let c = f64::from(c) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn rect_luma(px: &[u8], rect: [f32; 4]) -> f64 {
    let (x0, x1) = ((rect[0] * W as f32) as u32, (rect[2] * W as f32) as u32);
    let (y0, y1) = ((rect[1] * H as f32) as u32, (rect[3] * H as f32) as u32);
    let (mut s, mut n) = (0.0, 0.0);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            s += 0.2126 * srgb_to_linear(px[i])
                + 0.7152 * srgb_to_linear(px[i + 1])
                + 0.0722 * srgb_to_linear(px[i + 2]);
            n += 1.0;
        }
    }
    s / n
}

/// A thin `size`-metre slab whose top face sits at `y`, facing +Y.
fn quad(size: f32, y: f32) -> LabMesh {
    let (verts, idx) = regia_render::box_mesh([size * 0.5, 0.005, size * 0.5]);
    LabMesh {
        vertices: regia_render::translate_mesh(&verts, [0.0, y - 0.005, 0.0]),
        indices: idx,
        material: 0,
        ..LabMesh::unskinned()
    }
}

fn solid(rgba: [u8; 4]) -> LabTexture {
    LabTexture {
        width: 2,
        height: 2,
        rgba: rgba.repeat(4),
    }
}

/// One HDR (Desktop High, post on) frame of `meshes` seen from `eye` toward the origin.
fn frame(
    eye: [f32; 3],
    materials: Vec<LabMaterial>,
    meshes: Vec<LabMesh>,
    sun_lux: f32,
    sky: bool,
) -> Vec<u8> {
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye,
        target: [0.0, 0.0, 0.0],
        fov_y: 0.6,
        near: 0.05,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: sky,
        exposure_ev: 12.0,
        ambient_lux: if sky { 12_000.0 } else { 0.0 },
        double_sided: true,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.3, -0.8, 0.2], [1.0, 1.0, 1.0], sun_lux),
        local_lights: Vec::new(),
        materials,
        meshes,
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    if sky {
        renderer.set_outdoor(OutdoorParams {
            planet: AtmosphereParameters::from_preset(AtmospherePreset::Earth),
            ..OutdoorParams::default()
        });
    }
    let device = device();
    renderer.render_offscreen(&device, &frame).expect("frame").pixels
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_packed_orm_scales_roughness_and_metallic_like_the_gltf_texture() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // ORM (R occlusion, G roughness, B metallic) times the factors must render exactly like
    // the same products given as scalar factors.
    for (g, b, rough_f, metal_f) in [
        (64u8, 255u8, 1.0f32, 1.0f32),
        (255, 0, 0.5, 1.0),
        (128, 128, 1.0, 13.0),
    ] {
        let textured = LabMaterial {
            base_color: [0.7, 0.5, 0.3, 1.0],
            roughness: rough_f,
            metallic: metal_f,
            occlusion_map: Some(solid([255, g, b, 255])),
            orm_packed: true,
            ..LabMaterial::default()
        };
        let scalar = LabMaterial {
            base_color: [0.7, 0.5, 0.3, 1.0],
            roughness: (rough_f * f32::from(g) / 255.0).clamp(0.0, 1.0),
            metallic: (metal_f * f32::from(b) / 255.0).clamp(0.0, 1.0),
            occlusion_map: Some(solid([255, 255, 255, 255])),
            ..LabMaterial::default()
        };
        let a = frame(
            [0.0, 3.0, 2.0],
            vec![textured],
            vec![quad(4.0, 0.0)],
            30_000.0,
            true,
        );
        let s = frame(
            [0.0, 3.0, 2.0],
            vec![scalar],
            vec![quad(4.0, 0.0)],
            30_000.0,
            true,
        );
        let worst = a.iter().zip(&s).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        assert!(
            worst <= 3,
            "ORM g {g} b {b}: textured vs scalar differ by {worst}"
        );
    }
    // Without the flag the packed G/B are ignored (R stays occlusion).
    let plain = LabMaterial {
        base_color: [0.7, 0.5, 0.3, 1.0],
        roughness: 1.0,
        metallic: 1.0,
        occlusion_map: Some(solid([255, 64, 0, 255])),
        ..LabMaterial::default()
    };
    let flagged = LabMaterial {
        orm_packed: true,
        ..plain.clone()
    };
    let a = frame([0.0, 3.0, 2.0], vec![plain], vec![quad(4.0, 0.0)], 30_000.0, true);
    let b = frame(
        [0.0, 3.0, 2.0],
        vec![flagged],
        vec![quad(4.0, 0.0)],
        30_000.0,
        true,
    );
    assert!(a != b, "the ORM flag must change a metal-1 material whose B is 0");
}

/// A white unlit-bright backdrop at y = −1 with a glass pane at y = 0 over half of it.
fn glass_over_backdrop(eye: [f32; 3], with_glass: bool) -> Vec<u8> {
    let backdrop = LabMaterial {
        base_color: [0.0, 0.0, 0.0, 1.0],
        emissive: [1.0, 1.0, 1.0],
        emissive_intensity: 4000.0,
        roughness: 1.0,
        ..LabMaterial::default()
    };
    let glass = LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        roughness: 0.05,
        transmission: 1.0,
        ior: 1.5,
        alpha_mode: ALPHA_PREMULTIPLIED,
        ..LabMaterial::default()
    };
    let mut meshes = vec![quad(40.0, -1.0)];
    if with_glass {
        meshes.push(LabMesh {
            material: 1,
            ..quad(40.0, 0.0)
        });
    }
    // No sky: the pane reflects black, so what it shows is only what it transmits.
    frame(eye, vec![backdrop, glass], meshes, 0.0, false)
}

#[test]
fn glass_is_clear_head_on_and_reflective_at_grazing() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let centre = [0.35, 0.35, 0.65, 0.65];
    // Head-on: the backdrop shows through at ~96 % (one surface, F0 0.04).
    let bare = rect_luma(&glass_over_backdrop([0.0, 3.0, 0.01], false), centre);
    let glazed = rect_luma(&glass_over_backdrop([0.0, 3.0, 0.01], true), centre);
    eprintln!("head-on: backdrop {bare:.4}, through glass {glazed:.4}");
    assert!(
        glazed > 0.85 * bare,
        "glass must show what is behind it head-on: {glazed:.4} vs {bare:.4}"
    );
    // Grazing (~8° above the pane): mostly reflection, the backdrop strongly attenuated.
    let bare_g = rect_luma(&glass_over_backdrop([0.0, 0.42, 3.0], false), centre);
    let glazed_g = rect_luma(&glass_over_backdrop([0.0, 0.42, 3.0], true), centre);
    eprintln!("grazing: backdrop {bare_g:.4}, through glass {glazed_g:.4}");
    let head_on_loss = 1.0 - glazed / bare;
    let grazing_loss = 1.0 - glazed_g / bare_g;
    assert!(
        grazing_loss > head_on_loss + 0.15,
        "Fresnel: grazing must pass less light than head-on ({grazing_loss:.3} vs {head_on_loss:.3})"
    );
}

#[test]
fn a_blended_material_uses_its_alpha_on_the_hdr_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // A quarter-opaque black film over the bright backdrop: ~75 % of it shows through.
    // The HDR target's alpha carried the roughness (0.9 here), so the film covered 90 %.
    let backdrop = LabMaterial {
        base_color: [0.0, 0.0, 0.0, 1.0],
        emissive: [1.0, 1.0, 1.0],
        emissive_intensity: 4000.0,
        ..LabMaterial::default()
    };
    let film = LabMaterial {
        base_color: [0.0, 0.0, 0.0, 0.25],
        roughness: 0.9,
        alpha_mode: ALPHA_BLEND,
        ..LabMaterial::default()
    };
    let centre = [0.35, 0.35, 0.65, 0.65];
    let bare = rect_luma(
        &frame(
            [0.0, 3.0, 0.01],
            vec![backdrop.clone(), film.clone()],
            vec![quad(40.0, -1.0)],
            0.0,
            false,
        ),
        centre,
    );
    let filmed = rect_luma(
        &frame(
            [0.0, 3.0, 0.01],
            vec![backdrop, film],
            vec![
                quad(40.0, -1.0),
                LabMesh {
                    material: 1,
                    ..quad(40.0, 0.0)
                },
            ],
            0.0,
            false,
        ),
        centre,
    );
    eprintln!("backdrop {bare:.4}, under a 25 % film {filmed:.4}");
    let ratio = filmed / bare;
    assert!(
        (0.6..0.9).contains(&ratio),
        "a 25 % film passed {ratio:.3} of the backdrop"
    );
}

#[test]
fn metal_under_a_sky_is_never_black() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Sky light only (no sun): a 0.9 metal must read at least half as bright as a 0.9
    // diffuse surface, at any roughness; without the specular IBL it is black. Multi-scatter
    // (ADR-043) restores the energy single-scatter GGX drops on rough metal; the hero-asset
    // mapping turns it on.
    let centre = [0.3, 0.3, 0.7, 0.7];
    let diffuse = LabMaterial {
        base_color: [0.9, 0.9, 0.9, 1.0],
        roughness: 0.8,
        ..LabMaterial::default()
    };
    let d = rect_luma(
        &frame([0.0, 3.0, 1.5], vec![diffuse], vec![quad(6.0, 0.0)], 0.0, true),
        centre,
    );
    for rough in [0.15f32, 0.5, 0.9] {
        let metal = LabMaterial {
            base_color: [0.9, 0.9, 0.9, 1.0],
            metallic: 1.0,
            roughness: rough,
            hard_surface: regia_render::HardSurfaceFactors {
                multi_scatter: 1.0,
                ..regia_render::HardSurfaceFactors::OFF
            },
            ..LabMaterial::default()
        };
        let m = rect_luma(
            &frame([0.0, 3.0, 1.5], vec![metal], vec![quad(6.0, 0.0)], 0.0, true),
            centre,
        );
        eprintln!("roughness {rough}: metal {m:.4} vs diffuse {d:.4}");
        assert!(
            m > 0.5 * d,
            "metal at roughness {rough} reads {m:.4} against diffuse {d:.4}"
        );
    }
}

#[test]
fn volume_absorption_tints_what_is_seen_through_glass() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // White glass with a red-passing volume over the white backdrop: the backdrop seen through it
    // keeps red and loses green and blue (Beer–Lambert), head-on.
    let backdrop = LabMaterial {
        base_color: [0.0, 0.0, 0.0, 1.0],
        emissive: [1.0, 1.0, 1.0],
        emissive_intensity: 4000.0,
        roughness: 1.0,
        ..LabMaterial::default()
    };
    let glass = LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        roughness: 0.05,
        transmission: 1.0,
        transmission_tint: regia_render::volume_transmittance(0.02, [0.9, 0.3, 0.3], 0.01),
        alpha_mode: ALPHA_PREMULTIPLIED,
        ..LabMaterial::default()
    };
    let px = frame(
        [0.0, 3.0, 0.01],
        vec![backdrop, glass],
        vec![
            quad(40.0, -1.0),
            LabMesh {
                material: 1,
                ..quad(40.0, 0.0)
            },
        ],
        0.0,
        false,
    );
    let i = (((H / 2) * W + W / 2) * 4) as usize;
    let (r, g, b) = (px[i], px[i + 1], px[i + 2]);
    eprintln!("through tinted glass: {r} {g} {b}");
    assert!(
        r > g + 40 && r > b + 40,
        "the volume must tint the view red: {r} {g} {b}"
    );
}

#[test]
fn a_v3_glass_material_reaches_the_renderer() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let text = br#"(
        format_version: 3,
        shader: Standard,
        base: (base_color: (1.0, 1.0, 1.0, 1.0), roughness: 0.15, emissive: (0.2, 1.0, 0.3), emissive_strength: 12.0, ior: 1.52),
        transmission: (factor: 0.92, thickness: 0.012, attenuation_color: (0.75, 0.88, 0.84), attenuation_distance: 0.08),
        render_state: (alpha_mode: opaque, queue_offset: 0, alpha_cutoff: 0.3),
    )"#;
    let asset = regia_asset::MaterialAssetRon::from_ron(text).expect("v3");
    let m = LabMaterial::from_authored(&asset);
    assert!((m.transmission - 0.92).abs() < 1e-6);
    assert!((m.ior - 1.52).abs() < 1e-6);
    assert!((m.emissive_intensity - 12.0).abs() < 1e-6);
    assert!((m.alpha_cutoff - 0.3).abs() < 1e-6);
    assert_eq!(m.alpha_mode, ALPHA_PREMULTIPLIED);
    let want = regia_render::volume_transmittance(0.012, [0.75, 0.88, 0.84], 0.08);
    assert_eq!(m.transmission_tint, want);
    assert!(want[0] < 1.0 && want[1] > want[0], "{want:?}");
}
