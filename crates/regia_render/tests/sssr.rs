// SPDX-License-Identifier: MPL-2.0
//! Stochastic screen-space reflections (FidelityFX SSSR port, `sssr.rs`) on Desktop High.
//!
//! - `specs/27` § 2.2: a mirror floor under a lit object shows the object. DO27 renders Desktop
//!   High at 0.67 (ADR-041), where the older pass is skipped, so the cockpit floor lost it.
//! - A glossy floor shows a blurred reflection through the denoiser.
//! - A scene with nothing glossy is unchanged (the pass is skipped), and rough pixels keep the
//!   forward colour when it runs.
//! - A held camera converges: frame-to-frame FLIP ≤ 0.01 (`specs/27` § 2.3 stability).

use regia_core::glam::Vec3;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset,
};

const W: u32 = 192;
const H: u32 = 108;
const EV: f32 = 13.0;
const EYE: [f32; 3] = [0.0, 0.9, 2.5];
const TARGET: [f32; 3] = [0.0, 0.0, -1.0];
const FOV: f32 = 0.9;
const SPHERE: [f32; 3] = [0.0, 0.7, -1.5];

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A floor slab (top at y = 0) and, with `object`, a sun-lit orange sphere above it.
fn scene(object: bool, floor_roughness: f32, floor_metallic: f32, rough_sphere: f32) -> LabScene {
    let (fv, fi) = regia_render::box_mesh([6.0, 0.05, 6.0]);
    let floor = LabMesh {
        vertices: regia_render::translate_mesh(&fv, [0.0, -0.05, 0.0]),
        indices: fi,
        material: 0,
        ..LabMesh::unskinned()
    };
    let mut meshes = vec![floor];
    if object {
        let (sv, si) = regia_render::uv_sphere(0.5, 32, 16);
        meshes.push(LabMesh {
            vertices: regia_render::translate_mesh(&sv, SPHERE),
            indices: si,
            material: 1,
            ..LabMesh::unskinned()
        });
    }
    let post = PostSettings {
        exposure_ev: EV,
        ..PostSettings::golden_deterministic()
    };
    LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: FOV,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: EV,
        ambient_lux: 400.0,
        double_sided: false,
        post: Some(post),
        // Low sun from behind the camera: the face of the sphere the floor mirrors is lit.
        directional: DirectionalLightUniform::new([0.1, -0.4, -0.9], [1.0, 0.97, 0.92], 30_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.9, 0.9, 0.9, 1.0],
                metallic: floor_metallic,
                roughness: floor_roughness,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.9, 0.35, 0.08, 1.0],
                metallic: 0.0,
                roughness: rough_sphere,
                ..LabMaterial::default()
            },
        ],
        meshes,
    }
}

/// Desktop High (`scale_permille` 670 is DO27's budget, 1000 the preset) without clouds and fog.
fn desktop_high(scale_permille: u16) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.render_scale_permille = scale_permille;
    q.volumetric_clouds = false;
    q.volumetric_fog = false;
    q
}

/// Renders `frames` frames with one renderer and returns the last frame's linear HDR.
fn render_hdr(
    device: &RenderDevice,
    scene: &LabScene,
    quality: FrameQuality,
    frames: u32,
) -> (u32, u32, Vec<[f32; 3]>) {
    render_hdr_with(device, scene, quality, frames, FrameRenderer::new())
}

/// [`render_hdr`] with a renderer the caller has configured.
fn render_hdr_with(
    device: &RenderDevice,
    scene: &LabScene,
    quality: FrameQuality,
    frames: u32,
    mut renderer: FrameRenderer,
) -> (u32, u32, Vec<[f32; 3]>) {
    let frame = scene.as_frame_scene_with_quality(quality);
    for i in 0..frames {
        if i + 1 == frames {
            renderer.request_hdr_capture();
        }
        let ldr = renderer.render_offscreen(device, &frame).expect("frame");
        if i + 1 == frames {
            dump(&ldr);
        }
    }
    let hdr = renderer
        .take_hdr_capture(device)
        .expect("readback")
        .expect("the post stack ran");
    (hdr.width, hdr.height, hdr.rgb)
}

