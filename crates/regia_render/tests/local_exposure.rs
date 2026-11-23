// SPDX-License-Identifier: MPL-2.0
//! Local exposure (lane v020-terrain-local-lights, gap B; DO27's map look asks for its
//! shadows lifted to 0.75 contrast): a sunlit grey floor under a black sky with a wall's deep
//! shadow across half of it and small blocks in both halves. With `shadow_contrast` 0.75 the shadowed half prints
//! brighter, the sunlit half stays, the blocks keep their local contrast, and a setting within
//! a hair of neutral leaves the frame as it was (no halo, banding or grid seams).

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LocalExposure,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, box_mesh, translate_mesh,
};

const W: u32 = 192;
const H: u32 = 108;

fn render(device: &RenderDevice, local: Option<LocalExposure>) -> Vec<u8> {
    let post = PostSettings {
        local_exposure: local,
        exposure_ev: 14.0,
        ..PostSettings::golden_deterministic()
    };
    let mat = |c: f32| LabMaterial {
        base_color: [c, c, c, 1.0],
        roughness: 0.8,
        ..LabMaterial::default()
    };
    // The floor's texture: 2 m checks of 0.5 and 0.33 linear (the local detail).
    let checker = regia_render::LabTexture {
        width: 16,
        height: 16,
        rgba: (0..16 * 16)
            .flat_map(|i| {
                let v = if ((i % 16) / 2 + (i / 16) / 2) % 2 == 0 {
                    255
                } else {
                    210
                };
                [v, v, v, 255]
            })
            .collect(),
    };
    let floor = LabMaterial {
        albedo: Some(checker),
        ..mat(0.5)
    };
    let mut meshes = Vec::new();
    let mut add = |half: [f32; 3], at: [f32; 3], material: usize| {
        let (v, i) = box_mesh(half);
        meshes.push(LabMesh {
            vertices: translate_mesh(&v, at),
            indices: i,
            material,
            ..LabMesh::unskinned()
        });
    };
    // Floor, the wall that shadows its -X half, and blocks in both halves.
    add([8.0, 0.1, 8.0], [0.0, -0.1, 0.0], 0);
    add([0.2, 4.0, 8.0], [-0.2, 4.0, 0.0], 0);
    for (i, x) in [-5.5f32, -3.5, -1.8, 1.8, 3.5, 5.5].into_iter().enumerate() {
        add([0.35, 0.35, 0.35], [x, 0.35, (i as f32 - 2.5) * 0.9], 1);
    }
    let lab = LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        // Airless-like: no sky light, a dim flat fill, so the wall's shadow sits ~5 EV under
        // the sunlit floor (DO27's Europa plates).
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 1_500.0,
        double_sided: false,
        post: Some(post),
        // From +X, low: the wall at x = 0 shadows the floor at x < 0.
        directional: DirectionalLightUniform::new([-0.8, -0.6, 0.0], [1.0, 0.97, 0.92], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![floor, mat(0.25)],
        meshes,
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopLow));
    quality.taau = false;
    let mut renderer = FrameRenderer::new();
    let px = renderer
        .render_offscreen(device, &lab.as_frame_scene_with_quality(quality))
        .expect("frame")
        .pixels;
    if let Some(dir) = regia_core::brand::env_var_os("LOCAL_EXPOSURE_DUMP") {
        let name = match local {
            None => "off".to_owned(),
            Some(l) => format!(
                "s{}_h{}_d{}",
                l.shadow_contrast, l.highlight_contrast, l.detail_strength
            ),
        };
        let path = std::path::Path::new(&dir).join(format!("local_exposure_{name}.png"));
        if let Ok(file) = std::fs::File::create(&path) {
            let mut encoder = png::Encoder::new(file, W, H);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            if let Ok(mut writer) = encoder.write_header() {
                let _ = writer.write_image_data(&px);
            }
        }
    }
    px
}

