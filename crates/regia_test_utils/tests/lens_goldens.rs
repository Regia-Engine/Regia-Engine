// SPDX-License-Identifier: MPL-2.0
//! Camera lens effects (motion blur, depth of field) on lavapipe goldens, plus the checks that make
//! the goldens mean something:
//!
//! - **off is unchanged** (ADR-041): a lens that asks for nothing, and motion blur on a held
//!   camera over a still scene, give the same bytes as no lens at all;
//! - **spinning wheel**: the spokes lose most of their gradient energy under a 180° shutter while the
//!   static wall around the wheel keeps every byte;
//! - **focus pull**: over a receding row of spheres the sharpest sphere (relative to the
//!   all-in-focus frame) follows the focal distance, for both kernels.
//!
//! `LENS_DUMP=<dir>` writes every frame as PNG for judging by eye. `lens_timings_4k` (ignored)
//! prints the lens cost at 3840×2160 from the GPU post stamp; run it on the bench GPU.

use glam::{Mat4, Vec3};
use regia_render::{
    DepthOfField, DepthOfFieldMode, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial,
    LabMesh, LabScene, LabVertex, MotionBlur, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};
use regia_test_utils::golden::{
    DEFAULT_SSIM_THRESHOLD, GoldenGuard, compare_rgba8_to_png_on, golden_path, guard_golden,
};

const W: u32 = 256;
const H: u32 = 160;
const FOV_Y: f32 = 0.8;
/// The spheres frame is larger, behind a long lens: a 20° vertical field of view (about a 68 mm
/// lens on a full-frame sensor). A wide lens at a low resolution has a sub-pixel circle of
/// confusion at any real aperture (46° at 160 px: 0.6 px for a sphere 4 m behind the focus).
const DW: u32 = 480;
const DH: u32 = 300;
const DOF_FOV_Y: f32 = 0.35;
/// The spheres camera.
const DOF_EYE: [f32; 3] = [0.0, 0.0, 6.0];
/// Wheel spokes (alternating bright and dark sectors).
const SECTORS: u32 = 8;
/// Wheel rotation per frame, radians (30°: about 20 px at the rim).
const SPIN: f32 = std::f32::consts::PI / 6.0;

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skipping lens goldens: {err}");
            None
        }
    }
}

