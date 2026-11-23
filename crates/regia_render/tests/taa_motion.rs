// SPDX-License-Identifier: MPL-2.0
//! Temporal AA in motion (spec 27 § 2.3) on a chart scene: a wall of line pairs (1, 2
//! and 4 px, pixel-aligned at the scored poses so the native frame is not aliased) and
//! 1 px horizontal bars, a bright box sliding over it.
//!
//! Every look judgement before the motion-look lane was made on still frames; these
//! tests score the resolve under a held camera, a sub-pixel pan, and a moving object
//! against the native (un-jittered, no temporal resolve) frame of the same pose:
//!
//! - **held detail**: gradient energy of the converged still over native ≥ 0.85. The
//!   previous frame's matrix was stored jittered, so a held camera reprojected its
//!   history by the last jitter (up to ½ px) every frame: a bilinear random walk that
//!   left ~30 % of the native detail on DO27's ground.
//! - **pan detail** (2.5 px/frame, ending pixel-aligned): MTF50 of the moving chart over
//!   the native still's (spec 27 § 2.3 motion clarity), the 4 px pairs' modulation and
//!   the 1 px bars (a bilinear history fetch at a fractional offset blurred every frame).
//! - **ghosting**: 8 frames after the box left a region, the residual difference from
//!   the native frame is ≤ 2 % of the box/background contrast.
//! - **stability**: a held camera's frame-to-frame mean |ΔL| after convergence.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};

const W: u32 = 320;
const H: u32 = 180;
/// Wall distance from the eye, metres.
const DIST: f32 = 8.0;
const FOV_Y: f32 = 0.9;

/// Metres per pixel on the wall.
fn px_m() -> f32 {
    2.0 * DIST * (FOV_Y * 0.5).tan() / H as f32
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

/// Band layout in output pixels at the start pose: `(x0, x1, stripe width px)`.
const BANDS: [(u32, u32, f32); 3] = [(40, 120, 1.0), (120, 200, 2.0), (200, 280, 4.0)];
/// Rows of the 1 px horizontal bars and of the line-pair bands.
const BAR_ROWS: (u32, u32) = (40, 70);
const STRIPE_ROWS: (u32, u32) = (80, 150);

fn chart() -> LabScene {
    let m = px_m();
    // Screen pixel (px, py) at the start pose -> wall metres (camera at x = 0, y = 1.5).
    let wx = |px: f32| (px - W as f32 * 0.5) * m;
    let wy = |py: f32| 1.5 - (py - H as f32 * 0.5) * m;
    let mut dark = LabMesh {
        material: 0,
        ..LabMesh::unskinned()
    };
    let mut bright = LabMesh {
        material: 1,
        ..LabMesh::unskinned()
    };
    // The dark wall behind everything (wide enough for the pan).
    quad(&mut dark, -40.0, 40.0, -2.0, 6.0, -0.01);
    for (x0, x1, width) in BANDS {
        let mut x = x0 as f32;
        while x + width <= x1 as f32 {
            quad(
                &mut bright,
                wx(x),
                wx(x + width),
                wy(STRIPE_ROWS.1 as f32),
                wy(STRIPE_ROWS.0 as f32),
                0.0,
            );
            x += 2.0 * width;
        }
    }
    // 1 px bright bars, 3 px apart.
    let mut y = BAR_ROWS.0 as f32;
    while y + 1.0 <= BAR_ROWS.1 as f32 {
        quad(&mut bright, wx(40.0), wx(280.0), wy(y + 1.0), wy(y), 0.0);
        y += 4.0;
    }
    // The mover: a bright box in front of the wall, moved by its model matrix.
    let (bv, bi) = regia_render::box_mesh([0.4, 0.4, 0.2]);
    let mover = LabMesh {
        vertices: bv,
        indices: bi,
        material: 1,
        model: far_away(),
        ..LabMesh::unskinned()
    };
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 1.5, DIST],
        target: [0.0, 1.5, 0.0],
        fov_y: FOV_Y,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.04, 0.04, 0.04, 1.0],
                roughness: 1.0,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.8, 0.8, 0.8, 1.0],
                roughness: 1.0,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![dark, bright, mover],
    }
}

fn far_away() -> [[f32; 4]; 4] {
    translation([0.0, -50.0, 0.0])
}

fn translation(t: [f32; 3]) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [t[0], t[1], t[2], 1.0],
    ]
}