/// `REGIA_SSSR_DUMP=<dir>` writes every captured frame there (`frame_NN.png`) for review by eye.
fn dump(ldr: &regia_render::ForwardResult) {
    let Some(dir) = regia_core::brand::env_var_os("REGIA_SSSR_DUMP") else {
        return;
    };
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("dump dir");
    let file = std::fs::File::create(dir.join(format!("frame_{n:02}.png"))).expect("dump file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), ldr.width, ldr.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut w| w.write_image_data(&ldr.pixels))
        .expect("dump png");
}

/// Where the sphere's mirror image (its centre reflected in y = 0) lands, in uv.
fn mirror_uv() -> [f32; 2] {
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        Vec3::from_array(EYE),
        Vec3::from_array(TARGET),
        Vec3::Y,
    );
    let proj =
        regia_core::glam::camera::rh::proj::directx::perspective(FOV, W as f32 / H as f32, 0.05, 100.0);
    let p = (proj * view).project_point3(Vec3::new(SPHERE[0], -SPHERE[1], SPHERE[2]));
    [p.x * 0.5 + 0.5, 0.5 - p.y * 0.5]
}

/// Mean linear RGB of a box of ±`r` (uv) around `uv`.
fn region(w: u32, h: u32, rgb: &[[f32; 3]], uv: [f32; 2], r: f32) -> [f32; 3] {
    let x0 = ((uv[0] - r) * w as f32).max(0.0) as u32;
    let x1 = ((uv[0] + r) * w as f32).min(w as f32 - 1.0) as u32;
    let y0 = ((uv[1] - r) * h as f32).max(0.0) as u32;
    let y1 = ((uv[1] + r) * h as f32).min(h as f32 - 1.0) as u32;
    let mut sum = [0.0f64; 3];
    let mut n = 0.0f64;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let c = rgb[(y * w + x) as usize];
            for k in 0..3 {
                sum[k] += f64::from(c[k]);
            }
            n += 1.0;
        }
    }
    sum.map(|s| (s / n.max(1.0)) as f32)
}

fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// A renderer whose probes light only the rough specular tail (`ProbeSpecular::RoughOnly`).
/// Desktop High's probe specular (ADR-1103) lets a mirror reflect the probes' light, here the
/// 400 lux ambient fill, in the lobe; the contrast below measures the screen-space reflection
/// against a floor that reflects nothing else (ADR-1384).
fn sssr_only_renderer() -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    let ddgi = regia_render::DdgiSettings {
        specular: regia_render::ProbeSpecular::RoughOnly,
        ..renderer.ddgi_settings().clone()
    };
    renderer.set_ddgi_settings(ddgi);
    renderer
}

/// Floor brightness where the sphere's reflection belongs, with and without the sphere.
fn mirror_contrast(
    device: &RenderDevice,
    quality: FrameQuality,
    roughness: f32,
    frames: u32,
) -> (f32, [f32; 3]) {
    let uv = mirror_uv();
    let (w, h, with) = render_hdr_with(
        device,
        &scene(true, roughness, 1.0, 0.6),
        quality,
        frames,
        sssr_only_renderer(),
    );
    let (_, _, without) = render_hdr_with(
        device,
        &scene(false, roughness, 1.0, 0.6),
        quality,
        frames,
        sssr_only_renderer(),
    );
    let a = region(w, h, &with, uv, 0.02);
    let b = region(w, h, &without, uv, 0.02);
    (luma(a) / luma(b).max(1e-6), a)
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn mirror_floor_under_a_lit_object_shows_the_object_on_desktop_high() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    for scale in [1000u16, 670] {
        let (ratio, color) = mirror_contrast(&device, desktop_high(scale), 0.0, 4);
        eprintln!("scale {scale}: mirror region {ratio:.2}× the empty floor, color {color:?}");
        assert!(
            ratio > 3.0,
            "scale {scale}: the sphere's reflection must light the mirror floor ({ratio:.2}×)"
        );
        assert!(
            color[0] > color[1] && color[1] > color[2],
            "scale {scale}: the reflection keeps the sphere's orange, got {color:?}"
        );
    }
}

