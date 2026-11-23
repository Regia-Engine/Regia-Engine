// SPDX-License-Identifier: MPL-2.0
//! `specs/26` § 4 "Reflections" and `specs/27` § 2.2 "GI (specular)" for local reflection
//! captures (ADR-2860): a closed room with a mirror floor under a lit red cube. The floor's
//! indirect specular (the full frame minus a frame without image light) against the path tracer's
//! `indirect_specular` AOV, mean FLIP over the floor pixels.
//!
//! The room is closed and the environment is black, so without a capture the mirror shows nothing
//! the camera does not see: no cube, no walls. With one capture baked at the room's centre (box =
//! the room) the floor shows the cube and the lit walls from the capture's HDR map. Reported with
//! screen reflections off (the capture alone) and with stochastic SSR (on-screen hits, misses
//! keep the capture).
//!
//! Two references: the full transport (six path vertices) and the transport a capture stands for
//! (two: the mirror, then a directly lit wall). The real-time walls carry no interreflection of
//! the point light (the probes trace no local light), so the full reference measures that GI gap
//! as well as the capture; the two-vertex one measures the capture.
//!
//! The two renderers' point lights also fall off differently: the path tracer's as `1 / d²`, the
//! real-time one's as a windowed `1 / (d² + 1)`, about 10 % under at the walls of a 6 m room. That
//! is direct light, not reflection, and the capture inherits it. The second test measures it and
//! then removes it: the same room four times larger, where `d² + 1` and `d²` agree.

use regia_core::glam::Vec3;
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtSettings};
use regia_render::{
    DirectionalLightUniform, EnvironmentAuthority, FrameQuality, FrameRenderer, GiMode, LabLocalLight,
    LabMaterial, LabMesh, LabPointLight, LabScene, LocalCaptureDesc, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SsaoMode, StudioEquirect,
};

const W: u32 = 160;
const H: u32 = 96;
const EV: f32 = 7.0;
const EYE: [f32; 3] = [0.0, 1.5, 2.6];
const TARGET: [f32; 3] = [0.0, 0.3, -1.0];
const FOV: f32 = std::f32::consts::FRAC_PI_3; // 60°
/// The room: x and z within ±HALF, y from 0 to HEIGHT.
const HALF: f32 = 3.0;
const HEIGHT: f32 = 3.0;
/// The cube floats over the floor: centre and half size.
const CUBE: [f32; 3] = [0.0, 0.9, -0.6];
const CUBE_HALF: f32 = 0.35;
const LIGHT: [f32; 3] = [0.6, 2.5, 0.8];

fn slab(center: [f32; 3], half: [f32; 3], material: usize) -> LabMesh {
    let (v, i) = regia_render::box_mesh(half);
    LabMesh {
        vertices: regia_render::translate_mesh(&v, center),
        indices: i,
        material,
        ..LabMesh::unskinned()
    }
}

/// `v` scaled about the origin.
fn scaled(v: [f32; 3], s: f32) -> [f32; 3] {
    v.map(|c| c * s)
}