fn quality(taa: bool) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.screen_space_gi = false;
    q.ssr = false;
    q.ssao = regia_render::SsaoMode::Off;
    q.gi = regia_render::GiMode::Ambient;
    q.reflection_probes = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.physical_atmosphere = false;
    q.taau = taa;
    q
}

fn post() -> PostSettings {
    PostSettings {
        auto_exposure: false,
        exposure_ev: 14.5,
        bloom: false,
        taa: false,
        ..PostSettings::golden_deterministic()
    }
}

/// One camera/mover pose.
#[derive(Clone, Copy)]
struct Pose {
    /// Camera shift along +X, metres.
    pan_m: f32,
    /// Mover centre, or `None` (parked out of view).
    mover: Option<[f32; 3]>,
}

/// Renders `poses` in order through one renderer and returns every frame's luma.
/// `TAA_MOTION_DUMP=<dir>` also writes each frame as `<tag>_<taa>_<i>.ppm`.
fn run_tagged(device: &RenderDevice, taa: bool, poses: &[Pose], tag: &str) -> Vec<Vec<f32>> {
    let dump = regia_core::brand::env_var_os("TAA_MOTION_DUMP").map(std::path::PathBuf::from);
    let scene = chart();
    let post = post();
    let mut frame = scene.as_frame_scene_with_quality(quality(taa));
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    let mut out = Vec::new();
    for pose in poses {
        frame.eye = [pose.pan_m, 1.5, DIST];
        frame.target = [pose.pan_m, 1.5, 0.0];
        frame.meshes[2].model = pose.mover.map_or_else(far_away, translation);
        let image = renderer.render_offscreen(device, &frame).expect("chart frame");
        if let Some(dir) = dump.as_ref() {
            let mut ppm = format!("P6\n{W} {H}\n255\n").into_bytes();
            for p in image.pixels.as_chunks::<4>().0 {
                ppm.extend_from_slice(&p[..3]);
            }
            let _ = std::fs::create_dir_all(dir);
            let _ = std::fs::write(
                dir.join(format!("{tag}_{}_{:02}.ppm", u8::from(taa), out.len())),
                ppm,
            );
        }
        out.push(luma(&image.pixels));
    }
    out
}

fn native(device: &RenderDevice, pose: Pose) -> Vec<f32> {
    run_tagged(
        device,
        false,
        &[pose],
        &format!(
            "native{:.0}",
            pose.pan_m * 100.0 + pose.mover.map_or(0.0, |m| m[0] * 1000.0)
        ),
    )
    .remove(0)
}

fn luma(pixels: &[u8]) -> Vec<f32> {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
        .collect()
}

/// Mean squared forward difference (x and y) over `[x0, x1) × [y0, y1)`.
fn gradient_energy(l: &[f32], x0: u32, x1: u32, y0: u32, y1: u32) -> f32 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = (y * W + x) as usize;
            let dx = l[i + 1] - l[i];
            let dy = l[i + W as usize] - l[i];
            sum += dx * dx + dy * dy;
            n += 1.0;
        }
    }
    sum / n
}

fn mean_abs_diff(a: &[f32], b: &[f32], mask: impl Fn(u32, u32) -> bool) -> f32 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in 0..H {
        for x in 0..W {
            if mask(x, y) {
                let i = (y * W + x) as usize;
                sum += (a[i] - b[i]).abs();
                n += 1.0;
            }
        }
    }
    sum / f32::max(n, 1.0)
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    }
}

/// Band ratios (TAA over native) of gradient energy; pan `shift_px` moves the bands left.
fn band_ratios(taa: &[f32], native: &[f32], shift_px: f32) -> Vec<f32> {
    let mut out = Vec::new();
    for (x0, x1, _) in BANDS {
        // Stay 4 px inside the band so edges between bands do not count.
        let a = (x0 as f32 - shift_px + 4.0).max(1.0) as u32;
        let b = (x1 as f32 - shift_px - 4.0).min(W as f32 - 2.0) as u32;
        out.push(
            gradient_energy(taa, a, b, STRIPE_ROWS.0 + 4, STRIPE_ROWS.1 - 4)
                / gradient_energy(native, a, b, STRIPE_ROWS.0 + 4, STRIPE_ROWS.1 - 4).max(1e-3),
        );
    }
    let a = (40.0 - shift_px + 4.0).max(1.0) as u32;
    let b = (280.0 - shift_px - 4.0).min(W as f32 - 2.0) as u32;
    out.push(
        gradient_energy(taa, a, b, BAR_ROWS.0 + 2, BAR_ROWS.1 - 2)
            / gradient_energy(native, a, b, BAR_ROWS.0 + 2, BAR_ROWS.1 - 2).max(1e-3),
    );
    out
}