#[test]
fn glossy_floor_shows_a_blurred_reflection() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (ratio, color) = mirror_contrast(&device, desktop_high(1000), 0.25, 12);
    eprintln!("roughness 0.25: reflection region {ratio:.2}× the empty floor, color {color:?}");
    assert!(
        ratio > 1.5,
        "a 0.25-roughness floor reflects the sphere ({ratio:.2}×)"
    );
    assert!(color[0] > color[2], "orange, got {color:?}");
}

#[test]
fn a_scene_with_nothing_glossy_is_unchanged() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = scene(true, 0.9, 0.0, 0.9);
    let on = desktop_high(1000);
    let mut off = on;
    off.ssr = false;
    let (_, _, a) = render_hdr(&device, &scene, on, 2);
    let (_, _, b) = render_hdr(&device, &scene, off, 2);
    let differ = a.iter().zip(&b).filter(|(x, y)| x != y).count();
    assert_eq!(differ, 0, "{differ} pixels changed on a rough-only frame");
}

/// With a glossy sphere in view the pass runs; the rough floor's pixels (the bottom quarter of
/// the frame, below the sphere) must still keep the forward colour exactly.
#[test]
fn rough_pixels_are_unchanged_when_the_pass_runs() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = scene(true, 0.9, 0.0, 0.3);
    let on = desktop_high(1000);
    let mut off = on;
    off.ssr = false;
    let (w, h, a) = render_hdr(&device, &scene, on, 2);
    let (_, _, b) = render_hdr(&device, &scene, off, 2);
    let rows = (h * 3 / 4) as usize..h as usize;
    let floor = |img: &[[f32; 3]]| img[rows.start * w as usize..rows.end * w as usize].to_vec();
    let differ = floor(&a)
        .iter()
        .zip(floor(&b).iter())
        .filter(|(x, y)| x != y)
        .count();
    assert_eq!(differ, 0, "{differ} rough floor pixels changed");
    let all = a.iter().zip(&b).filter(|(x, y)| x != y).count();
    assert!(all > 0, "the glossy sphere must pick up a reflection");
}

#[test]
fn a_held_camera_is_temporally_stable() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = scene(true, 0.2, 1.0, 0.6);
    let quality = desktop_high(1000);
    let mut renderer = FrameRenderer::new();
    let frame = scene.as_frame_scene_with_quality(quality);
    let mut last: Option<Vec<[f32; 3]>> = None;
    let mut worst = 0.0f32;
    for i in 0..32 {
        renderer.request_hdr_capture();
        renderer.render_offscreen(&device, &frame).expect("frame");
        let hdr = renderer
            .take_hdr_capture(&device)
            .expect("readback")
            .expect("hdr");
        // Held views freeze once the resolve has converged (`HELD_CONVERGE_FRAMES`, 32): the
        // window is the last running frames, where SSSR still traces with a new jitter each frame.
        if (25..32).contains(&i)
            && let Some(prev) = last.as_ref()
        {
            let (report, _) = regia_render::compare::compare_linear(
                prev,
                &hdr.rgb,
                hdr.width,
                hdr.height,
                EV,
                regia_render::compare::DEFAULT_PPD,
            );
            worst = worst.max(report.flip_mean);
        }
        last = Some(hdr.rgb);
    }
    eprintln!("held camera: worst frame-to-frame FLIP {worst:.4} over frames 24..31");
    assert!(worst <= 0.01, "frame-to-frame FLIP {worst}");
}