/// The room at `s` times its size. The light's intensity grows with `s²`, so the walls keep their
/// inverse-square illuminance, and its reach with `s`.
fn scene(s: f32) -> LabScene {
    let t = 0.1 * s;
    let (half_x, height) = (HALF * s, HEIGHT * s);
    let post = PostSettings {
        exposure_ev: EV,
        ..PostSettings::golden_deterministic()
    };
    LabScene {
        width: W,
        height: H,
        eye: scaled(EYE, s),
        target: scaled(TARGET, s),
        fov_y: FOV,
        near: 0.05 * s,
        far: 20.0 * s,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: EV,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post),
        // No sun: the room is closed.
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0, 1.0, 1.0], 0.0),
        local_lights: vec![LabLocalLight::Point(LabPointLight {
            position: scaled(LIGHT, s),
            color: [1.0, 0.95, 0.9],
            intensity: 6000.0 * s * s,
            radius: 12.0 * s,
            cast_shadows: false,
            source_radius: None,
        })],
        materials: vec![
            // 0: the mirror floor.
            LabMaterial {
                base_color: [0.95, 0.95, 0.95, 1.0],
                metallic: 1.0,
                roughness: 0.02,
                ..LabMaterial::default()
            },
            // 1: walls and ceiling.
            LabMaterial {
                base_color: [0.6, 0.6, 0.6, 1.0],
                metallic: 0.0,
                roughness: 0.9,
                ..LabMaterial::default()
            },
            // 2: the red cube.
            LabMaterial {
                base_color: [0.8, 0.12, 0.08, 1.0],
                metallic: 0.0,
                roughness: 0.7,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![
            slab([0.0, -t, 0.0], [half_x + t, t, half_x + t], 0),
            slab([0.0, height + t, 0.0], [half_x + t, t, half_x + t], 1),
            slab([-half_x - t, height / 2.0, 0.0], [t, height / 2.0, half_x], 1),
            slab([half_x + t, height / 2.0, 0.0], [t, height / 2.0, half_x], 1),
            slab([0.0, height / 2.0, -half_x - t], [half_x, height / 2.0, t], 1),
            slab([0.0, height / 2.0, half_x + t], [half_x, height / 2.0, t], 1),
            slab(scaled(CUBE, s), [CUBE_HALF * s; 3], 2),
        ],
    }
}

fn quality(ssr: bool) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.volumetric_clouds = false;
    q.volumetric_fog = false;
    q.physical_atmosphere = false;
    q.taau = false;
    q.ssr = ssr;
    // GPU-driven draws select the stochastic pass.
    q.gpu_driven = ssr;
    q
}

/// A black studio environment: a closed room sees no sky, in either renderer.
fn black_renderer(device: &RenderDevice) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_environment_authority(EnvironmentAuthority::Studio);
    let image = StudioEquirect::new(64, 32, vec![[0.0; 3]; 64 * 32]).expect("black image");
    assert!(renderer.set_studio_equirect(device, Some(&image)), "installed");
    renderer
}

/// Real-time indirect light (full − no image light) after `frames` frames, with a capture of the
/// room when `capture`.
fn realtime_indirect(
    device: &RenderDevice,
    quality: FrameQuality,
    capture: bool,
    frames: u32,
    s: f32,
) -> Vec<[f32; 3]> {
    realtime_indirect_in(device, quality, capture, frames, true, s)
}

/// IEEE half to f32 (the capture chain's texels).
fn half(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from((bits >> 10) & 0x1f);
    let frac = f32::from(bits & 0x3ff);
    match exp {
        0 => sign * frac * 2f32.powi(-24),
        31 => f32::NAN,
        _ => sign * (1.0 + frac / 1024.0) * 2f32.powi(exp - 15),
    }
}