/// Modulation transfer per band: the amplitude of the line pairs' fundamental
/// (period 2 × stripe width) in `taa` over that in `native`, averaged over the band rows.
fn band_mtf(taa: &[f32], native: &[f32], shift_px: f32) -> Vec<f32> {
    let amplitude = |l: &[f32], x0: u32, x1: u32, width: f32| {
        let omega = std::f32::consts::PI / width;
        let mut total = 0.0;
        for y in STRIPE_ROWS.0 + 4..STRIPE_ROWS.1 - 4 {
            let (mut re, mut im) = (0.0, 0.0);
            for x in x0..x1 {
                let v = l[(y * W + x) as usize];
                re += v * (omega * x as f32).cos();
                im += v * (omega * x as f32).sin();
            }
            total += (re * re + im * im).sqrt() * 2.0 / (x1 - x0) as f32;
        }
        total
    };
    BANDS
        .iter()
        .map(|&(x0, x1, width)| {
            // A whole number of periods inside the band.
            let period = (2.0 * width) as u32;
            let a = (x0 as f32 - shift_px + 4.0).max(1.0) as u32;
            let b0 = (x1 as f32 - shift_px - 4.0).min(W as f32 - 2.0) as u32;
            let b = a + (b0 - a) / period * period;
            amplitude(taa, a, b, width) / amplitude(native, a, b, width).max(1e-3)
        })
        .collect()
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn held_camera_keeps_native_detail_and_is_stable() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let pose = Pose {
        pan_m: 0.0,
        mover: None,
    };
    let frames = run_tagged(&device, true, &[pose; 32], "held");
    let reference = native(&device, pose);
    let last = &frames[31];
    let ratios = band_ratios(last, &reference, 0.0);
    // Frame-to-frame change over the last 8 frames (static scene, converged history).
    let flicker: f32 = (24..32)
        .map(|i| mean_abs_diff(&frames[i], &frames[i - 1], |_, _| true))
        .sum::<f32>()
        / 8.0;
    eprintln!("held: band ratios (1 / 2 / 4 px, 1 px bars) {ratios:?}, flicker {flicker:.3}");
    for (name, ratio) in ["1 px", "2 px", "4 px", "1 px bar"].iter().zip(&ratios) {
        assert!(
            *ratio >= 0.85,
            "held TAA keeps {ratio:.2} of native detail on the {name} lines (≥ 0.85): the history is blurred at rest"
        );
    }
    assert!(
        flicker <= 1.0,
        "held TAA changes {flicker:.2} levels per frame after convergence (≤ 1)"
    );
}

#[test]
fn panning_keeps_native_detail() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    // 2.5 px per frame to the right: every other frame sits half a pixel off the grid
    // (the worst case for a bilinear fetch); frame 24 lands pixel-aligned at 60 px.
    let step_px = 2.5;
    let poses: Vec<Pose> = (0..25)
        .map(|i| Pose {
            pan_m: i as f32 * step_px * px_m(),
            mover: None,
        })
        .collect();
    let frames = run_tagged(&device, true, &poses, "pan");
    let last_pose = poses[24];
    let reference = native(&device, last_pose);
    let ratios = band_ratios(&frames[24], &reference, 24.0 * step_px);
    let mtf = band_mtf(&frames[24], &reference, 24.0 * step_px);
    let f50 = mtf50(&mtf);
    eprintln!(
        "pan 2.5 px/frame: gradient-energy ratios (1 / 2 / 4 px, 1 px bars) {ratios:?}, \
         MTF (1 / 2 / 4 px) {mtf:?}, MTF50 {f50:.3} c/px = {:.2} of native",
        f50 / 0.5
    );
    // Spec 27 § 2.3 motion clarity: MTF50 while panning over the native still's (Nyquist:
    // the native frame is pixel-aligned, its MTF is 1 on every band). Target ★ 0.85.
    // 0.39 before the motion-look lane, 0.74 after round 1 (Catmull–Rom history), 0.90
    // after round 2: a Keys a = −1 history cubic (0.77) and the motion-adaptive sharpen
    // after the resolve (`taau_sharpen.wgsl`).
    assert!(
        f50 / 0.5 >= 0.87,
        "panning TAA's MTF50 is {:.2} of native (≥ 0.87; spec target ★ 0.85)",
        f50 / 0.5
    );
    // The sharpen restores, it does not exaggerate: no band above native, and the line
    // pairs' edge energy at most 10 % over native (halos).
    for (name, m) in ["1 px", "2 px", "4 px"].iter().zip(&mtf) {
        assert!(
            *m <= 1.02,
            "panning TAA's {name} modulation is {m:.2}× native: oversharpened"
        );
    }
    for (name, r) in ["1 px", "2 px", "4 px", "1 px bar"].iter().zip(&ratios) {
        assert!(
            *r <= 1.1,
            "panning TAA's {name} edge energy is {r:.2}× native: halos"
        );
    }
    assert!(
        mtf[2] >= 0.85,
        "panning TAA keeps {:.2} of the 4 px line pairs' modulation (≥ 0.85)",
        mtf[2]
    );
    // Thin geometry: 1 px bars across the motion stay resolved (spec 27 § 2.3: ≥ 90 %).
    assert!(
        ratios[3] >= 0.9,
        "panning TAA keeps {:.2} of the 1 px bars (≥ 0.9)",
        ratios[3]
    );
}

