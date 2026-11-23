// SPDX-License-Identifier: MPL-2.0
//! Re-scores the spec 27 § 2.2 rows the contact calibration touches (ADR-3202) with its two
//! opt-in switches off (as shipped) and on, against the reference path tracer: `ao_contact`, the
//! sun penumbra at 1, 5 and 20 m, the contact gap at 1440p, and the GI rows whose frames carry
//! the SSAO (the trench and the color room: ambient, screen-space GI and probes with SSGI, scored
//! as `gi_validation` scores them). The scenes and metrics are `spec27_calibration.rs`'s (ADR-2660)
//! and `gi_validation.rs`'s, rebuilt here because those files score Desktop High as shipped.
//!
//! Hand-run, like the suite (`#[ignore]`d):
//! `cargo test -p regia_pt --test ao_penumbra_rescore -- --ignored --nocapture --test-threads=1`.
//! The horizon AO reaches the frame only with the frame hookup applied
//! (`<scratch>`, one line in
//! `frame/screen_occlusion.rs`); without it the `ao_contact` test fails with that message.

use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use regia_core::glam::Vec3;
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtImage, PtSettings, PtSky};
use regia_render::compare::display_encode;
use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, FrameScene,
    GiMode, LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset,
};
use regia_test_utils::gi_error::{
    GiErrorInput, add_planes, luminance, measure, shadow_mask, sub_planes, surface_mask,
};
use regia_test_utils::gi_scenes::{GI_HEIGHT, GI_WIDTH, GiScene, color_room, trench};
use regia_test_utils::spec27::{self, edge_width, quantile};

const SUN_RADIUS: f32 = 0.004_675;
const SUN_ELEVATION_DEG: f32 = 45.0;
const SUN_LUX: f32 = 100_000.0;
const SUN_EV: f32 = 14.4;

static SERIAL_GPU: Mutex<()> = Mutex::new(());

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("the rescore needs a GPU; lavapipe is one"))
}

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| PathTracer::new(device()).expect("compile the path tracer"))
}

fn gpu() -> MutexGuard<'static, ()> {
    SERIAL_GPU.lock().unwrap_or_else(PoisonError::into_inner)
}

// --- Plates ---------------------------------------------------------------------------------

/// With `$REGIA_SPEC27_OUT` set, writes `plates/<name>.png` (the lead's before/after plates:
/// shipped and calibrated at the same frame number).
fn plate_rgba(name: &str, width: u32, height: u32, rgba: &[u8]) {
    let Some(dir) = spec27::out_dir() else {
        return;
    };
    let dir = dir.join("plates");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("rescore plates: {e}");
        return;
    }
    if let Err(e) = regia_pt::output::write_png(&dir.join(format!("{name}.png")), width, height, rgba) {
        eprintln!("rescore plate {name}: {e}");
    }
}