/// [`realtime_indirect`] in the black studio environment (`black`) or the default sky.
fn realtime_indirect_in(
    device: &RenderDevice,
    quality: FrameQuality,
    capture: bool,
    frames: u32,
    black: bool,
    s: f32,
) -> Vec<[f32; 3]> {
    let lab = scene(s);
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut renderer = if black {
        black_renderer(device)
    } else {
        FrameRenderer::new()
    };
    if capture {
        renderer
            .place_local_capture(LocalCaptureDesc::new(
                1,
                [0.0, HEIGHT * s / 2.0, 0.0],
                [HALF * s, HEIGHT * s / 2.0, HALF * s],
            ))
            .expect("place");
        let info = renderer.bake_local_capture(device, &frame, 1).expect("bake");
        assert!(
            info.coverage > 0.99,
            "a closed room covers every direction: {}",
            info.coverage
        );
        // The map's mean premultiplied radiance (mip 0, over the sky IBL scale): what the
        // capture saw of the lit room.
        if let Some(bake) = renderer.local_captures()[0].desc.bake.clone() {
            let texels = bake.mips[0].len() / 8;
            let mut sum = [0.0f64; 4];
            for t in bake.mips[0].as_chunks::<8>().0 {
                for (k, s) in sum.iter_mut().enumerate() {
                    *s += f64::from(half(u16::from_le_bytes([t[2 * k], t[2 * k + 1]])));
                }
            }
            let mean = sum.map(|v| v / texels.max(1) as f64);
            eprintln!("capture map mean rgba {mean:?} (black environment {black})");
        }
    }
    for i in 0..frames {
        if i + 1 == frames {
            renderer.request_hdr_capture();
        }
        renderer.render_offscreen(device, &frame).expect("frame");
    }
    let full = renderer.take_hdr_capture(device).expect("readback").expect("hdr");
    let mask = floor_mask();
    let (r, g) = masked_red(&full.rgb, &mask);
    eprintln!("full frame floor mean red/green {r:.3}/{g:.3} (capture {capture}, black environment {black})");
    let mut direct = frame.clone();
    direct.ibl = false;
    direct.ambient_lux = 0.0;
    direct.quality.gi = GiMode::Ambient;
    direct.quality.screen_space_gi = false;
    direct.quality.ssr = false;
    direct.quality.reflection_probes = false;
    direct.quality.ssao = SsaoMode::Off;
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, &direct).expect("direct frame");
    let sun = renderer.take_hdr_capture(device).expect("readback").expect("hdr");
    full.rgb
        .iter()
        .zip(&sun.rgb)
        .map(|(f, d)| {
            [
                (f[0] - d[0]).max(0.0),
                (f[1] - d[1]).max(0.0),
                (f[2] - d[2]).max(0.0),
            ]
        })
        .collect()
}

/// Floor pixels: the camera ray reaches the floor inside the room before the cube (one pixel's
/// margin off the cube's silhouette and the walls).
fn floor_mask() -> Vec<bool> {
    let eye = Vec3::from_array(EYE);
    let view = regia_core::glam::camera::rh::view::look_at_mat4(eye, Vec3::from_array(TARGET), Vec3::Y);
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(FOV, W as f32 / H as f32, 0.05, 20.0);
    let inv = (proj * view).inverse();
    let cube = Vec3::from_array(CUBE);
    let hits_cube = |dir: Vec3| {
        let half = CUBE_HALF * 1.08;
        let inv_d = dir.recip();
        let t0 = (cube - Vec3::splat(half) - eye) * inv_d;
        let t1 = (cube + Vec3::splat(half) - eye) * inv_d;
        let near = t0.min(t1).max_element();
        let far = t0.max(t1).min_element();
        far >= near.max(0.0)
    };
    let floor = |x: f32, y: f32| {
        let ndc = Vec3::new(x / W as f32 * 2.0 - 1.0, 1.0 - y / H as f32 * 2.0, 1.0);
        let dir = (inv.project_point3(ndc) - eye).normalize();
        if dir.y >= -1e-3 {
            return false;
        }
        let t = -eye.y / dir.y;
        let p = eye + dir * t;
        p.x.abs() < HALF - 0.1 && p.z.abs() < HALF - 0.1 && !hits_cube(dir)
    };
    (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| floor(x as f32 + 0.5, y as f32 + 0.5))
        .collect()
}

