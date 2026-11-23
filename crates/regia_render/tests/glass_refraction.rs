// SPDX-License-Identifier: MPL-2.0
//! Screen-space refraction behind transmissive glass (`FrameQuality::screen_space_refraction`).
//!
//! - On in Desktop High and Ultra (exterior glass, owner decision 2026-10-08), off in Low,
//!   Mobile, Deck and the lab baseline. Off, or with a thin wall (no volume thickness), a glass
//!   frame is byte-identical to the unrefracted two-blend transmission.
//! - On, a tilted pane with a volume thickness shifts the striped backdrop seen through it by
//!   about the projected normal × thickness × (IOR − 1), the other way when tilted the other
//!   way, and leaves every pixel outside the glass alone.
//! - A bent sample that leaves the screen falls back to the unbent background.

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    ALPHA_PREMULTIPLIED, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh,
    LabScene, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};

const W: u32 = 192;
const H: u32 = 128;
const EYE_HEIGHT: f32 = 4.0;
const FOV_Y: f32 = 0.6;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn slab(half: [f32; 3], model: Mat4, material: usize) -> LabMesh {
    let (vertices, indices) = regia_render::box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material,
        model: model.to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

/// A black floor at y = −1 with bright bars along z every 0.5 m, under a 2.4 m glass pane at
/// the origin tilted by `tilt` radians about z.
fn glass_frame(refraction: bool, thickness: f32, tilt: f32, with_glass: bool) -> Vec<u8> {
    let glass = with_glass.then(|| slab([1.2, 0.005, 1.2], Mat4::from_rotation_z(tilt), 2));
    scene_frame(refraction, thickness, glass, W, H, [0.0, EYE_HEIGHT, 0.01], false)
}

/// The striped floor (bars along z; `grid` adds bars along x) under `glass` (material 2).
fn scene_frame(
    refraction: bool,
    thickness: f32,
    glass_mesh: Option<LabMesh>,
    width: u32,
    height: u32,
    eye: [f32; 3],
    grid: bool,
) -> Vec<u8> {
    let floor = LabMaterial {
        base_color: [0.02, 0.02, 0.02, 1.0],
        roughness: 1.0,
        ..LabMaterial::default()
    };
    let bar = LabMaterial {
        base_color: [0.0, 0.0, 0.0, 1.0],
        emissive: [1.0, 0.9, 0.8],
        emissive_intensity: 3000.0,
        roughness: 1.0,
        ..LabMaterial::default()
    };
    let glass = LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        roughness: 0.05,
        transmission: 1.0,
        ior: 1.5,
        transmission_thickness: thickness,
        alpha_mode: ALPHA_PREMULTIPLIED,
        ..LabMaterial::default()
    };
    let mut meshes = vec![slab(
        [10.0, 0.005, 10.0],
        Mat4::from_translation(Vec3::new(0.0, -1.005, 0.0)),
        0,
    )];
    for i in -10..=10 {
        meshes.push(slab(
            [0.125, 0.005, 10.0],
            Mat4::from_translation(Vec3::new(i as f32 * 0.5, -0.995, 0.0)),
            1,
        ));
        if grid {
            meshes.push(slab(
                [10.0, 0.005, 0.125],
                Mat4::from_translation(Vec3::new(0.0, -0.994, i as f32 * 0.5)),
                1,
            ));
        }
    }
    meshes.extend(glass_mesh);
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width,
        height,
        eye,
        target: [0.0, 0.0, 0.0],
        fov_y: FOV_Y,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.3, -0.8, 0.2], [1.0, 1.0, 1.0], 0.0),
        local_lights: Vec::new(),
        materials: vec![floor, bar, glass],
        meshes,
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.screen_space_refraction = refraction;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer
        .render_offscreen(&device(), &frame)
        .expect("frame")
        .pixels
}

fn luma(px: &[u8], x: u32, y: u32) -> f32 {
    let i = ((y * W + x) * 4) as usize;
    0.2126 * f32::from(px[i]) + 0.7152 * f32::from(px[i + 1]) + 0.0722 * f32::from(px[i + 2])
}

/// The integer x shift that best maps `unbent` onto `bent` over the pane's centre rows.
fn measured_shift(bent: &[u8], unbent: &[u8]) -> i32 {
    let (x0, x1) = (W / 2 - 24, W / 2 + 24);
    let (y0, y1) = (H / 2 - 8, H / 2 + 8);
    let mut best = (f32::MAX, 0i32);
    for s in -20i32..=20 {
        let mut ssd = 0.0;
        for y in y0..y1 {
            for x in x0..x1 {
                let xs = (x as i32 + s) as u32;
                let d = luma(bent, x, y) - luma(unbent, xs, y);
                ssd += d * d;
            }
        }
        if ssd < best.0 {
            best = (ssd, s);
        }
    }
    best.1
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn refraction_is_on_in_high_tiers_only_and_a_thin_wall_bends_nothing() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (preset, on) in [
        (RenderProfilePreset::DesktopHigh, true),
        (RenderProfilePreset::DesktopUltra, true),
        (RenderProfilePreset::DesktopLow, false),
        (RenderProfilePreset::MobileHigh, false),
        (RenderProfilePreset::MobileLow, false),
        (RenderProfilePreset::Deck, false),
    ] {
        let quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
        assert_eq!(quality.screen_space_refraction, on, "{preset:?}");
    }
    const { assert!(!FrameQuality::LAB.screen_space_refraction) };
    let reference = glass_frame(false, 0.0, 0.4, true);
    assert!(
        glass_frame(false, 0.8, 0.4, true) == reference,
        "a volume thickness alone must not change a frame with refraction off"
    );
    assert!(
        glass_frame(true, 0.0, 0.4, true) == reference,
        "a thin wall (thickness 0) must not change the frame with refraction on"
    );
}