/// The spatial frequency (cycles/px) where the band MTFs (1, 2, 4 px stripes = 0.5,
/// 0.25, 0.125 c/px) cross 0.5, linear between bands; 0.5 when even the 1 px band holds.
fn mtf50(mtf: &[f32]) -> f32 {
    let points = [(0.0, 1.0), (0.125, mtf[2]), (0.25, mtf[1]), (0.5, mtf[0])];
    for pair in points.windows(2) {
        let ((f0, m0), (f1, m1)) = (pair[0], pair[1]);
        if m1 < 0.5 {
            return f0 + (f1 - f0) * ((m0 - 0.5) / (m0 - m1).max(1e-6)).clamp(0.0, 1.0);
        }
    }
    0.5
}

#[test]
fn a_moving_object_leaves_no_ghost() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let m = px_m();
    // The box (0.8 m ≈ 18 px wide) slides right 3 px per frame in front of the 4 px band.
    let start_x = (200.0 - W as f32 * 0.5) * m;
    let poses: Vec<Pose> = (0..24)
        .map(|i| Pose {
            pan_m: 0.0,
            mover: Some([
                start_x + i as f32 * 3.0 * m,
                1.5 - (115.0 - H as f32 * 0.5) * m,
                0.5,
            ]),
        })
        .collect();
    let frames = run_tagged(&device, true, &poses, "ghost");
    let last = poses[23];
    let reference = native(&device, last);
    let empty = native(
        &device,
        Pose {
            pan_m: 0.0,
            mover: None,
        },
    );
    // Contrast: the box over the background, from the native frames.
    let covered_now = |x: u32, y: u32| {
        let i = (y * W + x) as usize;
        (reference[i] - empty[i]).abs() > 20.0
    };
    let box_l = {
        let mut sum = 0.0;
        let mut n = 0.0;
        for y in 0..H {
            for x in 0..W {
                if covered_now(x, y) {
                    sum += reference[(y * W + x) as usize];
                    n += 1.0;
                }
            }
        }
        sum / f32::max(n, 1.0)
    };
    // The dark wall below the bands.
    let background_l = mean_abs_diff(&empty, &vec![0.0; empty.len()], |x, y| {
        (40..280).contains(&x) && (158..172).contains(&y)
    });
    let contrast = (box_l - background_l).abs().max(1.0);
    // The trail: where the box was 8 to 16 frames ago and is not now.
    let box_px = 0.8 / m;
    let now_left = 200.0 + 23.0 * 3.0 - box_px * 0.5;
    let trail_x = (now_left - 16.0 * 3.0 - box_px * 0.5, now_left - 8.0 * 3.0 - 2.0);
    let trail = |x: u32, y: u32| {
        (x as f32) >= trail_x.0 && (x as f32) < trail_x.1 && (108..122).contains(&y) && !covered_now(x, y)
    };
    let ghost = mean_abs_diff(&frames[23], &reference, trail) / contrast;
    eprintln!("ghost: trail x {trail_x:?}, contrast {contrast:.1}, residual {ghost:.4}");
    assert!(
        ghost <= 0.02,
        "the moving box leaves {:.1} % of its contrast behind after 8 frames (spec 27 § 2.3: ≤ 2 %)",
        ghost * 100.0
    );
}