/// Wall and ceiling pixels: the camera ray misses the cube and does not end on the floor (the mask
/// is the same at every room scale; the camera scales with the room).
fn wall_mask() -> Vec<bool> {
    let floor = floor_mask();
    let eye = Vec3::from_array(EYE);
    let view = regia_core::glam::camera::rh::view::look_at_mat4(eye, Vec3::from_array(TARGET), Vec3::Y);
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(FOV, W as f32 / H as f32, 0.05, 20.0);
    let inv = (proj * view).inverse();
    let cube = Vec3::from_array(CUBE);
    (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .zip(&floor)
        .map(|((x, y), on_floor)| {
            let ndc = Vec3::new(
                (x as f32 + 0.5) / W as f32 * 2.0 - 1.0,
                1.0 - (y as f32 + 0.5) / H as f32 * 2.0,
                1.0,
            );
            let dir = (inv.project_point3(ndc) - eye).normalize();
            let half = CUBE_HALF * 1.08;
            let inv_d = dir.recip();
            let t0 = (cube - Vec3::splat(half) - eye) * inv_d;
            let t1 = (cube + Vec3::splat(half) - eye) * inv_d;
            let hits = t0.max(t1).min_element() >= t0.min(t1).max_element().max(0.0);
            // A downward ray whose floor point lies outside the room ends on a wall first; the
            // floor's margin along the walls is neither.
            let t = -eye.y / dir.y.min(-1e-3);
            let p = eye + dir * t;
            let on_wall = dir.y >= -1e-3 || p.x.abs() > HALF || p.z.abs() > HALF;
            !*on_floor && !hits && on_wall
        })
        .collect()
}

/// Real-time direct light (no image light, no GI, no screen effects) at room scale `s`.
fn realtime_direct(device: &RenderDevice, s: f32) -> Vec<[f32; 3]> {
    let lab = scene(s);
    let mut frame = lab.as_frame_scene_with_quality(quality(false));
    frame.ibl = false;
    frame.ambient_lux = 0.0;
    frame.quality.gi = GiMode::Ambient;
    frame.quality.screen_space_gi = false;
    frame.quality.ssr = false;
    frame.quality.reflection_probes = false;
    frame.quality.ssao = SsaoMode::Off;
    let mut renderer = black_renderer(device);
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, &frame).expect("direct frame");
    renderer
        .take_hdr_capture(device)
        .expect("readback")
        .expect("hdr")
        .rgb
}

/// The real-time walls' direct light over the path tracer's: a closed room's walls are what a
/// capture holds, so a floor reflection cannot read closer to the reference than this.
fn wall_direct_ratio(realtime: &[[f32; 3]], traced: &[[f32; 3]]) -> f32 {
    let (rt, _) = masked_red(realtime, &wall_mask());
    let (pt, _) = masked_red(traced, &wall_mask());
    rt / pt.max(1e-6)
}

/// Mean FLIP of `test` against `reference` over `mask` (outside it both read the reference).
fn masked_flip(reference: &[[f32; 3]], test: &[[f32; 3]], mask: &[bool]) -> f32 {
    let blended: Vec<[f32; 3]> = test
        .iter()
        .zip(reference)
        .zip(mask)
        .map(|((t, r), m)| if *m { *t } else { *r })
        .collect();
    let (_, flip) = regia_render::compare::compare_linear(
        reference,
        &blended,
        W,
        H,
        EV,
        regia_render::compare::DEFAULT_PPD,
    );
    let (sum, n) = flip
        .map
        .iter()
        .zip(mask)
        .filter(|(_, m)| **m)
        .fold((0.0f64, 0usize), |(s, n), (v, _)| (s + f64::from(*v), n + 1));
    (sum / n.max(1) as f64) as f32
}

/// Mean red minus mean green over `mask`: the cube's reflection is red, the walls are grey.
fn masked_red(img: &[[f32; 3]], mask: &[bool]) -> (f32, f32) {
    let (r, g, n) = img
        .iter()
        .zip(mask)
        .filter(|(_, m)| **m)
        .fold((0.0f64, 0.0f64, 0usize), |(r, g, n), (c, _)| {
            (r + f64::from(c[0]), g + f64::from(c[1]), n + 1)
        });
    let n = n.max(1) as f64;
    ((r / n) as f32, (g / n) as f32)
}

/// `REGIA_CAPTURE_DUMP=<dir>` writes PT | PT two vertices | capture | capture + SSSR | none side by side
/// (`capture_mirror.png`), at the test's exposure, for review by eye.
fn dump(images: &[&[[f32; 3]]]) {
    let Some(dir) = regia_core::brand::env_var_os("REGIA_CAPTURE_DUMP") else {
        return;
    };
    let exposure = 1.0 / (1.2 * 2f32.powf(EV));
    let encode = |v: f32| {
        let v = (v * exposure).clamp(0.0, 1.0);
        let s = if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0 + 0.5) as u8
    };
    let width = W * images.len() as u32;
    let mut rgb = Vec::with_capacity((width * H * 3) as usize);
    for y in 0..H {
        for img in images {
            for x in 0..W {
                let c = img[(y * W + x) as usize];
                rgb.extend(c.iter().map(|v| encode(*v)));
            }
        }
    }
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("dump dir");
    let file = std::fs::File::create(dir.join("capture_mirror.png")).expect("dump file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, H);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut w| w.write_image_data(&rgb))
        .expect("dump png");
}