fn plate_grey(name: &str, width: u32, height: u32, values: &[f32]) {
    let rgba: Vec<u8> = values
        .iter()
        .flat_map(|v| {
            let b = (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            [b, b, b, 255]
        })
        .collect();
    plate_rgba(name, width, height, &rgba);
}

fn plate_linear(name: &str, width: u32, height: u32, linear: &[[f32; 3]], ev: f32) {
    let rgba: Vec<u8> = display_encode(linear, ev)
        .iter()
        .flat_map(|c| {
            let e = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            [e(c[0]), e(c[1]), e(c[2]), 255]
        })
        .collect();
    plate_rgba(name, width, height, &rgba);
}

// --- Scenes (spec27_calibration.rs) ----------------------------------------------------------

fn quad(c: Vec3, u: Vec3, v: Vec3, material: usize) -> LabMesh {
    let n = u.cross(v).normalize_or(Vec3::Y).to_array();
    let corners = [c - u - v, c + u - v, c + u + v, c - u + v];
    let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    LabMesh {
        vertices: corners
            .iter()
            .zip(uvs)
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(p.to_array(), n, uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

fn cuboid(lo: Vec3, hi: Vec3, material: usize) -> Vec<LabMesh> {
    let c = 0.5 * (lo + hi);
    let h = 0.5 * (hi - lo);
    let (x, y, z) = (Vec3::X * h.x, Vec3::Y * h.y, Vec3::Z * h.z);
    vec![
        quad(c + x, -z, y, material),
        quad(c - x, z, y, material),
        quad(c + y, x, -z, material),
        quad(c - y, x, z, material),
        quad(c + z, x, y, material),
        quad(c - z, -x, y, material),
    ]
}

fn ground(h: f32, material: usize) -> LabMesh {
    quad(Vec3::ZERO, Vec3::X * h, -Vec3::Z * h, material)
}

fn grey(albedo: f32) -> LabMaterial {
    LabMaterial {
        base_color: [albedo, albedo, albedo, 1.0],
        roughness: 1.0,
        metallic: 0.0,
        ..LabMaterial::default()
    }
}

/// The two switches of ADR-3202.
#[derive(Clone, Copy, Debug)]
struct Switches {
    horizon_ao: bool,
    angular_sun_penumbra: bool,
}

const SHIPPED: Switches = Switches {
    horizon_ao: false,
    angular_sun_penumbra: false,
};
const CALIBRATED: Switches = Switches {
    horizon_ao: true,
    angular_sun_penumbra: true,
};

/// The suite's Desktop High (`spec27_calibration.rs` `desktop_high`) with the switches.
fn desktop_high(s: Switches) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.msaa_samples = 1;
    q.render_scale_permille = 1000;
    q.horizontal_plus = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.physical_atmosphere = false;
    q.ssr = false;
    q.reflection_probes = false;
    q.gi = GiMode::Ambient;
    q.screen_space_gi = false;
    q.gi_probes = false;
    q.baked_gi = false;
    q.horizon_ao = s.horizon_ao;
    q.angular_sun_penumbra = s.angular_sun_penumbra;
    q
}

fn lab(
    (width, height): (u32, u32),
    (eye, target, fov_y): ([f32; 3], [f32; 3], f32),
    ev: f32,
    directional: DirectionalLightUniform,
    materials: Vec<LabMaterial>,
    meshes: Vec<LabMesh>,
) -> LabScene {
    LabScene {
        width,
        height,
        eye,
        target,
        fov_y,
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: ev,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: ev,
            use_physical_camera: false,
            exposure_compensation: 0.0,
            ..PostSettings::golden_deterministic()
        }),
        directional,
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

fn capture(renderer: &mut FrameRenderer, frame: &FrameScene<'_>, frames: u32) -> Vec<[f32; 3]> {
    for i in 0..frames {
        if i + 1 == frames {
            renderer.request_hdr_capture();
        }
        renderer
            .render_offscreen(device(), frame)
            .expect("real-time frame");
    }
    renderer
        .take_hdr_capture(device())
        .expect("read back")
        .expect("the post stack ran, so the HDR copy exists")
        .rgb
}

fn raster(lab: &LabScene, quality: FrameQuality, frames: u32) -> Vec<[f32; 3]> {
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    capture(&mut renderer, &lab.as_frame_scene_with_quality(quality), frames)
}

fn trace(frame: &FrameScene<'_>, options: &ExtractOptions, spp: u32, bounces: u32) -> PtImage {
    let pt = from_frame(frame, options);
    tracer()
        .render(
            &pt,
            &PtSettings {
                spp,
                max_bounces: bounces,
                seed: 27,
                jitter: false,
                samples_per_submit: 8,
                ..PtSettings::default()
            },
        )
        .expect("trace the reference")
}

fn sun_options() -> ExtractOptions {
    ExtractOptions {
        sky: Some(PtSky::None),
        sun_angular_radius: SUN_RADIUS,
        ..ExtractOptions::default()
    }
}

fn sun_dir(elevation_deg: f32) -> [f32; 3] {
    let e = elevation_deg.to_radians();
    [e.cos(), -e.sin(), 0.0]
}

// --- ao_contact ------------------------------------------------------------------------------

fn ao_scene(cube: bool) -> LabScene {
    let mut meshes = vec![ground(40.0, 0)];
    if cube {
        meshes.extend(cuboid(Vec3::new(-1.0, 0.0, -0.5), Vec3::new(0.0, 1.0, 0.5), 0));
    }
    let mut scene = lab(
        (256, 256),
        ([0.6, 40.0, 0.8], [0.6, 0.0, 0.0], 2.0 * (1.5f32 / 40.0).atan()),
        12.0,
        DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        vec![grey(0.5)],
        meshes,
    );
    scene.directional_shadows = false;
    scene.ambient_lux = 20_000.0;
    scene
}

/// `spec27_calibration.rs` `score_ao` for the given switches: (raster darkening, tracer
/// darkening) in the 10 cm band, and the raster frame.
fn ao_band(s: Switches, pt: &PtImage, pt_open: &PtImage) -> (f64, f64, Vec<[f32; 3]>) {
    let quality = desktop_high(s);
    let (with, without) = (ao_scene(true), ao_scene(false));
    let rt = raster(&with, quality, 8);
    let rt_open = raster(&without, quality, 8);
    let (w, h) = (pt.width as usize, pt.height as usize);
    let pitch = 3.0 / w as f32;
    let ground_depth = pt_open.depth[(h / 2) * w + w / 2];
    let top = |i: usize| pt.depth[i] > 0.0 && pt.depth[i] < ground_depth - 0.5;
    let is_ground = |i: usize| pt.depth[i] > 0.0 && pt.normal[i][1] > 0.9 && !top(i);
    let edges: Vec<(usize, usize, isize)> = (0..h)
        .filter_map(|y| {
            let i = |x: usize| y * w + x;
            let (edge, step) = (1..w).find_map(|x| match (top(i(x - 1)), top(i(x))) {
                (true, false) => Some((x, 1isize)),
                (false, true) => Some((x - 1, -1isize)),
                _ => None,
            })?;
            let mut x = edge as isize;
            for _ in 0..4 {
                if (0..w as isize).contains(&x) && is_ground(i(x as usize)) {
                    return Some((y, x as usize, step));
                }
                x += step;
            }
            None
        })
        .collect();
    assert!(edges.len() > 40, "rows crossing the cube edge: {}", edges.len());
    let middle = &edges[edges.len() / 4..3 * edges.len() / 4];
    let band = |img: &[[f32; 3]], open: &[[f32; 3]]| -> f64 {
        let (mut sum, mut n) = (0.0f64, 0usize);
        for &(y, x0, step) in middle {
            for k in 0..w as isize {
                let x = x0 as isize + k * step;
                if (k as f32 + 0.5) * pitch > 0.10 || !(0..w as isize).contains(&x) {
                    break;
                }
                let i = y * w + x as usize;
                sum += f64::from(luminance(img[i]) / luminance(open[i]).max(1e-6));
                n += 1;
            }
        }
        1.0 - sum / n.max(1) as f64
    };
    (band(&rt, &rt_open), band(&pt.beauty, &pt_open.beauty), rt)
}

/// `ao_contact` shipped and with the horizon AO: the calibrated row within 15 % of the tracer.
#[test]
#[ignore = "hand-run (ADR-3202); needs the frame hookup patch for the horizon AO"]
fn rescore_ao_contact() {
    let _gpu = gpu();
    let spp = 512;
    let quality = desktop_high(SHIPPED);
    let pt = trace(
        &ao_scene(true).as_frame_scene_with_quality(quality),
        &ExtractOptions::default(),
        spp,
        4,
    );
    let pt_open = trace(
        &ao_scene(false).as_frame_scene_with_quality(quality),
        &ExtractOptions::default(),
        spp,
        4,
    );
    let (d_old, d_pt, old) = ao_band(SHIPPED, &pt, &pt_open);
    let (d_new, _, new) = ao_band(CALIBRATED, &pt, &pt_open);
    plate_linear("ao_contact_shipped", pt.width, pt.height, &old, 12.0);
    plate_linear("ao_contact_calibrated", pt.width, pt.height, &new, 12.0);
    plate_linear("ao_contact_pt", pt.width, pt.height, &pt.beauty, 12.0);
    println!(
        "| ao_contact | shipped {:.1} % / PT {:.1} % = {:.2}× | horizon AO {:.1} % = {:.2}× |",
        100.0 * d_old,
        100.0 * d_pt,
        d_old / d_pt,
        100.0 * d_new,
        d_new / d_pt
    );
    assert!(
        old != new,
        "the horizon AO did not change the frame: apply regia-scratch/reports/sprint-ao-penumbra-calibration-frame.patch"
    );
    let k = d_new / d_pt.max(1e-6);
    assert!((k - 1.0).abs() <= 0.15, "ao_contact with the horizon AO: {k:.2}×");
}

// --- Sun penumbra and contact gap ------------------------------------------------------------

fn sun_scene(size: (u32, u32), eye: [f32; 3], target: [f32; 3], fov_y: f32) -> LabScene {
    let mut materials = vec![grey(0.5), grey(0.5)];
    materials[1].double_sided = true;
    let mut meshes = vec![
        ground(60.0, 0),
        quad(Vec3::new(0.0, 15.0, -6.0), Vec3::Z * 6.0, Vec3::Y * 15.0, 1),
    ];
    meshes.extend(cuboid(Vec3::new(2.0, 0.0, 4.0), Vec3::new(3.0, 1.0, 5.0), 0));
    lab(
        size,
        (eye, target, fov_y),
        SUN_EV,
        DirectionalLightUniform::new(sun_dir(SUN_ELEVATION_DEG), [1.0; 3], SUN_LUX),
        materials,
        meshes,
    )
}

fn sun_visibility(scene: &LabScene, quality: FrameQuality) -> Vec<f32> {
    let lit = raster(scene, quality, 3);
    let mut open = scene.clone();
    open.directional_shadows = false;
    let unshadowed = raster(&open, quality, 3);
    lit.iter()
        .zip(&unshadowed)
        .map(|(n, d)| {
            let d = luminance(*d);
            if d > 1e-4 {
                (luminance(*n) / d).clamp(0.0, 1.5)
            } else {
                1.0
            }
        })
        .collect()
}

fn penumbra_px(pt_vis: &[f32], rt_vis: &[f32], w: usize, h: usize) -> (Option<f32>, Option<f32>) {
    let strip = (h / 20).max(2);
    let profile = |vis: &[f32], along_x: bool| -> Vec<f32> {
        if along_x {
            (0..w)
                .map(|x| {
                    (h / 2 - strip..h / 2 + strip)
                        .map(|y| vis[y * w + x])
                        .sum::<f32>()
                        / (2 * strip) as f32
                })
                .collect()
        } else {
            (0..h)
                .map(|y| {
                    (w / 2 - strip..w / 2 + strip)
                        .map(|x| vis[y * w + x])
                        .sum::<f32>()
                        / (2 * strip) as f32
                })
                .collect()
        }
    };
    let range =
        |p: &[f32]| p.iter().copied().fold(f32::MIN, f32::max) - p.iter().copied().fold(f32::MAX, f32::min);
    let along_x = range(&profile(pt_vis, true)) > range(&profile(pt_vis, false));
    (
        edge_width(&profile(pt_vis, along_x), 0.1, 0.9),
        edge_width(&profile(rt_vis, along_x), 0.1, 0.9),
    )
}

/// The penumbra rows at 1, 5 and 20 m and the contact gap, shipped and with the angular
/// penumbra: every calibrated row within its target.
#[test]
#[ignore = "hand-run (ADR-3202)"]
fn rescore_sun_penumbra_and_contact_gap() {
    let _gpu = gpu();
    let e = SUN_ELEVATION_DEG.to_radians();
    let (n, height, fov) = (512u32, 4.0f32, 20f32.to_radians());
    let pitch_mm = 2.0 * height * (0.5 * fov).tan() / n as f32 * 1000.0;
    let mut failures = Vec::new();
    for d in [1.0f32, 5.0, 20.0] {
        let p = [d * e.cos(), 0.0, 0.0];
        let scene = sun_scene((n, n), [p[0], height, 0.08], p, fov);
        let pt = trace(
            &scene.as_frame_scene_with_quality(desktop_high(SHIPPED)),
            &sun_options(),
            128,
            1,
        );
        plate_grey(&format!("sun_penumbra_{d}m_pt"), n, n, &pt.shadow);
        let mut cells = Vec::new();
        for s in [SHIPPED, CALIBRATED] {
            let rt = sun_visibility(&scene, desktop_high(s));
            let tag = if s.angular_sun_penumbra {
                "calibrated"
            } else {
                "shipped"
            };
            plate_grey(&format!("sun_penumbra_{d}m_{tag}"), n, n, &rt);
            let (pt_w, rt_w) = penumbra_px(&pt.shadow, &rt, n as usize, n as usize);
            let (Some(pw), Some(rw)) = (pt_w, rt_w) else {
                failures.push(format!("{d} m {s:?}: edge not resolved ({pt_w:?}, {rt_w:?})"));
                continue;
            };
            let k = rw / pw;
            cells.push(format!(
                "{:.1} mm / {:.1} mm = {k:.2}×",
                rw * pitch_mm,
                pw * pitch_mm
            ));
            if s.angular_sun_penumbra && !(0.8..=1.2).contains(&k) {
                failures.push(format!("sun_penumbra_{d}m calibrated: {k:.2}×"));
            }
        }
        println!("| sun_penumbra_{d}m | {} |", cells.join(" | "));
    }
    // Contact gap at 1440p (`spec27_calibration.rs`): the frame's centred 512×288 window.
    let (fw, fh) = (2560u32, 1440u32);
    let (cw, ch) = (512u32, 288u32);
    let (eye, target, fov) = ([6.0, 1.6, 4.5], [3.0, 0.05, 4.5], 60f32.to_radians());
    let crop_fov = 2.0 * ((0.5 * fov).tan() * ch as f32 / fh as f32).atan();
    let window = sun_scene((cw, ch), eye, target, crop_fov);
    let pt = trace(
        &window.as_frame_scene_with_quality(desktop_high(SHIPPED)),
        &sun_options(),
        16,
        1,
    );
    let (w, h) = (cw as usize, ch as usize);
    let (ox, oy) = (((fw - cw) / 2) as usize, ((fh - ch) / 2) as usize);
    let face = |i: usize| pt.depth[i] > 0.0 && pt.normal[i][0] > 0.9;
    let floor = |i: usize| pt.depth[i] > 0.0 && pt.normal[i][1] > 0.9;
    let mut cells = Vec::new();
    for s in [SHIPPED, CALIBRATED] {
        let rt_full = sun_visibility(&sun_scene((fw, fh), eye, target, fov), desktop_high(s));
        let rt: Vec<f32> = (0..w * h)
            .map(|i| rt_full[(oy + i / w) * fw as usize + ox + i % w])
            .collect();
        let mut gaps = Vec::new();
        for x in 0..w {
            let Some(yf) = (1..h - 4).rev().find(|&y| face(y * w + x)) else {
                continue;
            };
            let below = (yf + 1) * w + x;
            if !floor(below) || pt.shadow[below + 2 * w] >= 0.5 {
                continue;
            }
            let mut k = 0;
            while yf + 1 + k < h
                && floor((yf + 1 + k) * w + x)
                && pt.shadow[(yf + 1 + k) * w + x] < 0.5
                && rt[(yf + 1 + k) * w + x] > 0.5
            {
                k += 1;
            }
            gaps.push(k as f32);
        }
        assert!(gaps.len() > 100, "contact columns: {}", gaps.len());
        let p95 = quantile(&gaps, 0.95);
        cells.push(format!("p95 {p95:.0} px over {} columns", gaps.len()));
        if p95 > 1.0 {
            failures.push(format!("contact gap {s:?}: p95 {p95} px"));
        }
    }
    println!("| sun_contact_gap | {} |", cells.join(" | "));
    assert!(failures.is_empty(), "{failures:#?}");
}

// --- GI rows that carry the SSAO -------------------------------------------------------------

/// `gi_validation.rs`'s Desktop High base quality with the switches.
fn gi_base(s: Switches) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.msaa_samples = 1;
    q.render_scale_permille = 1000;
    q.horizontal_plus = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.ssr = false;
    q.reflection_probes = false;
    q.gi = GiMode::Ambient;
    q.screen_space_gi = false;
    q.gi_probes = false;
    q.baked_gi = false;
    q.horizon_ao = s.horizon_ao;
    q.angular_sun_penumbra = s.angular_sun_penumbra;
    q
}

fn gi_capture(
    scene: &GiScene,
    probes: bool,
    lab: &LabScene,
    quality: FrameQuality,
    frames: u32,
) -> Vec<[f32; 3]> {
    let mut r = FrameRenderer::new();
    r.set_ddgi_settings(scene.ddgi.clone());
    r.set_ddgi_enabled(Some(probes));
    capture(&mut r, &lab.as_frame_scene_with_quality(quality), frames)
}

/// Mean FLIP of the indirect light (ambient, SSGI, probes + SSGI) against the tracer for one GI
/// scene, shipped and calibrated. `gi_validation` keeps Desktop High's GTAO half-resolution row,
/// so these rows carry the SSAO kernel.
fn gi_rows(scene: &GiScene) {
    let frame = scene.lab.as_frame_scene_with_quality(gi_base(SHIPPED));
    let pt = from_frame(&frame, &ExtractOptions::default());
    let image = tracer()
        .render(
            &pt,
            &PtSettings {
                spp: 256,
                max_bounces: 6,
                seed: 23,
                jitter: false,
                samples_per_submit: 8,
                ..PtSettings::default()
            },
        )
        .expect("trace the reference");
    let surface = surface_mask(&image.depth, GI_WIDTH, GI_HEIGHT, 0.05);
    let shadow = shadow_mask(&surface, &image.shadow, &image.direct, 0.05);
    let reference = add_planes(&image.indirect_diffuse, &image.indirect_specular);
    let mut direct_lab = scene.lab.clone();
    direct_lab.ambient_lux = 0.0;
    let direct = gi_capture(
        scene,
        false,
        &direct_lab,
        FrameQuality {
            ssao: regia_render::SsaoMode::Off,
            ..gi_base(SHIPPED)
        },
        2,
    );
    for (name, probes, frames, tweak) in [
        ("ambient", false, 2u32, (GiMode::Ambient, false, false)),
        ("ssgi", false, 4, (GiMode::ScreenSpace, true, false)),
        ("probes_ssgi", true, 64, (GiMode::Probes, true, true)),
    ] {
        let mut cells = Vec::new();
        for s in [SHIPPED, CALIBRATED] {
            let q = FrameQuality {
                gi: tweak.0,
                screen_space_gi: tweak.1,
                gi_probes: tweak.2,
                ..gi_base(s)
            };
            let full = gi_capture(scene, probes, &scene.lab, q, frames);
            let plane = sub_planes(&full, &direct);
            let e = measure(&GiErrorInput {
                reference: &reference,
                test: &plane,
                surface: &surface,
                shadow: &shadow,
                width: GI_WIDTH,
                height: GI_HEIGHT,
                ev100: scene.lab.exposure_ev,
                scale: scene.scale,
            });
            cells.push(format!(
                "FLIP {:.3}, energy {:.2}",
                e.flip_mean,
                e.energy_ratio.unwrap_or(f32::NAN)
            ));
        }
        println!("| gi {} / {name} | {} |", scene.name, cells.join(" | "));
    }
}

/// The GI rows with the SSAO in their frames, shipped and calibrated (reported, not asserted:
/// the GI targets belong to the GI lanes; the horizon AO must not make them worse by much).
#[test]
#[ignore = "hand-run (ADR-3202)"]
fn rescore_gi_rows_with_ao() {
    let _gpu = gpu();
    for scene in [trench(), color_room()] {
        gi_rows(&scene);
    }
}