fn quad(out: &mut LabMesh, x0: f32, x1: f32, y0: f32, y1: f32, z: f32) {
    let base = out.vertices.len() as u32;
    let n = [0.0, 0.0, 1.0];
    out.vertices.extend([
        LabVertex::from_pos_normal_uv([x0, y0, z], n, [0.0, 1.0]),
        LabVertex::from_pos_normal_uv([x1, y0, z], n, [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([x1, y1, z], n, [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([x0, y1, z], n, [0.0, 0.0]),
    ]);
    out.indices
        .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
}

fn mesh(material: usize) -> LabMesh {
    LabMesh {
        material,
        ..LabMesh::unskinned()
    }
}

fn matte(rgb: [f32; 3]) -> LabMaterial {
    LabMaterial {
        base_color: [rgb[0], rgb[1], rgb[2], 1.0],
        roughness: 0.7,
        ..LabMaterial::default()
    }
}

/// A checker wall at `z` covering `half` metres each way, in materials 0 and 1.
fn checker_wall(dark: &mut LabMesh, light: &mut LabMesh, half: f32, cell: f32, z: f32) {
    let n = (2.0 * half / cell).round() as i32;
    for i in 0..n {
        for j in 0..n {
            let x0 = -half + i as f32 * cell;
            let y0 = -half + j as f32 * cell;
            let target = if (i + j) % 2 == 0 { &mut *dark } else { &mut *light };
            quad(target, x0, x0 + cell, y0, y0 + cell, z);
        }
    }
}

fn base_scene(materials: Vec<LabMaterial>, meshes: Vec<LabMesh>, eye: [f32; 3], far: f32) -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye,
        target: [0.0, 0.0, 0.0],
        fov_y: FOV_Y,
        near: 0.1,
        far,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.35, -1.0], [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

/// Meshes 0–1: the checker wall 3 m behind the wheel; 2–3: the wheel's bright and dark sectors
/// (spun by their model matrix about +Z); 4: the hub.
fn wheel_scene() -> LabScene {
    let mut wall_dark = mesh(0);
    let mut wall_light = mesh(1);
    checker_wall(&mut wall_dark, &mut wall_light, 8.0, 0.5, -3.0);
    let mut bright = mesh(2);
    let mut dark = mesh(3);
    let radius = 1.3;
    let n = [0.0, 0.0, 1.0];
    for s in 0..SECTORS {
        let a0 = s as f32 / SECTORS as f32 * std::f32::consts::TAU;
        let a1 = (s + 1) as f32 / SECTORS as f32 * std::f32::consts::TAU;
        let target = if s.is_multiple_of(2) {
            &mut bright
        } else {
            &mut dark
        };
        let base = target.vertices.len() as u32;
        // A fan of thin slices per sector, so the rim is round.
        let slices = 6;
        target
            .vertices
            .push(LabVertex::from_pos_normal_uv([0.0, 0.0, 0.0], n, [0.5, 0.5]));
        for k in 0..=slices {
            let a = a0 + (a1 - a0) * k as f32 / slices as f32;
            target.vertices.push(LabVertex::from_pos_normal_uv(
                [radius * a.cos(), radius * a.sin(), 0.0],
                n,
                [0.5, 0.5],
            ));
        }
        for k in 0..slices {
            target.indices.extend([base, base + 1 + k, base + 2 + k]);
        }
    }
    let mut hub = mesh(4);
    quad(&mut hub, -0.18, 0.18, -0.18, 0.18, 0.02);
    base_scene(
        vec![
            matte([0.05, 0.05, 0.06]),
            matte([0.35, 0.35, 0.33]),
            matte([0.85, 0.75, 0.2]),
            matte([0.03, 0.03, 0.03]),
            matte([0.6, 0.6, 0.6]),
        ],
        vec![wall_dark, wall_light, bright, dark, hub],
        [0.0, 0.0, 6.0],
        60.0,
    )
}

/// Sphere `i` (0–4): centre, radius and distance from the eye. They recede from 3 m to 11 m,
/// fanned across the frame, each about the same size on screen.
fn sphere_layout(i: usize) -> ([f32; 3], f32, f32) {
    let depth = 3.0 + 2.0 * i as f32;
    let angle = (i as f32 - 2.0) * 0.105;
    let lift = if i.is_multiple_of(2) { -0.02 } else { 0.02 } * depth;
    let center = [depth * angle.tan(), lift, DOF_EYE[2] - depth];
    (center, 0.034 * depth, depth)
}

/// Five spheres receding from 3 m to 11 m from the eye, a checker wall 20 m away, and small bright
/// emissive beads on the wall for the bokeh shapes. `checker: false` paints the wall one flat grey
/// (the sharpness measure then sees only the sphere silhouettes).
fn spheres_scene(checker: bool) -> LabScene {
    let wall_z = DOF_EYE[2] - 20.0;
    let mut wall_dark = mesh(0);
    let mut wall_light = mesh(1);
    checker_wall(&mut wall_dark, &mut wall_light, 8.0, 0.8, wall_z);
    let mut meshes = vec![wall_dark, wall_light];
    for i in 0..5 {
        let (center, radius, _) = sphere_layout(i);
        let (v, idx) = regia_render::uv_sphere(radius, 32, 20);
        meshes.push(LabMesh {
            vertices: regia_render::translate_mesh(&v, center),
            indices: idx,
            material: 2 + i,
            ..LabMesh::unskinned()
        });
    }
    let mut beads = mesh(7);
    for (bx, by) in [
        (-4.0, 2.4),
        (-1.5, 3.0),
        (1.6, 2.7),
        (4.2, 1.8),
        (3.4, -2.6),
        (-3.2, -2.2),
        (0.4, -3.0),
    ] {
        quad(
            &mut beads,
            bx - 0.06,
            bx + 0.06,
            by - 0.06,
            by + 0.06,
            wall_z + 0.05,
        );
    }
    meshes.push(beads);
    let sphere = |rgb| LabMaterial {
        roughness: 0.35,
        ..matte(rgb)
    };
    let mut scene = base_scene(
        vec![
            matte(if checker {
                [0.06, 0.06, 0.07]
            } else {
                [0.3, 0.3, 0.3]
            }),
            matte([0.3, 0.3, 0.3]),
            sphere([0.8, 0.15, 0.1]),
            sphere([0.85, 0.6, 0.1]),
            sphere([0.2, 0.7, 0.2]),
            sphere([0.15, 0.35, 0.85]),
            sphere([0.6, 0.2, 0.75]),
            LabMaterial {
                emissive: [1.0, 0.9, 0.7],
                emissive_intensity: 400_000.0,
                ..matte([0.0, 0.0, 0.0])
            },
        ],
        meshes,
        DOF_EYE,
        80.0,
    );
    scene.width = DW;
    scene.height = DH;
    scene.fov_y = DOF_FOV_Y;
    scene.target = [0.0, 0.0, -10.0];
    scene
}

/// Desktop High without the passes that are not under test; TAA at the preset's render scale
/// (a temporally upscaled frame: depth and velocity are smaller than the post colour).
fn taa_quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.screen_space_gi = false;
    q.ssr = false;
    q.ssao = regia_render::SsaoMode::Off;
    q.gi = regia_render::GiMode::Ambient;
    q.reflection_probes = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.physical_atmosphere = false;
    q.taau = true;
    q
}

/// Native, no temporal resolve (depth of field needs no velocity).
fn native_quality() -> FrameQuality {
    let mut q = taa_quality();
    q.taau = false;
    q.render_scale_permille = 1000;
    q
}

fn post(motion_blur: Option<MotionBlur>, depth_of_field: Option<DepthOfField>) -> PostSettings {
    PostSettings {
        auto_exposure: false,
        exposure_ev: 14.5,
        bloom: false,
        taa: false,
        motion_blur,
        depth_of_field,
        ..PostSettings::golden_deterministic()
    }
}

fn dump(tag: &str, pixels: &[u8], (w, h): (u32, u32)) {
    let Some(dir) = regia_core::brand::env_var_os("LENS_DUMP").map(std::path::PathBuf::from) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    if let Some(img) = image::RgbaImage::from_raw(w, h, pixels.to_vec()) {
        let _ = img.save(dir.join(format!("{tag}.png")));
    }
}

/// Renders `frames` frames of the wheel through one renderer, spinning `spin` radians per frame;
/// returns the last.
fn render_wheel(device: &RenderDevice, settings: &PostSettings, spin: f32, frames: u32) -> Vec<u8> {
    let scene = wheel_scene();
    let mut frame = scene.as_frame_scene_with_quality(taa_quality());
    frame.post = Some(settings);
    let mut renderer = FrameRenderer::new();
    let mut last = Vec::new();
    for i in 0..frames {
        let model = Mat4::from_rotation_z(spin * i as f32).to_cols_array_2d();
        frame.meshes[2].model = model;
        frame.meshes[3].model = model;
        last = renderer
            .render_offscreen(device, &frame)
            .expect("wheel frame")
            .pixels;
    }
    last
}

fn render_spheres(device: &RenderDevice, settings: &PostSettings, checker: bool) -> Vec<u8> {
    let scene = spheres_scene(checker);
    let mut frame = scene.as_frame_scene_with_quality(native_quality());
    frame.post = Some(settings);
    let mut renderer = FrameRenderer::new();
    renderer
        .render_offscreen(device, &frame)
        .expect("spheres frame")
        .pixels
}

fn luma(px: &[u8], w: u32, x: u32, y: u32) -> f32 {
    let i = ((y * w + x) * 4) as usize;
    0.2126 * f32::from(px[i]) + 0.7152 * f32::from(px[i + 1]) + 0.0722 * f32::from(px[i + 2])
}

/// Sum of (∂L/∂x)² + (∂L/∂y)² over the pixels of a `w × h` image for which `keep(x, y)` holds.
/// Squared, because a blur keeps the total variation of a step edge but spreads its gradient:
/// a step blurred over `n` pixels keeps about `1 / n` of this energy.
fn edge_energy(px: &[u8], (w, h): (u32, u32), keep: impl Fn(u32, u32) -> bool) -> f32 {
    let mut sum = 0.0;
    for y in 0..h - 1 {
        for x in 0..w - 1 {
            if keep(x, y) {
                let l = luma(px, w, x, y);
                sum += (luma(px, w, x + 1, y) - l).powi(2) + (luma(px, w, x, y + 1) - l).powi(2);
            }
        }
    }
    sum
}

/// Pixel centre and radius of sphere `i` in the spheres frame.
fn sphere_disc(i: usize) -> (f32, f32, f32) {
    let (center, radius, depth) = sphere_layout(i);
    let view =
        glam::camera::rh::view::look_at_mat4(Vec3::from_array(DOF_EYE), Vec3::new(0.0, 0.0, -10.0), Vec3::Y);
    let proj = glam::camera::rh::proj::directx::perspective(DOF_FOV_Y, DW as f32 / DH as f32, 0.1, 80.0);
    let clip = proj * view * Vec3::from_array(center).extend(1.0);
    let ndc = clip.truncate() / clip.w;
    let cx = (ndc.x * 0.5 + 0.5) * DW as f32;
    let cy = (0.5 - ndc.y * 0.5) * DH as f32;
    let r_px = radius / (depth * (DOF_FOV_Y * 0.5).tan()) * DH as f32 * 0.5;
    (cx, cy, r_px)
}

/// Gradient energy of sphere `i`'s silhouette: a band 2.5 px either side of its rim. In focus the
/// rim is a step; out of focus it spreads over the circle of confusion. (The wall behind is out of
/// focus in every frame, so a wider region would measure the wall.)
fn sphere_energy(px: &[u8], i: usize) -> f32 {
    let (cx, cy, r) = sphere_disc(i);
    edge_energy(px, (DW, DH), |x, y| {
        let d = (x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy);
        (d - r).abs() <= 2.5
    })
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn lens_off_leaves_frames_unchanged() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let off = post(None, None);
    // A shutter of zero and a lens with no depth of field ask for nothing.
    let inert = post(
        Some(MotionBlur {
            shutter_angle: 0.0,
            samples: 4,
        }),
        None,
    );
    let a = render_wheel(&device, &off, SPIN, 3);
    let b = render_wheel(&device, &inert, SPIN, 3);
    assert!(a == b, "an inert lens changed the frame");
    // Motion blur on a still scene under a held camera: every pixel moved less than a pixel.
    let blur = post(Some(MotionBlur::from_shutter_degrees(180.0, 4)), None);
    let still_off = render_wheel(&device, &off, 0.0, 3);
    let still_on = render_wheel(&device, &blur, 0.0, 3);
    let changed = still_off.iter().zip(&still_on).filter(|(x, y)| x != y).count();
    assert_eq!(
        changed, 0,
        "motion blur changed {changed} channels of a still frame"
    );
}

#[test]
fn spinning_wheel_blurs_the_spokes_not_the_wall() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let off = render_wheel(&device, &post(None, None), SPIN, 4);
    let blur = post(Some(MotionBlur::from_shutter_degrees(180.0, 4)), None);
    let on = render_wheel(&device, &blur, SPIN, 4);
    dump("wheel_off", &off, (W, H));
    dump("wheel_motion_blur", &on, (W, H));

    // The wheel's rim is about 41 px from the centre (1.3 m at 6 m; 160 px span 2·6·tan 0.4 m).
    // The spoke edges are radial and blur; the rim and the wall do not move along themselves, so
    // the measure is the annulus between the hub and the rim.
    let (cx, cy) = (W as f32 * 0.5, H as f32 * 0.5);
    let spokes = |x: u32, y: u32| {
        let r = (x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy);
        (12.0..34.0).contains(&r)
    };
    let e_off = edge_energy(&off, (W, H), spokes);
    let e_on = edge_energy(&on, (W, H), spokes);
    eprintln!(
        "wheel edge energy: off {e_off:.0}, motion blur {e_on:.0} ({:.2})",
        e_on / e_off
    );
    assert!(
        e_on < 0.7 * e_off,
        "the spokes kept {:.0} % of their edges",
        100.0 * e_on / e_off
    );

    // The wall away from the wheel does not move: its bytes are the unblurred frame's.
    for (x0, x1) in [(0, 60), (W - 60, W)] {
        for y in 0..H {
            for x in x0..x1 {
                let i = ((y * W + x) * 4) as usize;
                assert_eq!(
                    on[i..i + 4],
                    off[i..i + 4],
                    "static wall pixel ({x}, {y}) changed"
                );
            }
        }
    }

    guard_golden(
        &on,
        W,
        H,
        "lens_wheel_motion_blur",
        GoldenGuard {
            min_lum: 20.0,
            max_lum: 200.0,
            max_white_pct: 5.0,
            max_black_pct: 60.0,
            min_object_pct: 50.0,
        },
    );
    compare_rgba8_to_png_on(
        &device.adapter.get_info(),
        &on,
        W,
        H,
        &golden_path("lens_wheel_motion_blur"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("wheel motion blur golden");
}

fn dof(mode: DepthOfFieldMode, focal_distance: f32) -> DepthOfField {
    DepthOfField {
        mode,
        focal_distance,
        aperture_f_stops: 1.4,
        sensor_height: 0.024,
        max_circle_of_confusion_diameter: 32.0,
        ..DepthOfField::default()
    }
}

#[test]
fn focus_pull_moves_the_sharp_sphere() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let sharp = render_spheres(&device, &post(None, None), false);
    dump("spheres_off", &sharp, (DW, DH));
    let reference: Vec<f32> = (0..5).map(|i| sphere_energy(&sharp, i)).collect();
    for mode in [DepthOfFieldMode::Gaussian, DepthOfFieldMode::Bokeh] {
        // Focus on the first, middle and last sphere (3, 7 and 11 m).
        for (focus_ix, focus_m) in [(0usize, 3.0f32), (2, 7.0), (4, 11.0)] {
            let px = render_spheres(&device, &post(None, Some(dof(mode, focus_m))), false);
            dump(
                &format!("spheres_{mode:?}_{focus_m}m").to_lowercase(),
                &px,
                (DW, DH),
            );
            let kept: Vec<f32> = (0..5)
                .map(|i| sphere_energy(&px, i) / reference[i].max(1.0))
                .collect();
            eprintln!("{mode:?} focus {focus_m} m: edge energy kept per sphere {kept:.2?}");
            assert!(
                kept[focus_ix] > 0.85,
                "{mode:?}: the sphere in focus at {focus_m} m kept only {:.2} of its edges",
                kept[focus_ix]
            );
            for (i, k) in kept.iter().enumerate() {
                if i.abs_diff(focus_ix) >= 2 {
                    assert!(
                        *k < 0.75 * kept[focus_ix],
                        "{mode:?} focus {focus_m} m: sphere {i} kept {k:.2} of its edges, not blurred"
                    );
                }
            }
        }
    }

    let guard = GoldenGuard {
        min_lum: 15.0,
        max_lum: 200.0,
        max_white_pct: 5.0,
        max_black_pct: 60.0,
        min_object_pct: 50.0,
    };
    for (name, mode) in [
        ("lens_spheres_bokeh", DepthOfFieldMode::Bokeh),
        ("lens_spheres_gaussian", DepthOfFieldMode::Gaussian),
    ] {
        let px = render_spheres(&device, &post(None, Some(dof(mode, 3.0))), true);
        dump(name, &px, (DW, DH));
        guard_golden(&px, DW, DH, name, guard);
        compare_rgba8_to_png_on(
            &device.adapter.get_info(),
            &px,
            DW,
            DH,
            &golden_path(name),
            DEFAULT_SSIM_THRESHOLD,
        )
        .expect(name);
    }
}

/// Lens cost at 3840×2160: the GPU post interval (lens + post stack) with each effect minus
/// without. Run on the bench GPU: `cargo test --release -p regia_test_utils --test lens_goldens
/// -- --ignored --nocapture lens_timings_4k`. `LENS_BUDGET_MS` (default 0.6) is asserted per effect.
#[test]
#[ignore = "timing run for the bench GPU"]
fn lens_timings_4k() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let budget: f32 = regia_core::brand::env_var("LENS_BUDGET_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.6);
    let mut scene = wheel_scene();
    scene.width = 3840;
    scene.height = 2160;
    let cases = [
        ("off", post(None, None)),
        (
            "motion_blur_180_4",
            post(Some(MotionBlur::from_shutter_degrees(180.0, 4)), None),
        ),
        (
            "dof_gaussian",
            post(None, Some(dof(DepthOfFieldMode::Gaussian, 3.0))),
        ),
        ("dof_bokeh", post(None, Some(dof(DepthOfFieldMode::Bokeh, 3.0)))),
    ];
    let mut medians = Vec::new();
    for (name, settings) in &cases {
        let mut frame = scene.as_frame_scene_with_quality(taa_quality());
        frame.post = Some(settings);
        let mut renderer = FrameRenderer::new();
        renderer.set_gpu_timing(&device, true);
        let mut samples = Vec::new();
        for i in 0..60u32 {
            let model = Mat4::from_rotation_z(SPIN * i as f32).to_cols_array_2d();
            frame.meshes[2].model = model;
            frame.meshes[3].model = model;
            renderer.render_offscreen(&device, &frame).expect("4k frame");
            if i >= 20 {
                samples.push(renderer.gpu_timings().post_ms);
            }
        }
        samples.sort_by(f32::total_cmp);
        let median = samples[samples.len() / 2];
        println!("lens_4k,{name},post_ms_median,{median:.3}");
        medians.push(median);
    }
    let off = medians[0];
    for ((name, _), median) in cases.iter().zip(&medians).skip(1) {
        let cost = median - off;
        println!("lens_4k,{name},cost_ms,{cost:.3}");
        assert!(
            cost <= budget,
            "{name} costs {cost:.3} ms at 4K (budget {budget} ms)"
        );
    }
}

// ---- Post-effect correctness goldens (ADR-1152) ----
//
// TODO(lead): these goldens are not blessed yet (the lane could not run a GPU). Bless each on the
// the build server lavapipe reference, judge the `LENS_DUMP` PNGs by eye, commit the PNGs, then remove the
// `#[ignore]`:
// `REGIA_BLESS=1 cargo test -p regia_test_utils --test lens_goldens -- --ignored post_effect_golden`

/// Bright beads on a dark wall at an odd frame size (241 × 161: every bloom level is odd), with a
/// strong bloom: each glow is centred on its bead, the same at every bead.
#[test]
#[ignore = "golden not blessed yet (ADR-1152): bless on the lavapipe reference, then remove the ignore"]
fn post_effect_golden_bloom_beads_odd_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let mut wall = mesh(0);
    quad(&mut wall, -8.0, 8.0, -8.0, 8.0, -3.0);
    let mut beads = mesh(1);
    for (i, bx) in [-1.6f32, -0.79, 0.03, 0.81, 1.62].iter().enumerate() {
        // Each bead a different fraction of a pixel off the grid.
        let by = -0.6 + 0.3 * i as f32 + 0.007 * i as f32;
        quad(&mut beads, bx - 0.02, bx + 0.02, by - 0.02, by + 0.02, -2.95);
    }
    let mut scene = base_scene(
        vec![
            matte([0.04, 0.04, 0.05]),
            LabMaterial {
                emissive: [1.0, 0.85, 0.6],
                emissive_intensity: 400_000.0,
                ..matte([0.0, 0.0, 0.0])
            },
        ],
        vec![wall, beads],
        [0.0, 0.0, 6.0],
        60.0,
    );
    scene.width = 241;
    scene.height = 161;
    let settings = PostSettings {
        bloom: true,
        bloom_mips: 5,
        bloom_intensity: 0.3,
        ..post(None, None)
    };
    let mut frame = scene.as_frame_scene_with_quality(native_quality());
    frame.post = Some(&settings);
    let mut renderer = FrameRenderer::new();
    let px = renderer
        .render_offscreen(&device, &frame)
        .expect("bloom beads frame")
        .pixels;
    dump("post_bloom_beads_odd", &px, (241, 161));
    guard_golden(
        &px,
        241,
        161,
        "post_bloom_beads_odd",
        GoldenGuard {
            min_lum: 5.0,
            max_lum: 120.0,
            max_white_pct: 5.0,
            max_black_pct: 98.0,
            min_object_pct: 1.0,
        },
    );
    compare_rgba8_to_png_on(
        &device.adapter.get_info(),
        &px,
        241,
        161,
        &golden_path("post_bloom_beads_odd"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("bloom beads golden");
}

/// The spheres frame focused on the far wall with a 64 px largest circle: the near spheres' bokeh
/// spreads its full radius over the sharp wall (the tile max reaches two tiles).
#[test]
#[ignore = "golden not blessed yet (ADR-1152): bless on the lavapipe reference, then remove the ignore"]
fn post_effect_golden_large_bokeh() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let lens = DepthOfField {
        max_circle_of_confusion_diameter: 64.0,
        ..dof(DepthOfFieldMode::Bokeh, 20.0)
    };
    let px = render_spheres(&device, &post(None, Some(lens)), true);
    dump("lens_bokeh_large_circle", &px, (DW, DH));
    guard_golden(
        &px,
        DW,
        DH,
        "lens_bokeh_large_circle",
        GoldenGuard {
            min_lum: 15.0,
            max_lum: 200.0,
            max_white_pct: 5.0,
            max_black_pct: 60.0,
            min_object_pct: 50.0,
        },
    );
    compare_rgba8_to_png_on(
        &device.adapter.get_info(),
        &px,
        DW,
        DH,
        &golden_path("lens_bokeh_large_circle"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("large bokeh golden");
}

/// The wheel spun a quarter turn a frame (about 65 px at the rim, 32 px of blur under a 180°
/// shutter) with one sample asked for: the pass adds samples, so the spokes blur into a smooth
/// disc instead of ghost copies, and the static wall keeps every byte.
#[test]
#[ignore = "golden not blessed yet (ADR-1152): bless on the lavapipe reference, then remove the ignore"]
fn post_effect_golden_fast_wheel() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let spin = std::f32::consts::FRAC_PI_2;
    let off = render_wheel(&device, &post(None, None), spin, 4);
    let blur = post(Some(MotionBlur::from_shutter_degrees(180.0, 1)), None);
    let on = render_wheel(&device, &blur, spin, 4);
    dump("lens_wheel_fast", &on, (W, H));
    for (x0, x1) in [(0, 60), (W - 60, W)] {
        for y in 0..H {
            for x in x0..x1 {
                let i = ((y * W + x) * 4) as usize;
                assert_eq!(
                    on[i..i + 4],
                    off[i..i + 4],
                    "static wall pixel ({x}, {y}) changed"
                );
            }
        }
    }
    guard_golden(
        &on,
        W,
        H,
        "lens_wheel_fast",
        GoldenGuard {
            min_lum: 20.0,
            max_lum: 200.0,
            max_white_pct: 5.0,
            max_black_pct: 60.0,
            min_object_pct: 50.0,
        },
    );
    compare_rgba8_to_png_on(
        &device.adapter.get_info(),
        &on,
        W,
        H,
        &golden_path("lens_wheel_fast"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("fast wheel golden");
}