#[test]
fn a_mirror_floor_shows_the_lit_cube_from_its_local_capture() {
    let device = RenderDevice::headless().expect("headless device");
    let lab = scene(1.0);
    let frame = lab.as_frame_scene_with_quality(quality(false));
    let pt_scene = from_frame(
        &frame,
        &ExtractOptions {
            sky: None,
            terrain: None,
            skip_blended: true,
            ..ExtractOptions::default()
        },
    );
    let tracer = PathTracer::new(&device).expect("compile");
    let trace = |max_bounces: u32| {
        tracer
            .render(
                &pt_scene,
                &PtSettings {
                    spp: 1024,
                    max_bounces,
                    ..PtSettings::default()
                },
            )
            .expect("trace")
    };
    // The full transport: the closed room's interreflection lights its walls about three times
    // over, which the real-time walls (direct light; the probes trace no local light) lack.
    let reference = trace(6).indirect_specular;
    // The transport a capture stands for: the mirror floor, then a wall lit directly (two path
    // vertices). This measures the capture itself, apart from the room's missing bounce.
    let matched_frame = trace(2);
    let matched = matched_frame.indirect_specular;
    let wall_ratio = wall_direct_ratio(&realtime_direct(&device, 1.0), &matched_frame.direct);
    eprintln!("6 m room: real-time wall direct light / PT = {wall_ratio:.3} (1 / (d² + 1) against 1 / d²)");
    let mask = floor_mask();
    let floor_pixels = mask.iter().filter(|m| **m).count();
    assert!(floor_pixels > 2000, "floor pixels: {floor_pixels}");
    let captured = realtime_indirect(&device, quality(false), true, 3, 1.0);
    let captured_sssr = realtime_indirect(&device, quality(true), true, 24, 1.0);
    let none = realtime_indirect(&device, quality(false), false, 3, 1.0);
    dump(&[&reference, &matched, &captured, &captured_sssr, &none]);
    let (pt_r, pt_g) = masked_red(&reference, &mask);
    let (m_r, m_g) = masked_red(&matched, &mask);
    let (cap_r, cap_g) = masked_red(&captured, &mask);
    let (none_r, none_g) = masked_red(&none, &mask);
    eprintln!(
        "floor mean red/green: PT {pt_r:.2}/{pt_g:.2}, PT two vertices {m_r:.2}/{m_g:.2}, capture {cap_r:.2}/{cap_g:.2}, \
         none {none_r:.2}/{none_g:.2}"
    );
    let flip_capture = masked_flip(&reference, &captured, &mask);
    let flip_sssr = masked_flip(&reference, &captured_sssr, &mask);
    let flip_none = masked_flip(&reference, &none, &mask);
    let matched_capture = masked_flip(&matched, &captured, &mask);
    let matched_sssr = masked_flip(&matched, &captured_sssr, &mask);
    let matched_none = masked_flip(&matched, &none, &mask);
    eprintln!(
        "mirror floor indirect specular FLIP vs PT: capture {flip_capture:.4}, capture + SSSR {flip_sssr:.4}, \
         no capture {flip_none:.4} (specs/27 § 2.2 target 0.10: {})",
        if flip_capture <= 0.10 { "met" } else { "not met" }
    );
    eprintln!(
        "mirror floor indirect specular FLIP vs PT two vertices: capture {matched_capture:.4}, capture + SSSR \
         {matched_sssr:.4}, no capture {matched_none:.4} (target 0.10: {})",
        if matched_capture <= 0.10 { "met" } else { "not met" }
    );
    // The mirror shows the room: without a capture the black environment leaves it nearly empty.
    assert!(
        none_r < 0.25 * m_r,
        "no capture, no reflection: {none_r} vs PT two vertices {m_r}"
    );
    assert!(
        cap_r > 0.5 * m_r && cap_r < 2.0 * m_r,
        "the capture shows the directly lit room and cube: {cap_r} vs PT two vertices {m_r}"
    );
    assert!(
        cap_r > cap_g,
        "the red cube is in the reflection: red {cap_r} green {cap_g}"
    );
    assert!(
        matched_capture <= matched_none - 0.1 && flip_capture <= flip_none - 0.05,
        "the capture moves the floor toward the path tracer: {matched_capture} vs {matched_none} without \
         (two vertices), {flip_capture} vs {flip_none} (full)"
    );
    assert!(
        matched_capture <= MATCHED_FLIP_BOUND && matched_sssr <= MATCHED_FLIP_BOUND,
        "capture FLIP {matched_capture}, capture + SSSR {matched_sssr} over {MATCHED_FLIP_BOUND} (two vertices)"
    );
    assert!(
        flip_capture <= CAPTURE_FLIP_BOUND && flip_sssr <= CAPTURE_FLIP_BOUND,
        "capture FLIP {flip_capture}, capture + SSSR {flip_sssr} over {CAPTURE_FLIP_BOUND} (full transport)"
    );
}