/// Cost of the reflection slot (`GpuTimings::ssr_ms`: SPD pyramid, classify, trace, denoise,
/// composite) at 2560×1440, on a moving camera so no held-view reuse applies. Worst cases: the
/// whole lower half a mirror (every pixel traced) and a 0.3-rough metal floor (quads plus the
/// variance-guided rays). `--ignored`, on the bench GPU.
#[test]
#[ignore = "timing; run on the bench GPU"]
fn reflection_cost_at_1440p() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    assert!(
        device.capabilities.timestamp_query,
        "timestamp queries are required"
    );
    for (label, roughness, scale) in [
        ("mirror floor, native", 0.0, 1000u16),
        ("mirror floor, 0.67", 0.0, 670),
        ("0.3-rough metal floor, native", 0.3, 1000),
        ("0.3-rough metal floor, 0.67", 0.3, 670),
    ] {
        let mut lab = scene(true, roughness, 1.0, 0.6);
        lab.width = 2560;
        lab.height = 1440;
        let mut renderer = FrameRenderer::new();
        renderer.set_gpu_timing(&device, true);
        let mut frame = lab.as_frame_scene_with_quality(desktop_high(scale));
        let (mut ms, mut trace, mut denoise) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..60 {
            frame.eye[0] = 0.002 * i as f32;
            renderer.render_offscreen(&device, &frame).expect("frame");
            if i >= 20 {
                let t = renderer.gpu_timings();
                ms.push(t.ssr_ms);
                trace.push(t.sssr_trace_ms);
                denoise.push(t.sssr_denoise_ms);
            }
        }
        let median = |v: &mut Vec<f32>| {
            v.sort_by(f32::total_cmp);
            v[v.len() / 2]
        };
        let (mut trace, mut denoise) = (trace, denoise);
        let (trace, denoise) = (median(&mut trace), median(&mut denoise));
        ms.sort_by(f32::total_cmp);
        eprintln!(
            "sssr 2560×1440 {label}: median {:.3} ms (trace {trace:.3}, denoise {denoise:.3}), p90 {:.3} ms",
            ms[ms.len() / 2],
            ms[ms.len() * 9 / 10]
        );
    }
}

/// Reflection memory at 2560×1440 (gpu-farm3: stochastic SSR held ≈ 400 MiB there; budget
/// 200 MiB): the bytes per pixel of the targets a glossy frame allocates, scaled to 1440p. The
/// pass's own targets must fit; the "ssr color" output it shares is reported alongside.
#[test]
fn reflection_targets_fit_the_1440p_budget() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = scene(true, 0.0, 1.0, 0.3);
    let mut renderer = FrameRenderer::new();
    let frame = lab.as_frame_scene_with_quality(desktop_high(1000));
    renderer.render_offscreen(&device, &frame).expect("frame");
    let total = renderer.reflection_target_bytes();
    // The shared "ssr color" output (Rgba16Float, one per pixel).
    let output = u64::from(W) * u64::from(H) * 8;
    assert!(
        total > output,
        "a glossy frame allocates the stochastic SSR targets ({total} B)"
    );
    let mib_at_1440p = |bytes: u64| bytes as f64 / f64::from(W * H) * 2560.0 * 1440.0 / (1024.0 * 1024.0);
    let own = mib_at_1440p(total - output);
    let all = mib_at_1440p(total);
    eprintln!(
        "reflection targets at 2560×1440: stochastic SSR {own:.1} MiB, with the ssr color output {all:.1} MiB"
    );
    assert!(own <= 200.0, "stochastic SSR targets {own:.1} MiB at 1440p");
}