#[test]
fn a_thick_tilted_pane_shifts_what_is_behind_it() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let thickness = 0.8f32;
    let ior = 1.5f32;
    let mut shifts = Vec::new();
    for tilt in [0.4f32, -0.4] {
        let unbent = glass_frame(false, thickness, tilt, true);
        let bent = glass_frame(true, thickness, tilt, true);
        // Outside the pane the two frames are the same pixels.
        let mut moved_outside = 0;
        for y in 0..H {
            for x in 0..W {
                let in_pane = (x as i32 - W as i32 / 2).abs() < 72;
                let i = ((y * W + x) * 4) as usize;
                if !in_pane && bent[i..i + 3] != unbent[i..i + 3] {
                    moved_outside += 1;
                }
            }
        }
        assert_eq!(
            moved_outside, 0,
            "tilt {tilt}: refraction changed pixels outside the glass"
        );
        // The bend replaces what shows through the glass, it does not add to it: inside the
        // pane, bars move off some pixels (bright → dark) as well as onto others.
        let (mut darkened, mut brightened) = (0, 0);
        for y in H / 2 - 20..H / 2 + 20 {
            for x in W / 2 - 30..W / 2 + 30 {
                let (a, b) = (luma(&unbent, x, y), luma(&bent, x, y));
                if a > 120.0 && b < 60.0 {
                    darkened += 1;
                }
                if a < 60.0 && b > 120.0 {
                    brightened += 1;
                }
            }
        }
        eprintln!("tilt {tilt}: {darkened} pixels darkened, {brightened} brightened");
        assert!(
            darkened > 100 && brightened > 100,
            "tilt {tilt}: the bend must move the bars, not add a copy ({darkened} darkened, {brightened} brightened)"
        );
        let shift = measured_shift(&bent, &unbent);
        // Expected: the normal's part across the (near-vertical) view, × thickness × (IOR − 1),
        // projected at the pane's distance.
        let across = tilt.sin();
        let world = across * thickness * (ior - 1.0);
        let px_per_m = (H as f32 * 0.5) / (EYE_HEIGHT * (FOV_Y * 0.5).tan());
        let expected = world * px_per_m;
        eprintln!("tilt {tilt}: measured shift {shift} px, expected about {expected:.1} px");
        assert!(
            (shift.unsigned_abs() as f32) > 0.5 * expected.abs()
                && (shift.unsigned_abs() as f32) < 1.6 * expected.abs(),
            "tilt {tilt}: shift {shift} px vs about {expected:.1} px"
        );
        shifts.push(shift);
    }
    assert!(
        shifts[0].signum() == -shifts[1].signum(),
        "opposite tilts must bend the background opposite ways: {shifts:?}"
    );
}

#[test]
fn a_bend_off_screen_falls_back_to_the_unbent_background() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // A huge thickness throws every bent sample off the screen: the frame is the unbent one.
    assert!(
        glass_frame(true, 500.0, 0.4, true) == glass_frame(false, 500.0, 0.4, true),
        "an off-screen bend must fall back to the unbent background"
    );
}

/// A glass ball (0.5 m radius, volume thickness 0.3 m) over a grid, refraction off and on.
/// (The offset is linear in the normal: at thickness × (IOR − 1) = radius every bent sample
/// lands on the ball's centre line, so the evidence keeps the bend below that.)
/// `REGIA_WRITE_REFRACTION_EVIDENCE=1` writes both frames to `docs/evidence/refraction/`.
#[test]
fn a_glass_ball_bends_the_grid_behind_it() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (w, h) = (480u32, 270u32);
    let (verts, indices) = regia_render::uv_sphere(0.5, 48, 24);
    let ball = || LabMesh {
        vertices: verts.clone(),
        indices: indices.clone(),
        material: 2,
        ..LabMesh::unskinned()
    };
    let eye = [0.0, 2.2, 2.6];
    let off = scene_frame(false, 0.3, Some(ball()), w, h, eye, true);
    let on = scene_frame(true, 0.3, Some(ball()), w, h, eye, true);
    let mut changed = 0usize;
    for (a, b) in off.as_chunks::<4>().0.iter().zip(on.as_chunks::<4>().0) {
        if (0..3).any(|c| a[c].abs_diff(b[c]) > 8) {
            changed += 1;
        }
    }
    let share = changed as f32 / (w * h) as f32;
    eprintln!("refraction changed {:.1} % of the frame", share * 100.0);
    // The ball covers roughly 8 % of the frame; the bend moves a good part of what it shows
    // and nothing outside it.
    assert!(share > 0.01 && share < 0.15, "changed share {share}");
    if regia_core::brand::env_var("REGIA_WRITE_REFRACTION_EVIDENCE")
        .ok()
        .as_deref()
        == Some("1")
    {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/evidence/refraction");
        std::fs::create_dir_all(&dir).expect("evidence dir");
        for (name, px) in [("ball_off.png", &off), ("ball_on.png", &on)] {
            let file = std::fs::File::create(dir.join(name)).expect("png create");
            let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("png header");
            writer.write_image_data(px).expect("png data");
        }
    }
}