/// The measured FLIP against the two-vertex reference plus margin (lavapipe, 2026-11-05: capture
/// 0.123, capture + SSSR 0.114, no capture 0.390; ADR-2860); `specs/27` § 2.2's 0.10 is the goal.
const MATCHED_FLIP_BOUND: f32 = 0.15;
/// The measured FLIP against the full transport plus margin (capture 0.580, capture + SSSR 0.574,
/// no capture 0.753): the room's missing interreflection (a GI limit, not the capture's) keeps it
/// over the 0.10 goal.
const CAPTURE_FLIP_BOUND: f32 = 0.62;

/// The room four times larger, where the two renderers' point lights agree (`d² + 1` against `d²`),
/// against the two-vertex reference: this is what is left of the capture itself.
#[test]
fn in_a_large_hall_the_lights_agree_and_the_mirror_floor_measures_the_capture() {
    const S: f32 = 4.0;
    let device = RenderDevice::headless().expect("headless device");
    let lab = scene(S);
    let frame = lab.as_frame_scene_with_quality(quality(false));
    let pt_scene = from_frame(
        &frame,
        &ExtractOptions {
            sky: None,
            terrain: None,
            skip_blended: true,
            ..ExtractOptions::default()
        },
    );
    let tracer = PathTracer::new(&device).expect("compile");
    let matched_frame = tracer
        .render(
            &pt_scene,
            &PtSettings {
                spp: 1024,
                max_bounces: 2,
                ..PtSettings::default()
            },
        )
        .expect("trace");
    let matched = matched_frame.indirect_specular;
    let wall_ratio = wall_direct_ratio(&realtime_direct(&device, S), &matched_frame.direct);
    let mask = floor_mask();
    let captured = realtime_indirect(&device, quality(false), true, 3, S);
    let captured_sssr = realtime_indirect(&device, quality(true), true, 24, S);
    let none = realtime_indirect(&device, quality(false), false, 3, S);
    let (m_r, m_g) = masked_red(&matched, &mask);
    let (cap_r, cap_g) = masked_red(&captured, &mask);
    let flip_capture = masked_flip(&matched, &captured, &mask);
    let flip_sssr = masked_flip(&matched, &captured_sssr, &mask);
    let flip_none = masked_flip(&matched, &none, &mask);
    eprintln!(
        "24 m hall: wall direct light / PT = {wall_ratio:.3}; floor mean red/green PT two vertices {m_r:.2}/{m_g:.2}, \
         capture {cap_r:.2}/{cap_g:.2}; FLIP capture {flip_capture:.4}, capture + SSSR {flip_sssr:.4}, no capture \
         {flip_none:.4}"
    );
    assert!(
        flip_capture < flip_none,
        "the capture moves the floor toward the path tracer: {flip_capture} vs {flip_none}"
    );
}