/// A frame with nothing glossy allocates no reflection target, and a glossy view's targets are
/// freed once nothing glossy has been in view for the idle window (60 frames).
#[test]
fn reflection_targets_are_lazy_and_freed_when_idle() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (rough_lab, glossy_lab) = (scene(true, 0.9, 0.0, 0.9), scene(true, 0.0, 1.0, 0.3));
    let rough = rough_lab.as_frame_scene_with_quality(desktop_high(1000));
    let glossy = glossy_lab.as_frame_scene_with_quality(desktop_high(1000));
    let mut renderer = FrameRenderer::new();
    renderer.render_offscreen(&device, &rough).expect("frame");
    assert_eq!(
        renderer.reflection_target_bytes(),
        0,
        "nothing glossy: no reflection targets"
    );
    renderer.render_offscreen(&device, &glossy).expect("frame");
    assert!(
        renderer.reflection_target_bytes() > 0,
        "a glossy frame allocates them"
    );
    for _ in 0..59 {
        renderer.render_offscreen(&device, &rough).expect("frame");
    }
    assert!(renderer.reflection_target_bytes() > 0, "kept through a short gap");
    renderer.render_offscreen(&device, &rough).expect("frame");
    assert_eq!(
        renderer.reflection_target_bytes(),
        0,
        "freed after 60 idle frames"
    );
    renderer.render_offscreen(&device, &glossy).expect("frame");
    assert!(
        renderer.reflection_target_bytes() > 0,
        "allocated again on demand"
    );
}

/// A shared draw (the terrain) keeps a frame out of stochastic SSR unless it opts in
/// (`SharedPassDraw::screen_space_reflective`, a wet or icy material).
#[test]
fn a_shared_draw_runs_the_pass_only_when_it_opts_in() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    struct Ground {
        reflective: bool,
        codes: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    impl regia_render::SharedPassDraw for Ground {
        fn prepare(
            &mut self,
            info: &regia_render::SharedFrameInfo,
            _upload: &mut regia_render::SharedUpload<'_>,
        ) {
            self.codes
                .store(info.sssr_codes, std::sync::atomic::Ordering::Relaxed);
        }
        fn draw<'a>(&'a self, _pass: &mut wgpu::RenderPass<'a>, _kind: regia_render::SharedPass) {}
        fn screen_space_reflective(&self) -> bool {
            self.reflective
        }
    }
    let device = device();
    let rough_lab = scene(true, 0.9, 0.0, 0.9);
    let rough = rough_lab.as_frame_scene_with_quality(desktop_high(1000));
    for reflective in [false, true] {
        let codes = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(!reflective));
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(Box::new(Ground {
            reflective,
            codes: codes.clone(),
        })));
        renderer.render_offscreen(&device, &rough).expect("frame");
        assert_eq!(
            codes.load(std::sync::atomic::Ordering::Relaxed),
            reflective,
            "sssr_codes follows the opt-in"
        );
        assert_eq!(
            renderer.reflection_target_bytes() > 0,
            reflective,
            "reflective = {reflective}: the pass runs only for an opted-in draw"
        );
    }
}

/// The mirror-ball scene of the dormancy tests: a rough floor in view and a glossy ball behind
/// the camera (`hidden`), and the same view with a mirror floor (`glossy`).
fn hidden_and_glossy() -> (LabScene, LabScene) {
    let mut hidden = scene(true, 0.9, 0.0, 0.9);
    let (bv, bi) = regia_render::uv_sphere(0.3, 16, 8);
    hidden.meshes.push(LabMesh {
        vertices: regia_render::translate_mesh(&bv, [0.0, 1.0, 12.0]),
        indices: bi,
        material: 2,
        ..LabMesh::unskinned()
    });
    hidden.materials.push(LabMaterial {
        base_color: [0.9, 0.9, 0.9, 1.0],
        metallic: 1.0,
        roughness: 0.05,
        ..LabMaterial::default()
    });
    let mut glossy = hidden.clone();
    glossy.materials[0].roughness = 0.0;
    glossy.materials[0].metallic = 1.0;
    (hidden, glossy)
}