fn srgb_to_linear(c: u8) -> f64 {
    let c = f64::from(c) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Display-linear luma per pixel.
fn lumas(px: &[u8]) -> Vec<f64> {
    px.as_chunks::<4>()
        .0
        .iter()
        .map(|p| {
            0.2126 * srgb_to_linear(p[0]) + 0.7152 * srgb_to_linear(p[1]) + 0.0722 * srgb_to_linear(p[2])
        })
        .collect()
}

const EYE: [f32; 3] = [0.0, 9.0, 11.0];

/// The pixels of a 17 × 17 patch around the floor point `(x, 0, z)`.
fn floor_patch(x: f32, z: f32) -> Vec<usize> {
    use regia_core::glam::camera::rh::{proj::directx::perspective, view::look_at_mat4};
    use regia_core::glam::{Vec3, Vec4};
    let view = look_at_mat4(Vec3::from(EYE), Vec3::ZERO, Vec3::Y);
    let proj = perspective(0.9, W as f32 / H as f32, 0.1, 200.0);
    let clip = proj * view * Vec4::new(x, 0.0, z, 1.0);
    let (px, py) = (
        ((clip.x / clip.w * 0.5 + 0.5) * W as f32) as u32,
        ((0.5 - clip.y / clip.w * 0.5) * H as f32) as u32,
    );
    let mut set = Vec::new();
    for y in py - 8..=py + 8 {
        for x in px - 8..=px + 8 {
            set.push((y * W + x) as usize);
        }
    }
    set
}

/// The wall's shadow on the floor (with a block in it) and the sunlit floor.
fn dark_and_bright() -> (Vec<usize>, Vec<usize>) {
    (floor_patch(-3.5, 2.5), floor_patch(3.0, 3.5))
}

fn mean(l: &[f64], set: &[usize]) -> f64 {
    set.iter().map(|&i| l[i]).sum::<f64>() / set.len().max(1) as f64
}

/// Mean |Δ log luma| to the right-hand neighbour over `set` (local contrast).
fn local_contrast(l: &[f64], set: &[usize]) -> f64 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for &i in set {
        if (i as u32 % W) + 1 < W && l[i + 1] > 1e-3 {
            sum += (l[i + 1].ln() - l[i].ln()).abs();
            n += 1.0;
        }
    }
    sum / f64::max(n, 1.0)
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn local_exposure_lifts_shadows_and_keeps_highlights_and_detail() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe");
    let off = lumas(&render(&device, None));
    let (dark, bright) = dark_and_bright();
    let lifted = lumas(&render(
        &device,
        Some(LocalExposure {
            shadow_contrast: 0.75,
            ..LocalExposure::default()
        }),
    ));
    let dark_gain = mean(&lifted, &dark) / mean(&off, &dark);
    let bright_gain = mean(&lifted, &bright) / mean(&off, &bright);
    let detail = local_contrast(&lifted, &dark) / local_contrast(&off, &dark);
    eprintln!(
        "shadow 0.75: dark {:.4} -> {:.4} (x{dark_gain:.3}), bright {:.4} -> {:.4} (x{bright_gain:.3}), \
         dark local contrast x{detail:.3}",
        mean(&off, &dark),
        mean(&lifted, &dark),
        mean(&off, &bright),
        mean(&lifted, &bright)
    );
    assert!(
        dark_gain > 1.25,
        "the shadowed half is not lifted: x{dark_gain:.3}"
    );
    assert!(
        (0.95..=1.06).contains(&bright_gain),
        "the sunlit half moved: x{bright_gain:.3}"
    );
    assert!(
        (0.8..=1.25).contains(&detail),
        "the shadowed half lost or gained local contrast: x{detail:.3}"
    );
}

#[test]
fn a_near_neutral_local_exposure_leaves_the_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe");
    let off = render(&device, None);
    let near = render(
        &device,
        Some(LocalExposure {
            shadow_contrast: 0.999,
            highlight_contrast: 0.999,
            detail_strength: 1.0,
        }),
    );
    let worst = off
        .iter()
        .zip(&near)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    eprintln!("near-neutral local exposure: worst byte difference {worst}");
    assert!(
        worst <= 3,
        "a near-neutral local exposure changed the frame by {worst} levels"
    );
    // Exactly neutral settings skip the pass: bit-identical.
    let neutral = render(&device, Some(LocalExposure::default()));
    assert!(off == neutral, "neutral local exposure changed the frame");
}

#[test]
fn highlight_contrast_holds_the_sunlit_half_down() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe");
    let off = lumas(&render(&device, None));
    let (dark, bright) = dark_and_bright();
    let held = lumas(&render(
        &device,
        Some(LocalExposure {
            highlight_contrast: 0.6,
            ..LocalExposure::default()
        }),
    ));
    let bright_gain = mean(&held, &bright) / mean(&off, &bright);
    let dark_gain = mean(&held, &dark) / mean(&off, &dark);
    eprintln!("highlight 0.6: bright x{bright_gain:.3}, dark x{dark_gain:.3}");
    assert!(
        bright_gain < 0.95,
        "the sunlit half is not held down: x{bright_gain:.3}"
    );
    assert!(
        (0.95..=1.05).contains(&dark_gain),
        "the shadowed half moved: x{dark_gain:.3}"
    );
}