/// Frame-path creates of the reflection passes in `events` (stochastic SSR, its depth pyramid,
/// the shared "ssr color" output).
fn reflection_creates(events: &[regia_render::FramePathEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.label.starts_with("sssr") || e.label.starts_with("spd") || e.label == "ssr color")
        .map(|e| format!("{:?} {:?}", e.kind, e.label))
        .collect()
}

/// A glossy material the CPU check cannot rule out but nothing glossy on screen (a mirror ball
/// behind the camera, as DO27's ORM-roughness props on a landscape map): the pass goes dormant
/// after its readbacks see no glossy tile and runs only the probe; a glossy floor coming into
/// view wakes it within a few frames. Sleeping and waking create nothing: the targets and their
/// bind groups are kept (ADR-168, `specs/27` § 2.4 zero creates after warm-up).
#[test]
fn the_pass_sleeps_without_glossy_pixels_and_wakes_for_them() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (hidden, glossy) = hidden_and_glossy();
    let hidden_frame = hidden.as_frame_scene_with_quality(desktop_high(1000));
    let glossy_frame = glossy.as_frame_scene_with_quality(desktop_high(1000));
    let mut renderer = FrameRenderer::new();
    // Frames until the pass is dormant (`Some(frames)`), or `None` after 90.
    let sleep = |renderer: &mut FrameRenderer| {
        (1..=90).find(|_| {
            renderer.render_offscreen(&device, &hidden_frame).expect("frame");
            renderer.sssr_dormant()
        })
    };
    // Frames until the pass is awake again, or `None` after 12.
    let wake = |renderer: &mut FrameRenderer| {
        (0..12).find(|_| {
            renderer.render_offscreen(&device, &glossy_frame).expect("frame");
            !renderer.sssr_dormant()
        })
    };
    // Both views awake first, so every bind group the views need exists.
    for _ in 0..3 {
        renderer.render_offscreen(&device, &glossy_frame).expect("frame");
    }
    let awake_bytes = renderer.reflection_target_bytes();
    assert!(awake_bytes > 0, "a glossy frame allocates the targets");
    // From here on, two sleep/wake cycles (the DO27 chase has one every ~20 s) create nothing,
    // not even the probe's bind group on the first dormant frame (built while awake).
    regia_render::trace_frame_path(true);
    let slept_at = sleep(&mut renderer).expect("the pass went dormant");
    eprintln!("dormant after {slept_at} frames");
    assert!(slept_at >= 30, "not before 30 empty readbacks ({slept_at})");
    assert_eq!(
        renderer.reflection_target_bytes(),
        awake_bytes,
        "dormant keeps its targets (ADR-168)"
    );
    wake(&mut renderer).expect("a glossy floor in view wakes the pass");
    sleep(&mut renderer).expect("dormant again");
    for _ in 0..5 {
        renderer.render_offscreen(&device, &hidden_frame).expect("frame");
    }
    assert!(
        renderer.sssr_dormant(),
        "stays dormant while nothing glossy is in view"
    );
    let woke_at = wake(&mut renderer);
    // Two more awake frames: the wake's clear and the first full passes.
    for _ in 0..2 {
        renderer.render_offscreen(&device, &glossy_frame).expect("frame");
    }
    let creates = reflection_creates(&regia_render::take_frame_path_events());
    regia_render::trace_frame_path(false);
    assert!(
        creates.is_empty(),
        "a sleep/wake cycle created reflection resources: {creates:?}"
    );
    let woke_at = woke_at.expect("a glossy floor in view wakes the pass");
    eprintln!("awake after {woke_at} glossy frames");
    // Awake, the mirror floor shows the object again.
    let (_, _, on) = render_hdr(&device, &glossy, desktop_high(1000), 2);
    let mut off_q = desktop_high(1000);
    off_q.ssr = false;
    let (_, _, off) = render_hdr(&device, &glossy, off_q, 2);
    assert!(on.iter().zip(&off).filter(|(a, b)| a != b).count() > 0);
}

/// A wake keeps the targets and clears them; the frames after it must equal those of the old
/// path that freed the targets while dormant and allocated fresh (zeroed) ones on the wake
/// (`FrameRenderer::trim_memory` still takes that path under memory pressure). Bit-identical
/// HDR, frame by frame.
#[test]
fn a_wake_on_kept_targets_matches_fresh_targets() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (hidden, glossy) = hidden_and_glossy();
    let hidden_frame = hidden.as_frame_scene_with_quality(desktop_high(1000));
    let glossy_frame = glossy.as_frame_scene_with_quality(desktop_high(1000));
    let (mut kept, mut fresh) = (FrameRenderer::new(), FrameRenderer::new());
    // A few glossy frames first, so the kept targets hold a real history to clear.
    for renderer in [&mut kept, &mut fresh] {
        for _ in 0..4 {
            renderer.render_offscreen(&device, &glossy_frame).expect("frame");
        }
    }
    let mut asleep = false;
    for _ in 0..90 {
        for renderer in [&mut kept, &mut fresh] {
            renderer.render_offscreen(&device, &hidden_frame).expect("frame");
        }
        if kept.sssr_dormant() && fresh.sssr_dormant() {
            asleep = true;
            break;
        }
    }
    assert!(asleep, "both renderers went dormant");
    assert_eq!(
        kept.reflection_target_bytes(),
        fresh.reflection_target_bytes(),
        "both kept their targets through the dormant spell"
    );
    let freed = fresh.trim_memory();
    assert!(freed > 0, "memory pressure frees a dormant pass's targets");
    assert_eq!(fresh.reflection_target_bytes(), 0);
    assert!(kept.reflection_target_bytes() > 0);
    let mut differing = Vec::new();
    for frame in 0..10 {
        let mut hdr = Vec::new();
        for renderer in [&mut kept, &mut fresh] {
            renderer.request_hdr_capture();
            renderer.render_offscreen(&device, &glossy_frame).expect("frame");
            let capture = renderer
                .take_hdr_capture(&device)
                .expect("readback")
                .expect("the post stack ran");
            hdr.push((capture.rgb, renderer.sssr_dormant()));
        }
        assert_eq!(hdr[0].1, hdr[1].1, "frame {frame}: both wake on the same frame");
        let diff = hdr[0].0.iter().zip(&hdr[1].0).filter(|(a, b)| a != b).count();
        if diff > 0 {
            differing.push((frame, diff));
        }
    }
    assert!(!kept.sssr_dormant(), "the glossy floor woke the pass");
    assert!(
        differing.is_empty(),
        "kept vs fresh targets after the wake, (frame, pixels that differ): {differing:?}"
    );
}

/// Tiers without stochastic SSR (Desktop Low, the Deck, mobile) allocate none of its targets,
/// even with a mirror floor in view.
#[test]
fn tiers_without_the_pass_allocate_nothing() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = scene(true, 0.0, 1.0, 0.3);
    for preset in [
        RenderProfilePreset::DesktopLow,
        RenderProfilePreset::Deck,
        RenderProfilePreset::MobileHigh,
        RenderProfilePreset::MobileLow,
    ] {
        let mut quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
        quality.volumetric_clouds = false;
        quality.volumetric_fog = false;
        let frame = lab.as_frame_scene_with_quality(quality);
        let mut renderer = FrameRenderer::new();
        regia_render::trace_frame_path(true);
        for _ in 0..3 {
            renderer.render_offscreen(&device, &frame).expect("frame");
        }
        let creates: Vec<String> = regia_render::take_frame_path_events()
            .iter()
            .filter(|e| e.label.starts_with("sssr"))
            .map(|e| e.label.clone())
            .collect();
        regia_render::trace_frame_path(false);
        assert_eq!(
            renderer.sssr_target_bytes(),
            0,
            "{preset:?}: no stochastic SSR targets"
        );
        assert!(creates.is_empty(), "{preset:?} created {creates:?}");
    }
}
